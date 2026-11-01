//! The prepare step, run in the normal system once every check has passed: put the recovery
//! system on the ESP, write the journal, and point the boot path at the recovery system.
//!
//! The order matters. Until the GRUB redirect is installed, nothing has changed how the
//! computer boots, so any earlier failure leaves it exactly as it was. The journal is read
//! back through the raw disk before the redirect, which proves the recovery system will find
//! it where the kernel command line says.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use bootshift_boot::cmdline::RecoveryArgs;
use bootshift_boot::initramfs::{self, Inputs};
use bootshift_boot::{efi, grub, journal_file};
use bootshift_core::journal;
use bootshift_core::layout::{Content, Layout};
use bootshift_core::plan::{Operation, Plan};
use bootshift_disk::{Access, FileExtent, GptImage, Journal, JournalFile, RealDevice};
use bootshift_fs::probe::{self, DiskInfo};
use bootshift_fs::run::{CommandLine, RunError, Runner};

use crate::run_plan::{BootRestore, RunPlan, SwapIdentity};

/// Keeps the recovery system's console readable.
const EXTRA_KERNEL_ARGS: &str = "loglevel=3";

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("{action}: {source}")]
    Io {
        action: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Tool(#[from] RunError),
    #[error(transparent)]
    Initramfs(#[from] initramfs::InitramfsError),
    #[error(transparent)]
    Journal(#[from] bootshift_disk::JournalError),
    #[error(transparent)]
    Disk(#[from] bootshift_disk::DiskError),
    #[error(transparent)]
    Table(#[from] bootshift_disk::GptError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Refused(String),
}

pub struct Prepare<'a> {
    pub runner: &'a dyn Runner,
    pub disk: &'a DiskInfo,
    pub layout: &'a Layout,
    pub plan: &'a Plan,
    pub esp_mount: &'a Path,
    pub grub_vendor: &'a str,
    pub kernel_image: &'a Path,
    pub initramfs: Inputs<'a>,
    /// Scratch space for building the initramfs.
    pub work_dir: &'a Path,
}

pub fn prepare(p: &Prepare<'_>) -> Result<RecoveryArgs, PrepareError> {
    let esp = p
        .layout
        .partitions
        .iter()
        .find(|partition| partition.content == Content::EfiSystem)
        .ok_or_else(|| refused("the disk has no EFI system partition"))?;
    let esp_fs_uuid = p
        .disk
        .partition(esp.number)
        .and_then(|info| info.uuid.clone())
        .ok_or_else(|| refused("the EFI system partition has no filesystem UUID"))?;
    let dir = p.esp_mount.join("EFI").join("bootshift");
    fs::create_dir_all(&dir).map_err(io_error("creating EFI/bootshift"))?;

    copy_synced(p.kernel_image, &dir.join("vmlinuz"))?;
    install_initramfs(p, &dir.join("initrd.img"))?;

    let entries = efi::read(p.runner)?;
    let recovery_order = efi::recovery_order(&entries).map_err(refused)?;
    let run = RunPlan {
        layout: p.layout.clone(),
        plan: p.plan.clone(),
        swaps: swap_identities(p)?,
        boot: BootRestore {
            esp_partition: esp.number,
            grub_vendor: p.grub_vendor.to_string(),
            original_boot_order: entries.order.clone(),
        },
    };
    let run_id = random_run_id()?;
    let record = run.to_record(run_id)?;

    let journal_path = dir.join("journal.bin");
    journal_file::create(&journal_path, &journal::initial_file_contents(&record))
        .map_err(io_error("writing the journal"))?;
    let esp_offset = esp.extent.start.0 * u64::from(p.layout.geometry.sector_size);
    let extents = journal_file::extents(p.runner, &journal_path, esp_offset)?;
    let disk_guid = verify_journal(&p.disk.path, &extents, run_id, &record.payload)?;

    back_up_table(p.runner, &p.disk.path, &dir.join("table-backup.sfdisk"))?;
    let args = RecoveryArgs {
        disk_guid,
        journal: extents,
    };
    let mut kernel_args = format!("{} {EXTRA_KERNEL_ARGS}", args.to_kernel_args());
    // The VM test harness appends `console=ttyS0` here to watch recovery on the serial port.
    // This concatenates onto the recovery PID-1 kernel command line, so it is gated to debug
    // builds (like BOOTSHIFT_SKIP_POLKIT) and cleared by the systemd unit: a stray environment
    // entry must never be able to inject kernel arguments into a packaged daemon's recovery boot.
    if cfg!(debug_assertions) {
        if let Ok(extra) = std::env::var("BOOTSHIFT_EXTRA_KERNEL_ARGS") {
            kernel_args.push(' ');
            kernel_args.push_str(&extra);
        }
    }
    write_synced(&dir.join("recovery-args.txt"), kernel_args.as_bytes())?;

    let config = grub::recovery_config(&esp_fs_uuid, &kernel_args);
    grub::install_redirect(p.esp_mount, p.grub_vendor, &config)
        .map_err(io_error("installing the GRUB redirect"))?;
    if let Err(error) = efi::set_order(p.runner, &recovery_order) {
        let _ = grub::remove_redirect(p.esp_mount, p.grub_vendor);
        let _ = efi::set_order(p.runner, &entries.order);
        return Err(error.into());
    }
    Ok(args)
}

fn install_initramfs(p: &Prepare<'_>, destination: &Path) -> Result<(), PrepareError> {
    let archive = initramfs::build_archive(p.runner, &p.initramfs)?;
    let raw = p.work_dir.join("initrd.cpio");
    let compressed = p.work_dir.join("initrd.img");
    fs::write(&raw, archive).map_err(io_error("writing the initramfs"))?;
    initramfs::compress(p.runner, &raw, &compressed)?;
    copy_synced(&compressed, destination)
}

/// The UUID and label of each swap partition the plan recreates.
fn swap_identities(
    p: &Prepare<'_>,
) -> Result<BTreeMap<bootshift_core::layout::Guid, SwapIdentity>, PrepareError> {
    let mut swaps = BTreeMap::new();
    for operation in &p.plan.operations {
        let Operation::RemoveSwap { partition } = operation else {
            continue;
        };
        let swap = p
            .layout
            .partition(*partition)
            .ok_or_else(|| refused("the plan names a swap partition the disk doesn't have"))?;
        let device = probe::partition_path(&p.disk.path, swap.number);
        let signature = probe::signature(p.runner, &device)?;
        let uuid = signature
            .uuid
            .filter(|_| signature.fstype.as_deref() == Some("swap"))
            .ok_or_else(|| {
                refused(format!(
                    "{} doesn't hold a readable swap signature",
                    swap.label()
                ))
            })?;
        swaps.insert(
            *partition,
            SwapIdentity {
                uuid,
                label: signature.label,
            },
        );
    }
    Ok(swaps)
}

/// Reads the journal back through the raw disk, exactly as the recovery system will, and
/// returns the disk's GUID for the kernel command line.
fn verify_journal(
    disk: &Path,
    extents: &[FileExtent],
    run_id: u64,
    payload: &[u8],
) -> Result<bootshift_core::layout::Guid, PrepareError> {
    let device = RealDevice::open(disk, Access::ReadOnly)?;
    let opened = Journal::open(&device, JournalFile::new(extents.to_vec())?)?;
    if opened.plan.run_id != run_id || opened.plan.payload != payload || opened.progress.is_some() {
        return Err(refused(
            "the journal read back through the disk doesn't match what was written",
        ));
    }
    Ok(GptImage::read(&device)?.disk_guid()?)
}

fn back_up_table(runner: &dyn Runner, disk: &Path, destination: &Path) -> Result<(), PrepareError> {
    let output = runner.run_ok(&CommandLine::new("sfdisk").arg("--dump").path(disk))?;
    write_synced(destination, &output.stdout)
}

fn random_run_id() -> Result<u64, PrepareError> {
    let mut bytes = [0u8; 8];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(io_error("reading /dev/urandom"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn copy_synced(source: &Path, destination: &Path) -> Result<(), PrepareError> {
    let contents = fs::read(source).map_err(io_error(&format!("reading {}", source.display())))?;
    write_synced(destination, &contents)
}

fn write_synced(path: &Path, contents: &[u8]) -> Result<(), PrepareError> {
    let action = format!("writing {}", path.display());
    let mut file = File::create(path).map_err(io_error(&action))?;
    file.write_all(contents).map_err(io_error(&action))?;
    file.sync_all().map_err(io_error(&action))
}

fn io_error(action: &str) -> impl FnOnce(io::Error) -> PrepareError + '_ {
    move |source| PrepareError::Io {
        action: action.to_string(),
        source,
    }
}

fn refused(reason: impl Into<String>) -> PrepareError {
    PrepareError::Refused(reason.into())
}
