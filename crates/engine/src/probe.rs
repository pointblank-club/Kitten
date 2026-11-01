//! Turning what the normal system can see (the GPT, `lsblk`, the filesystem tools) into the
//! layout the planner works on.

use bootshift_core::layout::{Content, Extent, Geometry, Layout, Partition};
use bootshift_core::units::Sectors;
use bootshift_disk::GptEntry;
use bootshift_fs::ntfs::DirtyMark;
use bootshift_fs::probe::{DiskInfo, content_of};
use bootshift_fs::run::Runner;
use bootshift_fs::{ext4, ntfs};

const MIB: u64 = 1 << 20;

/// The layout of `disk`. Measurements that fail leave a partition counted as full and with no
/// known minimum size, so the planner and preflight refuse to shrink it rather than guess.
pub fn layout(
    runner: &dyn Runner,
    disk: &DiskInfo,
    usable: Extent,
    entries: &[GptEntry],
) -> Layout {
    let sector_size = disk.logical_sector_size;
    let partitions = entries
        .iter()
        .map(|entry| {
            let info = disk.partition(entry.number);
            let content = content_of(
                info.and_then(|p| p.fstype.as_deref()),
                Some(entry.type_guid),
            );
            let (used, min_len) = match info {
                Some(info) => measure(runner, &info.path, content, entry.extent.len, sector_size),
                None => (entry.extent.len, None),
            };
            Partition {
                number: entry.number,
                unique_guid: entry.unique_guid,
                type_guid: entry.type_guid,
                name: entry.name.clone(),
                attributes: entry.attributes,
                extent: entry.extent,
                content,
                used,
                min_len,
            }
        })
        .collect();
    Layout {
        geometry: Geometry {
            sector_size,
            usable,
            alignment: Sectors(MIB / u64::from(sector_size)),
        },
        partitions,
    }
}

fn measure(
    runner: &dyn Runner,
    device: &std::path::Path,
    content: Content,
    len: Sectors,
    sector_size: u32,
) -> (Sectors, Option<Sectors>) {
    let to_sectors = |bytes: u64| Sectors(bytes.div_ceil(u64::from(sector_size)));
    match content {
        Content::Ext4 => {
            let Ok(summary) = ext4::summary(runner, device) else {
                return (len, None);
            };
            // Checked: block_count/free_blocks/block_size come from an untrusted superblock reached
            // by the password-less inspect. parse_summary already rejects free_blocks > block_count,
            // but guard the multiply too so a huge block_count cannot overflow and abort the daemon;
            // fall back to "whole device used, no min" on anything implausible.
            let Some(used_bytes) = summary
                .block_count
                .checked_sub(summary.free_blocks)
                .and_then(|used_blocks| used_blocks.checked_mul(summary.block_size))
            else {
                return (len, None);
            };
            let used = to_sectors(used_bytes);
            let min = ext4::min_size(runner, device, summary.block_size)
                .ok()
                .map(to_sectors);
            (used, min)
        }
        Content::Ntfs => match ntfs::info(runner, device, DirtyMark::Refuse) {
            Ok(info) => {
                let min = info.min_size.map(to_sectors);
                (min.unwrap_or(len), min)
            }
            Err(_) => (len, None),
        },
        Content::Swap => (Sectors(0), Some(Sectors(0))),
        _ => (len, None),
    }
}

#[cfg(test)]
mod tests {
    use bootshift_core::layout::Guid;
    use bootshift_core::units::Lba;
    use bootshift_fs::probe::parse_lsblk;
    use bootshift_fs::run::ScriptedRunner;

    use super::*;

    const DUAL_BOOT: &str = include_str!("../../fs/tests/fixtures/lsblk/dual-boot.json");
    const TUNE2FS: &str = include_str!("../../fs/tests/fixtures/ext4/tune2fs-l.txt");
    const RESIZE2FS: &str = include_str!("../../fs/tests/fixtures/ext4/resize2fs-P.txt");
    const NTFSRESIZE: &str = include_str!("../../fs/tests/fixtures/ntfs/ntfsresize-info.txt");

    fn entry(number: u32, start: u64, len: u64, type_guid: &str) -> GptEntry {
        GptEntry {
            number,
            type_guid: type_guid.parse().unwrap(),
            unique_guid: Guid([number as u8; 16]),
            name: String::new(),
            attributes: 0,
            extent: Extent::new(Lba(start), Sectors(len)),
        }
    }

    #[test]
    fn measures_each_filesystem_it_understands() {
        let disk = parse_lsblk(DUAL_BOOT).unwrap().remove(0);
        let entries = [
            entry(1, 2048, 532_480, "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"),
            entry(
                3,
                567_296,
                419_430_400,
                "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
            ),
            entry(
                5,
                421_636_096,
                524_288_000,
                "0fc63daf-8483-4772-8e79-3d69d8477de4",
            ),
            entry(
                6,
                945_924_096,
                16_777_216,
                "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f",
            ),
        ];
        let runner = ScriptedRunner::new()
            .reply(0, NTFSRESIZE)
            .reply(0, TUNE2FS)
            .reply(0, RESIZE2FS);
        let usable = Extent::new(Lba(34), Sectors(1_000_000_000));

        let layout = layout(&runner, &disk, usable, &entries);

        let contents: Vec<Content> = layout.partitions.iter().map(|p| p.content).collect();
        assert_eq!(
            contents,
            [
                Content::EfiSystem,
                Content::Ntfs,
                Content::Ext4,
                Content::Swap
            ]
        );
        let windows = &layout.partitions[1];
        assert_eq!(windows.min_len, Some(Sectors(2_564_096 / 512)));
        let linux = &layout.partitions[2];
        assert_eq!(linux.used, Sectors((102_400 - 87_129) * 4096 / 512));
        assert_eq!(linux.min_len, Some(Sectors(15_297 * 4096 / 512)));
        assert_eq!(layout.geometry.alignment, Sectors(2048));
    }

    #[test]
    fn a_failed_measurement_means_full_and_unshrinkable() {
        let disk = parse_lsblk(DUAL_BOOT).unwrap().remove(0);
        let entries = [entry(
            5,
            421_636_096,
            524_288_000,
            "0fc63daf-8483-4772-8e79-3d69d8477de4",
        )];
        let runner = ScriptedRunner::new().reply_error(1, "Bad magic number in super-block");
        let usable = Extent::new(Lba(34), Sectors(1_000_000_000));

        let layout = layout(&runner, &disk, usable, &entries);

        assert_eq!(layout.partitions[0].used, Sectors(524_288_000));
        assert_eq!(layout.partitions[0].min_len, None);
    }
}
