//! Btrfs through btrfs-progs. Resizing is done online by the kernel, which relocates data
//! transactionally, so it survives a power cut without a guarded run.

use std::ops::Range;
use std::path::Path;

use crate::parse::unexpected;
use crate::run::{CommandLine, RunError, Runner};

/// Resizes the mounted filesystem at `mountpoint` to `size` bytes, or to fill its partition.
pub fn resize(runner: &dyn Runner, mountpoint: &Path, size: Option<u64>) -> Result<(), RunError> {
    let size = size.map_or_else(|| "max".to_string(), |bytes| bytes.to_string());
    let command = CommandLine::new("btrfs")
        .arg("filesystem")
        .arg("resize")
        .arg(size)
        .path(mountpoint);
    runner.run_ok(&command).map(|_| ())
}

/// A read-only check of an unmounted filesystem.
pub fn check(runner: &dyn Runner, device: &Path) -> Result<(), RunError> {
    let command = CommandLine::new("btrfs")
        .arg("check")
        .arg("--readonly")
        .path(device);
    runner.run_ok(&command).map(|_| ())
}

/// The byte ranges Btrfs has allocated on this device, from the dev-extent tree. Everything
/// outside them holds no data. Returns `None` when the filesystem spans more than one device,
/// where only a full copy is safe.
pub fn used_ranges(
    runner: &dyn Runner,
    device: &Path,
) -> Result<Option<Vec<Range<u64>>>, RunError> {
    let command = CommandLine::new("btrfs")
        .arg("inspect-internal")
        .arg("dump-tree")
        .arg("-t")
        .arg("dev")
        .path(device);
    let output = runner.run_ok(&command)?;
    parse_dev_tree(&output.text()).map_err(|detail| unexpected(&command, detail))
}

/// Pairs each `item N key (<devid> DEV_EXTENT <offset>)` line with the `length <n>` on the
/// extent's detail line.
fn parse_dev_tree(text: &str) -> Result<Option<Vec<Range<u64>>>, String> {
    let mut ranges = Vec::new();
    let mut offset: Option<u64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.split("key (").nth(1) {
            let mut words = rest.split_whitespace();
            let devid = words.next().unwrap_or_default();
            if words.next() == Some("DEV_EXTENT") {
                if devid != "1" {
                    return Ok(None);
                }
                // A new extent key before the previous one's length line means we would otherwise
                // overwrite `offset` and silently drop that allocated extent (the mover would then
                // skip live data). Treat it as malformed output instead.
                if offset.is_some() {
                    return Err(format!("dev extent key without a preceding length: {line}"));
                }
                let text = words.next().unwrap_or_default().trim_end_matches(')');
                offset = Some(
                    text.parse()
                        .map_err(|_| format!("bad extent offset: {line}"))?,
                );
            }
        } else if let (Some(start), Some(rest)) = (offset, line.split(" length ").nth(1)) {
            let len: u64 = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .parse()
                .map_err(|_| format!("bad extent length: {line}"))?;
            // Values come from untrusted tool output; refuse an overflowing extent rather than
            // wrapping or (under release overflow-checks) aborting, as xfs already does.
            let end = start
                .checked_add(len)
                .ok_or_else(|| format!("dev extent overflows a 64-bit size: {line}"))?;
            ranges.push(start..end);
            offset = None;
        }
    }
    if ranges.is_empty() {
        return Err("no dev extents found".to_string());
    }
    Ok(Some(ranges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ScriptedRunner;

    const DEV_TREE: &str = include_str!("../tests/fixtures/btrfs/dev-tree.txt");

    #[test]
    fn reads_used_ranges_from_a_real_dev_tree() {
        let runner = ScriptedRunner::new().reply(0, DEV_TREE);
        let ranges = used_ranges(&runner, Path::new("/dev/sda9"))
            .unwrap()
            .unwrap();
        assert_eq!(
            runner.commands(),
            ["btrfs inspect-internal dump-tree -t dev /dev/sda9"]
        );
        assert_eq!(ranges.len(), 7);
        assert_eq!(ranges[0], 1_048_576..1_048_576 + 8_388_608);
        assert_eq!(ranges[4], 38_797_312..38_797_312 + 33_554_432);
        assert_eq!(ranges[6], 105_906_176..105_906_176 + 8_388_608);
    }

    #[test]
    fn a_second_device_means_full_copy() {
        let two_devices =
            DEV_TREE.replace("key (1 DEV_EXTENT 13631488)", "key (2 DEV_EXTENT 13631488)");
        let runner = ScriptedRunner::new().reply(0, &two_devices);
        assert_eq!(used_ranges(&runner, Path::new("/dev/sda9")).unwrap(), None);
    }

    #[test]
    fn a_dump_without_extents_is_refused() {
        let runner = ScriptedRunner::new().reply(0, "device tree key (DEV_TREE ROOT_ITEM 0)\n");
        assert!(used_ranges(&runner, Path::new("/dev/sda9")).is_err());
    }

    #[test]
    fn grows_to_max_and_shrinks_to_a_size() {
        let runner = ScriptedRunner::new().reply(0, "").reply(0, "");
        resize(&runner, Path::new("/mnt"), None).unwrap();
        resize(&runner, Path::new("/mnt"), Some(1 << 30)).unwrap();
        assert_eq!(
            runner.commands(),
            [
                "btrfs filesystem resize max /mnt",
                "btrfs filesystem resize 1073741824 /mnt"
            ]
        );
    }
}
