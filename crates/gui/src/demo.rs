//! A made-up disk for walking through the whole flow without hardware. Nothing here touches a
//! disk, and the "plan" only lists what changed; the real planner runs in the helper.

use bootshift_core::layout::{Content, Extent, Geometry, Guid, Layout, Partition, Target};
use bootshift_core::plan::Operation;
use bootshift_core::units::{Lba, Sectors};

use crate::backend::{Backend, BackendError};
use crate::protocol::{Applied, CheckResult, DiskSummary, FinishedRun, Planned, Probed, Reply};

const MIB: u64 = 2048;
const GIB: u64 = 1024 * MIB;
const DISK_BYTES: u64 = 512 * (1 << 30);
pub const DISK_PATH: &str = "/dev/demo";

pub struct DemoBackend {
    layout: Layout,
    /// A finished run to reopen onto, as the helper would report after a real restart. Set from
    /// `BOOTSHIFT_DONE_DEMO` so the finished screen can be demoed without hardware.
    finished: Option<FinishedRun>,
}

impl DemoBackend {
    pub fn new() -> Self {
        Self {
            layout: demo_layout(),
            finished: std::env::var_os("BOOTSHIFT_DONE_DEMO").map(|_| demo_finished_run()),
        }
    }

    /// A demo backend that reports a completed run, so the app reopens straight onto the finished
    /// screen (what happens after the machine restarts and the move is done).
    #[cfg(test)]
    pub fn finished() -> Self {
        Self {
            layout: demo_layout(),
            finished: Some(demo_finished_run()),
        }
    }
}

/// The canned numbers the finished screen shows in the demo: a 90 GiB move in 1448 chunks.
fn demo_finished_run() -> FinishedRun {
    FinishedRun {
        bytes: 90 << 30,
        chunks: 1448,
        power_cuts: 1,
        seconds: 214,
    }
}

impl Backend for DemoBackend {
    fn is_demo(&self) -> bool {
        true
    }

    fn list_disks(&mut self) -> Result<Reply<Vec<DiskSummary>>, BackendError> {
        Ok(Reply::Ok {
            value: vec![DiskSummary {
                path: DISK_PATH.to_string(),
                size: DISK_BYTES,
                transport: Some("nvme".to_string()),
                removable: false,
                partition_table: Some("gpt".to_string()),
            }],
        })
    }

    fn probe(&mut self, disk: &str) -> Result<Reply<Probed>, BackendError> {
        Ok(Reply::Ok {
            value: Probed {
                disk: disk.to_string(),
                size: DISK_BYTES,
                layout: self.layout.clone(),
                target: current_target(&self.layout),
            },
        })
    }

    fn plan(&mut self, _disk: &str, target: &Target) -> Result<Reply<Planned>, BackendError> {
        let operations = list_changes(&self.layout, target);
        let copied: Sectors = operations
            .iter()
            .map(|op| match op {
                Operation::Move { copy_estimate, .. } => *copy_estimate,
                _ => Sectors(0),
            })
            .sum();
        Ok(Reply::Ok {
            value: Planned {
                operations,
                copy_bytes: copied.to_bytes(self.layout.geometry.sector_size),
                unavailable: Vec::new(),
            },
        })
    }

    // The pretend checks pass and apply "succeeds", so the whole flow can be walked through up to
    // the restart. The rail says it's demo data throughout.
    fn check(
        &mut self,
        _disk: &str,
        _target: &Target,
    ) -> Result<Reply<Option<Vec<CheckResult>>>, BackendError> {
        Ok(Reply::Ok {
            value: Some(demo_checks()),
        })
    }

    fn apply(&mut self, _disk: &str, _target: &Target) -> Result<Reply<Applied>, BackendError> {
        Ok(Reply::Ok {
            value: Applied {
                restart_to_begin: true,
            },
        })
    }

    fn last_run(&mut self) -> Result<Reply<Option<FinishedRun>>, BackendError> {
        Ok(Reply::Ok {
            value: self.finished.clone(),
        })
    }
}

/// 512 GB, 512-byte sectors, 1 MiB alignment, 476 GiB usable from 1 MiB, with about 17 GiB
/// left unallocated at the end.
pub fn demo_layout() -> Layout {
    Layout {
        geometry: Geometry {
            sector_size: 512,
            usable: Extent::new(Lba(MIB), Sectors(476 * GIB)),
            alignment: Sectors(MIB),
        },
        partitions: vec![
            part(
                1,
                Content::EfiSystem,
                "EFI system partition",
                MIB,
                260 * MIB,
                40 * MIB,
                None,
            ),
            part(
                2,
                Content::Opaque,
                "Microsoft reserved",
                261 * MIB,
                16 * MIB,
                16 * MIB,
                None,
            ),
            part(
                3,
                Content::Ntfs,
                "Windows",
                277 * MIB,
                200 * GIB,
                60 * GIB,
                Some(61 * GIB),
            ),
            part(
                4,
                Content::Ntfs,
                "Windows recovery",
                277 * MIB + 200 * GIB,
                800 * MIB,
                500 * MIB,
                Some(600 * MIB),
            ),
            part(
                5,
                Content::Ext4,
                "Linux",
                1077 * MIB + 200 * GIB,
                250 * GIB,
                90 * GIB,
                Some(92 * GIB),
            ),
            part(
                6,
                Content::Swap,
                "",
                1077 * MIB + 450 * GIB,
                8 * GIB,
                0,
                Some(0),
            ),
        ],
    }
}

fn part(
    number: u32,
    content: Content,
    name: &str,
    start: u64,
    len: u64,
    used: u64,
    min_len: Option<u64>,
) -> Partition {
    Partition {
        number,
        unique_guid: demo_guid(number),
        type_guid: Guid([0; 16]),
        name: name.to_string(),
        attributes: 0,
        extent: Extent::new(Lba(start), Sectors(len)),
        content,
        used: Sectors(used),
        min_len: min_len.map(Sectors),
    }
}

/// `0000000N-0000-4000-8000-00000000000N`, the same GUIDs the web build uses.
fn demo_guid(number: u32) -> Guid {
    let n = number as u8;
    Guid([n, 0, 0, 0, 0, 0, 0, 0x40, 0x80, 0, 0, 0, 0, 0, 0, n])
}

fn current_target(layout: &Layout) -> Target {
    layout
        .partitions
        .iter()
        .map(|p| (p.unique_guid, p.extent))
        .collect()
}

/// A Shrink for anything that got smaller, a Move for anything whose start changed (copying
/// its used data), and a Grow for anything that got bigger.
fn list_changes(layout: &Layout, target: &Target) -> Vec<Operation> {
    let mut operations = Vec::new();
    for p in &layout.partitions {
        let Some(to) = target.get(&p.unique_guid) else {
            // A partition the target leaves out is being deleted.
            operations.push(Operation::Delete {
                partition: p.unique_guid,
            });
            continue;
        };
        let guid = p.unique_guid;
        if to.len < p.extent.len {
            operations.push(Operation::Shrink {
                partition: guid,
                len: to.len,
            });
        }
        if to.start != p.extent.start {
            let len = to.len.min(p.extent.len);
            operations.push(Operation::Move {
                partition: guid,
                from: p.extent.start,
                to: to.start,
                len,
                copy_estimate: p.used.min(len),
            });
        }
        if to.len > p.extent.len {
            operations.push(Operation::Grow {
                partition: guid,
                len: to.len,
            });
        }
    }
    operations
}

/// The names and order mirror `engine::preflight::evaluate`; keep them in step.
fn demo_checks() -> Vec<CheckResult> {
    [
        ("Firmware mode", "UEFI, Secure Boot on"),
        ("Partition table", "GPT, 6 partitions, backup header intact"),
        ("How the disk is attached", "NVMe, direct (not over USB)"),
        ("Power", "On mains, battery at 87%"),
        ("Disk health", "SMART self-assessment passed"),
        ("Kernel modules", "Present for 6.11.0-9-generic"),
        ("Room on the EFI partition", "180 MiB free, 46 MiB needed"),
        ("Bootloader", "GRUB (ubuntu), signed shim in place"),
        ("Hibernation", "No hibernated system in swap"),
        ("Encryption", "No BitLocker volume on this disk"),
        ("Filesystems", "ext4 and NTFS both clean and readable"),
        (
            "Partition-table backup",
            "Written to the EFI partition first",
        ),
    ]
    .into_iter()
    .map(|(name, detail)| CheckResult {
        name: name.to_string(),
        detail: detail.to_string(),
        passed: true,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use bootshift_core::validate::validate;

    use super::*;

    #[test]
    fn the_demo_guids_match_the_web_build() {
        assert_eq!(
            demo_guid(4).to_string(),
            "00000004-0000-4000-8000-000000000004"
        );
    }

    #[test]
    fn the_demo_disk_leaves_about_17_gib_free_at_the_end() {
        let layout = demo_layout();
        let last = layout
            .partitions
            .iter()
            .map(|p| p.extent.end())
            .max()
            .unwrap();
        let free = layout.geometry.usable.end().0 - last.0;
        assert_eq!(free, 18 * GIB - 1076 * MIB);
        assert_eq!(validate(&layout, &current_target(&layout)), Ok(()));
    }

    #[test]
    fn the_demo_plan_shrinks_moves_and_grows_what_changed() {
        let mut backend = DemoBackend::new();
        let layout = demo_layout();
        let mut target = current_target(&layout);
        let windows = demo_guid(3);
        let linux = demo_guid(5);
        target.insert(windows, Extent::new(Lba(277 * MIB), Sectors(100 * GIB)));
        target.insert(
            linux,
            Extent::new(Lba(277 * MIB + 120 * GIB), Sectors(260 * GIB)),
        );
        let Ok(Reply::Ok { value }) = backend.plan(DISK_PATH, &target) else {
            panic!("the demo always plans");
        };
        assert!(matches!(value.operations[0], Operation::Shrink { .. }));
        assert!(matches!(value.operations[1], Operation::Move { .. }));
        assert!(matches!(value.operations[2], Operation::Grow { .. }));
        assert_eq!(value.copy_bytes, 90 * (1 << 30));
    }

    #[test]
    fn the_demo_lists_the_twelve_checks_all_passing() {
        let checks = demo_checks();
        assert_eq!(checks.len(), 12);
        assert!(checks.iter().all(|c| c.passed));
        assert_eq!(checks[0].name, "Firmware mode");
        assert_eq!(checks[11].name, "Partition-table backup");
    }
}
