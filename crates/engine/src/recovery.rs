//! What the recovery system does once it has found its disk: run the plan, then put normal
//! booting back.

use std::path::Path;

use bootshift_boot::cmdline::RecoveryArgs;
use bootshift_boot::{efi, grub};
use bootshift_core::journal::{Progress, Stage};
use bootshift_core::layout::Guid;
use bootshift_disk::{
    Access, BlockDevice, GptImage, Journal, JournalError, JournalFile, RealDevice,
};
use bootshift_fs::mount;
use bootshift_fs::probe::partition_path;
use bootshift_fs::run::{RunError, Runner};

use crate::copier::{CopyOptions, ProgressSink};
use crate::executor::{ExecError, check_table_writes, execute};
use crate::run_plan::{BootRestore, RunPlan};
use crate::system_filesystems::SystemFilesystems;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Disk(#[from] bootshift_disk::DiskError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("the journal's plan can't be read: {0}")]
    Plan(#[from] serde_json::Error),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    Tool(#[from] RunError),
    #[error(transparent)]
    Table(#[from] bootshift_disk::GptError),
    #[error(
        "the partition table changed after the plan was made, so nothing was touched and normal booting is restored; plan again"
    )]
    LayoutChanged,
    #[error(
        "the plan can't be carried out on this disk ({0}), so nothing was touched and normal booting is restored"
    )]
    CantCarryOut(#[source] ExecError),
}

impl RecoveryError {
    /// Whether recovery stopped before its first write and put normal booting back, so the
    /// computer can simply restart into its system.
    pub fn left_disk_untouched(&self) -> bool {
        matches!(self, Self::LayoutChanged | Self::CantCarryOut(_))
    }
}

/// Whether `device` holds the GPT with this disk GUID.
pub fn disk_matches<D: BlockDevice + ?Sized>(device: &D, guid: Guid) -> bool {
    GptImage::read(device)
        .and_then(|table| table.disk_guid())
        .is_ok_and(|found| found == guid)
}

/// Runs the plan recorded in the journal to the end, then restores normal booting.
pub fn run(
    runner: &dyn Runner,
    disk_path: &Path,
    args: &RecoveryArgs,
    work_dir: &Path,
    options: &CopyOptions,
    sink: &mut dyn ProgressSink,
) -> Result<(), RecoveryError> {
    let mut disk = RealDevice::open(disk_path, Access::ReadWriteShared)?;
    let opened = Journal::open(&disk, JournalFile::new(args.journal.clone())?)?;
    let run = RunPlan::from_record(&opened.plan)?;
    let mut journal = opened.journal;

    if opened.progress.is_none() {
        // On a fresh run, every reason we can't carry out the plan must reach restore_booting below,
        // never propagate before it, or the GRUB redirect is left in place and the machine
        // boot-loops into recovery. A table_matches error is now possible (entries() rejects a
        // corrupt/out-of-range GPT), so route it to the same safe path as a changed layout.
        let refusal = match table_matches(&disk, &run) {
            Ok(true) => check_table_writes(&disk, &run.layout, &run.plan)
                .err()
                .map(RecoveryError::CantCarryOut),
            Ok(false) => Some(RecoveryError::LayoutChanged),
            Err(error) => Some(error),
        };
        if let Some(refusal) = refusal {
            restore_booting(runner, disk_path, &run.boot, &work_dir.join("esp"))?;
            journal.commit(&mut disk, &done())?;
            return Err(refusal);
        }
    }

    let mut filesystems = SystemFilesystems::new(
        runner,
        disk_path,
        run.layout.geometry.sector_size,
        &work_dir.join("partition"),
        run.swaps.clone(),
    );
    execute(
        &mut disk,
        &mut journal,
        &mut filesystems,
        &run,
        opened.progress.clone(),
        options,
        sink,
    )?;

    let already_done = opened
        .progress
        .as_ref()
        .is_some_and(|p| p.stage == Stage::Done);
    if !already_done {
        restore_booting(runner, disk_path, &run.boot, &work_dir.join("esp"))?;
        journal.commit(&mut disk, &done())?;
    }
    Ok(())
}

/// Whether the disk's table still describes exactly the layout the plan starts from.
pub fn table_matches<D: BlockDevice + ?Sized>(
    disk: &D,
    run: &RunPlan,
) -> Result<bool, RecoveryError> {
    let mut on_disk: Vec<_> = GptImage::read(disk)?
        .entries()?
        .into_iter()
        .map(|e| (e.number, e.unique_guid, e.type_guid, e.extent))
        .collect();
    let mut planned: Vec<_> = run
        .layout
        .partitions
        .iter()
        .map(|p| (p.number, p.unique_guid, p.type_guid, p.extent))
        .collect();
    on_disk.sort_by_key(|entry| entry.0);
    planned.sort_by_key(|entry| entry.0);
    Ok(on_disk == planned)
}

/// Puts the original boot order and GRUB config back. Safe to repeat.
pub fn restore_booting(
    runner: &dyn Runner,
    disk_path: &Path,
    boot: &BootRestore,
    esp_mountpoint: &Path,
) -> Result<(), RunError> {
    efi::set_order(runner, &boot.original_boot_order)?;
    let esp = partition_path(disk_path, boot.esp_partition);
    mount::with_mounted(runner, &esp, "vfat", esp_mountpoint, |esp| {
        grub::remove_redirect(esp, &boot.grub_vendor).map_err(|source| RunError::Start {
            command: "restore grub.cfg".to_string(),
            source,
        })
    })
}

fn done() -> Progress {
    Progress {
        stage: Stage::Done,
        operation: 0,
        steps_done: 0,
        next_chunk: 0,
        hash_chain: [0; 32],
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use bootshift_disk::MemDisk;
    use bootshift_fs::run::ScriptedRunner;
    use gptman::GPT;

    use super::*;

    fn disk_with_guid(guid: [u8; 16]) -> MemDisk {
        let mut cursor = Cursor::new(vec![0; 1 << 20]);
        let mut gpt = GPT::new_from(&mut cursor, 512, guid).unwrap();
        gpt.write_into(&mut cursor).unwrap();
        MemDisk::from_bytes(cursor.into_inner(), 512).unwrap()
    }

    #[test]
    fn finds_the_disk_by_its_gpt_guid() {
        let disk = disk_with_guid([0xAB; 16]);
        assert!(disk_matches(&disk, Guid([0xAB; 16])));
        assert!(!disk_matches(&disk, Guid([0xCD; 16])));
        assert!(!disk_matches(
            &MemDisk::zeroed(1 << 20, 512).unwrap(),
            Guid([0xAB; 16])
        ));
    }

    fn run_for(disk: &MemDisk) -> RunPlan {
        use bootshift_core::layout::{Content, Geometry, Layout, Partition};
        use bootshift_core::plan::Plan;
        let table = GptImage::read(disk).unwrap();
        let partitions = table
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| Partition {
                number: e.number,
                unique_guid: e.unique_guid,
                type_guid: e.type_guid,
                name: e.name,
                attributes: e.attributes,
                extent: e.extent,
                content: Content::Opaque,
                used: e.extent.len,
                min_len: None,
            })
            .collect();
        RunPlan {
            layout: Layout {
                geometry: Geometry {
                    sector_size: 512,
                    usable: table.usable(),
                    alignment: bootshift_core::units::Sectors(8),
                },
                partitions,
            },
            plan: Plan::default(),
            swaps: Default::default(),
            boot: BootRestore {
                esp_partition: 1,
                grub_vendor: "ubuntu".to_string(),
                original_boot_order: Vec::new(),
            },
        }
    }

    fn disk_with_partition(start: u64) -> MemDisk {
        let mut cursor = Cursor::new(vec![0; 1 << 20]);
        let mut gpt = GPT::new_from(&mut cursor, 512, [0xAB; 16]).unwrap();
        gpt[1] = gptman::GPTPartitionEntry {
            partition_type_guid: [0xAF; 16],
            unique_partition_guid: [1; 16],
            starting_lba: start,
            ending_lba: start + 99,
            attribute_bits: 0,
            partition_name: "data".into(),
        };
        gpt.write_into(&mut cursor).unwrap();
        MemDisk::from_bytes(cursor.into_inner(), 512).unwrap()
    }

    #[test]
    fn a_table_changed_after_planning_is_noticed() {
        let planned = disk_with_partition(100);
        let run = run_for(&planned);

        assert!(table_matches(&planned, &run).unwrap());
        assert!(!table_matches(&disk_with_partition(200), &run).unwrap());
    }

    #[test]
    fn an_unreadable_table_makes_table_matches_refuse_not_abort() {
        // A corrupt/unreadable GPT surfaces as an Err from GptImage::read / entries() (the latter now
        // rejects out-of-range LBAs instead of overflowing). table_matches must return that Err, not
        // panic, so run() can route it to restore_booting on a fresh run rather than propagating
        // before the safety net and leaving the GRUB redirect in place (ENG-02).
        let run = run_for(&disk_with_partition(100));
        let unreadable = MemDisk::zeroed(1 << 20, 512).unwrap();
        assert!(table_matches(&unreadable, &run).is_err());
    }

    #[test]
    fn restoring_puts_the_boot_order_back_and_then_the_grub_config() {
        let esp = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new().reply(0, "").reply(0, "").reply(0, "");
        let boot = BootRestore {
            esp_partition: 1,
            grub_vendor: "ubuntu".to_string(),
            original_boot_order: vec![0x0000, 0x0001],
        };

        restore_booting(&runner, Path::new("/dev/nvme0n1"), &boot, esp.path()).unwrap();

        let commands = runner.commands();
        assert_eq!(commands[0], "efibootmgr --bootorder 0000,0001");
        assert!(commands[1].starts_with("mount -t vfat /dev/nvme0n1p1"));
        assert!(commands[2].starts_with("umount"));
    }
}
