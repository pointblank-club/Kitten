//! XFS through xfsprogs. XFS can grow (online, journaled by the kernel) but never shrink.

use std::ops::Range;
use std::path::Path;

use crate::parse::{blocks_to_bytes, unexpected};
use crate::run::{CommandLine, RunError, Runner};

/// Grows the mounted filesystem at `mountpoint` to fill its partition.
pub fn grow(runner: &dyn Runner, mountpoint: &Path) -> Result<(), RunError> {
    runner
        .run_ok(&CommandLine::new("xfs_growfs").path(mountpoint))
        .map(|_| ())
}

/// A check that changes nothing, on an unmounted filesystem.
pub fn check(runner: &dyn Runner, device: &Path) -> Result<(), RunError> {
    runner
        .run_ok(&CommandLine::new("xfs_repair").arg("-n").path(device))
        .map(|_| ())
}

/// The byte ranges that hold data or metadata: everything that `xfs_db freesp` doesn't list
/// as free. Returns `None` for layouts a complement can't describe safely (a realtime device,
/// or an external log).
pub fn used_ranges(
    runner: &dyn Runner,
    device: &Path,
) -> Result<Option<Vec<Range<u64>>>, RunError> {
    let sb_command = CommandLine::new("xfs_db")
        .arg("-r")
        .arg("-c")
        .arg("sb 0")
        .arg("-c")
        .arg("p dblocks blocksize agblocks logstart rbmblocks")
        .path(device);
    let sb = runner.run_ok(&sb_command)?;
    let geometry = Geometry::parse(&sb.text()).map_err(|detail| unexpected(&sb_command, detail))?;
    if geometry.rbmblocks != 0 || geometry.logstart == 0 {
        return Ok(None);
    }

    let free_command = CommandLine::new("xfs_db")
        .arg("-r")
        .arg("-c")
        .arg("freesp -d")
        .path(device);
    let free = runner.run_ok(&free_command)?;
    parse_freesp(&free.text(), &geometry).map_err(|detail| unexpected(&free_command, detail))
}

struct Geometry {
    dblocks: u64,
    blocksize: u64,
    agblocks: u64,
    logstart: u64,
    rbmblocks: u64,
}

impl Geometry {
    fn parse(text: &str) -> Result<Self, String> {
        let field = |name: &str| -> Result<u64, String> {
            text.lines()
                .find_map(|line| {
                    let (key, value) = line.split_once('=')?;
                    (key.trim() == name).then(|| value.trim().parse().ok())?
                })
                .ok_or_else(|| format!("no {name} in the superblock output"))
        };
        let blocksize = field("blocksize")?;
        if blocksize == 0 {
            // A zero block size collapses every used range to 0..0, so the mover would skip live
            // data. Refuse it rather than trusting a corrupt superblock.
            return Err("blocksize is zero".to_string());
        }
        Ok(Self {
            dblocks: field("dblocks")?,
            blocksize,
            agblocks: field("agblocks")?,
            logstart: field("logstart")?,
            rbmblocks: field("rbmblocks")?,
        })
    }
}

/// Free extents are "agno agbno len" rows; the trailing histogram has five columns and is
/// skipped. Used space is the complement over the whole data area.
fn parse_freesp(text: &str, geometry: &Geometry) -> Result<Option<Vec<Range<u64>>>, String> {
    let mut free: Vec<Range<u64>> = Vec::new();
    for line in text.lines() {
        let columns: Vec<u64> = line
            .split_whitespace()
            .map_while(|w| w.parse().ok())
            .collect();
        if columns.len() != 3 || line.split_whitespace().count() != 3 {
            continue;
        }
        let (agno, agbno, len) = (columns[0], columns[1], columns[2]);
        // All of these come from tool output describing an untrusted disk, so every
        // multiply and add is checked: a corrupt or hostile superblock refuses cleanly
        // instead of wrapping (or, under release overflow-checks, aborting the helper).
        let start_block = agno
            .checked_mul(geometry.agblocks)
            .and_then(|v| v.checked_add(agbno))
            .ok_or_else(|| format!("a free extent's start block overflows: {line}"))?;
        let start = blocks_to_bytes(start_block, geometry.blocksize)?;
        let end = blocks_to_bytes(len, geometry.blocksize)?
            .checked_add(start)
            .ok_or_else(|| format!("a free extent's end overflows: {line}"))?;
        if end > blocks_to_bytes(geometry.dblocks, geometry.blocksize)? {
            return Err(format!("a free extent ends past the filesystem: {line}"));
        }
        free.push(start..end);
    }
    free.sort_by_key(|range| range.start);
    let mut used = Vec::new();
    let mut cursor = 0;
    for range in free {
        if range.start < cursor {
            return Err("free extents overlap".to_string());
        }
        if range.start > cursor {
            used.push(cursor..range.start);
        }
        cursor = range.end;
    }
    let total = blocks_to_bytes(geometry.dblocks, geometry.blocksize)?;
    if cursor < total {
        used.push(cursor..total);
    }
    Ok(Some(used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ScriptedRunner;

    const SB: &str = include_str!("../tests/fixtures/xfs/sb.txt");
    const FREESP: &str = include_str!("../tests/fixtures/xfs/freesp.txt");

    #[test]
    fn used_is_the_complement_of_the_free_extents() {
        let runner = ScriptedRunner::new().reply(0, SB).reply(0, FREESP);
        let used = used_ranges(&runner, Path::new("/dev/sda8"))
            .unwrap()
            .unwrap();

        // The first AG: blocks 0..6 are metadata, 6..16 and 24.. are free with 16..24 used.
        assert_eq!(used[0], 0..6 * 4096);
        assert_eq!(used[1], 16 * 4096..24 * 4096);
        // The internal log sits in AG 2 and is used space.
        let log_byte = 2 * 32768 * 4096 + 42 * 4096;
        assert!(used.iter().any(|r| r.contains(&log_byte)));
        // Free space never appears as used.
        let total: u64 = used.iter().map(|r| r.end - r.start).sum();
        assert_eq!(total, (131_072 - 129_672) * 4096);
    }

    #[test]
    fn a_realtime_or_external_log_filesystem_is_a_full_copy() {
        let realtime = SB.replace("rbmblocks = 0", "rbmblocks = 5");
        let runner = ScriptedRunner::new().reply(0, &realtime);
        assert_eq!(used_ranges(&runner, Path::new("/dev/sda8")).unwrap(), None);

        let external_log = SB.replace("logstart = 65542", "logstart = 0");
        let runner = ScriptedRunner::new().reply(0, &external_log);
        assert_eq!(used_ranges(&runner, Path::new("/dev/sda8")).unwrap(), None);
    }
}
