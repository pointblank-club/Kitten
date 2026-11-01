//! ext4 through e2fsprogs.

use std::ops::Range;
use std::path::Path;

use crate::parse::{blocks_to_bytes, complement, field, number_field, unexpected};
use crate::run::{CommandLine, RunError, Runner};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ext4Summary {
    pub block_size: u64,
    pub block_count: u64,
    pub free_blocks: u64,
    pub clean: bool,
    /// The journal holds changes not yet written to the filesystem. Normal while mounted.
    pub needs_recovery: bool,
    pub error_count: u64,
}

impl Ext4Summary {
    /// Saturates rather than wrapping on overflow: a filesystem claiming more bytes than
    /// a u64 can hold will be larger than any real device and get refused downstream.
    pub fn len_bytes(&self) -> u64 {
        self.block_count.saturating_mul(self.block_size)
    }
}

/// Reads the superblock summary. Safe while the filesystem is mounted.
pub fn summary(runner: &dyn Runner, device: &Path) -> Result<Ext4Summary, RunError> {
    let command = CommandLine::new("tune2fs").arg("-l").path(device);
    let output = runner.run_ok(&command)?;
    parse_summary(&output.text()).map_err(|detail| unexpected(&command, detail))
}

/// A full check that repairs what it safely can. Only valid on an unmounted filesystem.
pub fn check(runner: &dyn Runner, device: &Path) -> Result<(), RunError> {
    let command = CommandLine::new("e2fsck").arg("-f").arg("-p").path(device);
    runner.run_expecting(&command, &[0, 1]).map(|_| ())
}

/// The smallest size the filesystem can shrink to, in bytes.
pub fn min_size(runner: &dyn Runner, device: &Path, block_size: u64) -> Result<u64, RunError> {
    let command = CommandLine::new("resize2fs").arg("-P").path(device);
    let output = runner.run_ok(&command)?;
    number_field(&output.text(), "Estimated minimum size of the filesystem")
        .and_then(|blocks| blocks_to_bytes(blocks, block_size))
        .map_err(|detail| unexpected(&command, detail))
}

/// Byte ranges holding data, relative to the start of the filesystem. Run on an unmounted,
/// checked filesystem: the per-group free counts must add up to the superblock's.
pub fn used_ranges(runner: &dyn Runner, device: &Path) -> Result<Vec<Range<u64>>, RunError> {
    let command = CommandLine::new("dumpe2fs").path(device);
    let output = runner.run_ok(&command)?;
    parse_used_ranges(&output.text()).map_err(|detail| unexpected(&command, detail))
}

/// Resizes an unmounted filesystem to `size` bytes. Never call this on a real partition
/// outside a guarded run: `resize2fs` can't survive a power cut on its own.
pub fn resize_offline(runner: &dyn Runner, device: &Path, size: u64) -> Result<(), RunError> {
    let command = CommandLine::new("resize2fs")
        .path(device)
        .arg(format!("{}K", size / 1024));
    runner.run_ok(&command).map(|_| ())
}

/// Grows a mounted filesystem to fill its partition. The kernel journals an online grow.
pub fn grow_online(runner: &dyn Runner, device: &Path) -> Result<(), RunError> {
    runner
        .run_ok(&CommandLine::new("resize2fs").path(device))
        .map(|_| ())
}

pub fn parse_summary(text: &str) -> Result<Ext4Summary, String> {
    let state = field(text, "Filesystem state")?.ok_or("missing \"Filesystem state\"")?;
    let features = field(text, "Filesystem features")?.unwrap_or_default();
    let error_count = match field(text, "FS Error count")? {
        Some(_) => number_field(text, "FS Error count")?,
        None => 0,
    };
    let block_size = number_field(text, "Block size")?;
    if block_size == 0 {
        // A zero block size would collapse every used byte-range to 0..0 (blocks_to_bytes with a
        // zero multiplier), so the mover would skip live data. Refuse it like NTFS already does.
        return Err("block size is zero".to_string());
    }
    let block_count = number_field(text, "Block count")?;
    let free_blocks = number_field(text, "Free blocks")?;
    if free_blocks > block_count {
        // An impossible superblock; without this, used = block_count - free_blocks underflows
        // (aborting under overflow-checks) in callers such as the inspect probe.
        return Err(format!(
            "{free_blocks} free blocks exceeds the {block_count}-block filesystem"
        ));
    }
    Ok(Ext4Summary {
        block_size,
        block_count,
        free_blocks,
        clean: state == "clean",
        needs_recovery: features.split_whitespace().any(|f| f == "needs_recovery"),
        error_count,
    })
}

pub fn parse_used_ranges(dumpe2fs: &str) -> Result<Vec<Range<u64>>, String> {
    let summary = parse_summary(dumpe2fs)?;
    let mut free = Vec::new();
    for line in dumpe2fs.lines() {
        let Some(list) = line.strip_prefix("  Free blocks:") else {
            continue;
        };
        for item in list
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            free.push(parse_block_range(item)?);
        }
    }

    // Reject any free range that runs past the filesystem. `complement` clamps out-of-range ranges,
    // which would relocate coverage and could drop a real used block from the result (skipped data);
    // mirror the xfs bound and refuse instead. (parse_block_range already guarantees start < end.)
    for r in &free {
        if r.end > summary.block_count {
            return Err(format!(
                "a free block range ends at {} past the {}-block filesystem",
                r.end, summary.block_count
            ));
        }
    }
    // Checked sum: the ranges come from untrusted dumpe2fs output, so a crafted list must produce a
    // clean error rather than overflow and abort before the consistency comparison below.
    let free_count = free.iter().try_fold(0u64, |acc, r| {
        acc.checked_add(r.end - r.start)
            .ok_or_else(|| "free block counts overflow a 64-bit total".to_string())
    })?;
    if free_count != summary.free_blocks {
        return Err(format!(
            "groups list {free_count} free blocks but the superblock says {}",
            summary.free_blocks
        ));
    }
    complement(free, summary.block_count)
        .into_iter()
        .map(|r| {
            Ok(blocks_to_bytes(r.start, summary.block_size)?
                ..blocks_to_bytes(r.end, summary.block_size)?)
        })
        .collect()
}

/// Parses `12` or `12-40` (inclusive) into a half-open block range. The values come from
/// untrusted `dumpe2fs` output, so a reversed range or a block number at `u64::MAX` must be a
/// clean error, not a reversed span or (under release `overflow-checks`) a process abort.
fn parse_block_range(item: &str) -> Result<Range<u64>, String> {
    let parse = |s: &str| {
        s.parse::<u64>()
            .map_err(|_| format!("bad block number in \"{item}\""))
    };
    let end_after = |last: u64| {
        last.checked_add(1)
            .ok_or_else(|| format!("block range ends at u64::MAX in \"{item}\""))
    };
    match item.split_once('-') {
        Some((first, last)) => {
            let (first, last) = (parse(first)?, parse(last)?);
            if first > last {
                return Err(format!("reversed block range \"{item}\""));
            }
            Ok(first..end_after(last)?)
        }
        None => {
            let block = parse(item)?;
            Ok(block..end_after(block)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMPE2FS: &str = include_str!("../tests/fixtures/ext4/dumpe2fs.txt");
    const TUNE2FS: &str = include_str!("../tests/fixtures/ext4/tune2fs-l.txt");

    #[test]
    fn an_injected_block_size_line_is_refused() {
        // A volume label that embedded a newline injects a fake "Block size:512" ahead of the real
        // one. Taking the first would make used_ranges ~8x too small and silently skip live data, so
        // the duplicate key must be refused.
        let text = "Filesystem volume name: x\nBlock size:512\n\
                    Filesystem state: clean\nBlock size:              4096\n\
                    Block count:              102400\nFree blocks:              87129\n";
        assert!(parse_summary(text).is_err());
    }

    #[test]
    fn free_blocks_exceeding_block_count_is_refused() {
        let text = "Filesystem state: clean\nBlock size: 4096\n\
                    Block count: 100\nFree blocks: 200\n";
        assert!(parse_summary(text).is_err());
    }

    #[test]
    fn parses_the_superblock_summary() {
        let summary = parse_summary(TUNE2FS).unwrap();
        assert_eq!(
            summary,
            Ext4Summary {
                block_size: 4096,
                block_count: 102_400,
                free_blocks: 87_129,
                clean: true,
                needs_recovery: false,
                error_count: 0,
            }
        );
    }

    #[test]
    fn used_ranges_are_the_complement_of_the_free_lists() {
        let used = parse_used_ranges(DUMPE2FS).unwrap();
        let block = 4096;
        assert_eq!(
            used,
            vec![
                0..11_073 * block,
                32_768 * block..32_819 * block,
                65_536 * block..69_632 * block,
                98_304 * block..98_355 * block,
            ]
        );
    }

    #[test]
    fn refuses_free_lists_that_disagree_with_the_superblock() {
        let tampered = DUMPE2FS.replace("Free blocks: 69632-98303", "Free blocks: 69632-98300");
        assert!(
            parse_used_ranges(&tampered)
                .unwrap_err()
                .contains("superblock")
        );
    }

    #[test]
    fn reads_single_blocks_and_ranges() {
        assert_eq!(parse_block_range("7").unwrap(), 7..8);
        assert_eq!(parse_block_range("7-9").unwrap(), 7..10);
        assert!(parse_block_range("7-x").is_err());
    }

    #[test]
    fn reports_errors_and_dirty_state() {
        let text = TUNE2FS
            .replace(
                "Filesystem state:         clean",
                "Filesystem state:         not clean",
            )
            .replace("metadata_csum", "metadata_csum needs_recovery")
            + "FS Error count:           3\n";
        let summary = parse_summary(&text).unwrap();
        assert!(!summary.clean);
        assert!(summary.needs_recovery);
        assert_eq!(summary.error_count, 3);
    }
}
