//! `bootshift-migrate`: the one binary that runs as `send` on the source and `receive` inside the
//! receiver image on the target (implementation I8). The subcommands are wired here; the work
//! behind each lands phase by phase per `migrate/plan.md`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod common;
mod finalize;
mod mirror;
mod receive;
mod rehearse;
mod send;
mod status;
mod verify;

const DEFAULT_NQN: &str = "nqn.2026-10.org.bootshift:migrate";

#[derive(Parser)]
#[command(
    name = "bootshift-migrate",
    about = "Migrate a running Linux machine onto new hardware: verified, reshaped to the new disk, \
             the source never written, with a signed receipt.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot as the receiver on the target: wait for the source and apply what it sends.
    Receive {
        /// The new data disk the machine will run from.
        #[arg(long)]
        data: PathBuf,
        /// A small device for the receiver's journal, index and receipt.
        #[arg(long)]
        scratch: PathBuf,
        /// Address to listen on for the source.
        #[arg(long, default_value = "0.0.0.0:9876")]
        listen: String,
        /// Block size to assume when the data or scratch is an image file, not a block device.
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
        /// Migration epoch (must match the sender).
        #[arg(long, default_value_t = 1)]
        epoch: u64,
        /// Run id for the scratch journal.
        #[arg(long, default_value_t = 1)]
        run_id: u64,
        /// Write the signed receipt to this file when the migration finalizes.
        #[arg(long)]
        receipt: Option<PathBuf>,
    },
    /// Send this source to a paired receiver.
    Send {
        /// The source disk or image to send (opened read-only).
        #[arg(long)]
        source: PathBuf,
        /// The receiver's address, host:port.
        #[arg(long)]
        to: String,
        /// Block size to assume when the source is an image file, not a block device.
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
        /// Migration epoch (must match the receiver).
        #[arg(long, default_value_t = 1)]
        epoch: u64,
        /// Track live changes on this block device with eBPF and migrate live: a bulk pass, then
        /// delta passes until a cutover is cheap, then freeze and a signed receipt. Needs the
        /// `ebpf` build feature and root.
        #[arg(long)]
        track: Option<PathBuf>,
        /// Freeze this mounted filesystem for the final pass (recommended with --track).
        #[arg(long)]
        freeze: Option<String>,
        /// Data-downtime budget in milliseconds for the cutover gate (with --track).
        #[arg(long, default_value_t = 5000)]
        budget_ms: u64,
    },
    /// Show the migration state from the receiver's journal.
    Status {
        /// The receiver's scratch device holding the journal.
        #[arg(long)]
        scratch: PathBuf,
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
    },
    /// Mirror mode (sender): roll back the mirror, keeping the source and dropping the target leg.
    MirrorRollback {
        #[arg(long)]
        vg: String,
        #[arg(long)]
        lv: String,
        #[arg(long, default_value = DEFAULT_NQN)]
        nqn: String,
    },
    /// Boot a throwaway copy of the replica to test it while replication continues.
    Rehearse {
        /// The replica (target) block device to snapshot.
        #[arg(long)]
        data: PathBuf,
        /// A copy-on-write device for the snapshot's writes.
        #[arg(long)]
        cow: Option<PathBuf>,
        /// Name prefix for the dm devices.
        #[arg(long, default_value = "rehearse")]
        name: String,
        /// Also export the snapshot over nvme-tcp so another machine can boot it.
        #[arg(long)]
        export: bool,
        /// Port for the nvme-tcp export.
        #[arg(long, default_value_t = 4421)]
        port: u16,
        /// Restrict the nvme-tcp export to this initiator NQN. Without it, any host that can
        /// reach the port can read and write the snapshot.
        #[arg(long)]
        host_nqn: Option<String>,
        /// Tear the rehearsal down.
        #[arg(long)]
        end: bool,
    },
    /// Freeze the source, send the last changes, verify, and commit.
    Cutover {
        /// Prove the whole target before it boots (minutes), rather than after go-live.
        #[arg(long)]
        verified_first: bool,
        /// Schedule the cutover for this time.
        #[arg(long)]
        at: Option<String>,
    },
    /// Mirror mode (receiver): export the target PV over nvme-tcp for the source to mirror onto.
    MirrorExport {
        #[arg(long)]
        dev: PathBuf,
        #[arg(long, default_value = "0.0.0.0")]
        listen: String,
        #[arg(long, default_value_t = 4420)]
        port: u16,
        #[arg(long, default_value = DEFAULT_NQN)]
        nqn: String,
        /// Restrict the export to this initiator (sender) NQN. Without it, any host that can
        /// reach the port can read and write the target disk.
        #[arg(long)]
        host_nqn: Option<String>,
    },
    /// Mirror mode (sender): connect the target leg and mirror the LV onto it until it is in sync.
    MirrorAttach {
        #[arg(long)]
        addr: String,
        #[arg(long, default_value_t = 4420)]
        port: u16,
        #[arg(long, default_value = DEFAULT_NQN)]
        nqn: String,
        #[arg(long)]
        vg: String,
        #[arg(long)]
        lv: String,
    },
    /// Mirror mode (sender): stand down after the source filesystem is frozen and unmounted.
    MirrorStandDown {
        #[arg(long)]
        vg: String,
        #[arg(long, default_value = DEFAULT_NQN)]
        nqn: String,
    },
    /// Mirror mode (receiver): promote the synced leg to a standalone volume group.
    MirrorPromote {
        #[arg(long)]
        vg: String,
        #[arg(long)]
        lv: String,
    },
    /// Mirror mode (receiver): remove the nvme-tcp export.
    MirrorTeardown {
        #[arg(long, default_value = DEFAULT_NQN)]
        nqn: String,
        #[arg(long, default_value_t = 4420)]
        port: u16,
    },
    /// Abandon the migration and leave the source running.
    Rollback,
    /// Background re-read of the preserved source against the receipt (A10): confirm the source
    /// still matches the root it was signed with.
    Verify {
        #[arg(long)]
        receipt: PathBuf,
        #[arg(long)]
        source: PathBuf,
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
    },
    /// Check a receipt's signature and, given both disks, its roots.
    VerifyReceipt {
        file: PathBuf,
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        target: Option<PathBuf>,
        /// Block size to assume when a disk is an image file, not a block device.
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
    },
    /// Lay out, grow and adapt the target from a committed run.
    Finalize {
        /// The migrated target disk to finalize.
        #[arg(long)]
        data: PathBuf,
        /// Grow the last partition and its filesystem to fill the (larger) target disk.
        #[arg(long)]
        grow_last: bool,
        /// Adapt the guest's /etc/fstab at this mounted root for its new device names.
        #[arg(long)]
        adapt_root: Option<String>,
        /// A device rename to apply when adapting, old=new (repeatable), e.g. /dev/sda2=/dev/vda2.
        #[arg(long = "rename", value_name = "OLD=NEW")]
        renames: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Receive {
            data,
            scratch,
            listen,
            block_size,
            epoch,
            run_id,
            receipt,
        } => receive::run(
            &data,
            &scratch,
            &listen,
            block_size,
            epoch,
            run_id,
            receipt.as_deref(),
        ),
        Command::Send {
            source,
            to,
            block_size,
            epoch,
            track,
            freeze,
            budget_ms,
        } => match track {
            Some(track_dev) => run_tracked(
                &source,
                &track_dev,
                &to,
                block_size,
                epoch,
                budget_ms,
                freeze.as_deref(),
            ),
            None => send::run(&source, &to, block_size, epoch),
        },
        Command::MirrorExport {
            dev,
            listen,
            port,
            nqn,
            host_nqn,
        } => mirror::export(
            &dev.to_string_lossy(),
            &listen,
            port,
            &nqn,
            host_nqn.as_deref(),
        ),
        Command::MirrorAttach {
            addr,
            port,
            nqn,
            vg,
            lv,
        } => mirror::attach(&addr, port, &nqn, &vg, &lv),
        Command::MirrorStandDown { vg, nqn } => mirror::stand_down(&vg, &nqn),
        Command::MirrorPromote { vg, lv } => mirror::promote(&vg, &lv),
        Command::MirrorTeardown { nqn, port } => mirror::teardown(&nqn, port),
        Command::MirrorRollback { vg, lv, nqn } => mirror::rollback(&vg, &lv, &nqn),
        Command::Status {
            scratch,
            block_size,
        } => status::run(&scratch, block_size),
        Command::Rehearse {
            data,
            cow,
            name,
            export,
            port,
            host_nqn,
            end,
        } => {
            let nqn = "nqn.2026-10.org.bootshift:rehearse";
            if end {
                rehearse::end(&name, export.then_some((port, nqn)))
            } else {
                let cow = cow.ok_or_else(|| anyhow::anyhow!("rehearse needs --cow <device>"))?;
                let export = export.then_some(("0.0.0.0", port, nqn, host_nqn.as_deref()));
                rehearse::start(
                    &data.to_string_lossy(),
                    &cow.to_string_lossy(),
                    &name,
                    export,
                )
            }
        }
        Command::Cutover { .. } => not_implemented("cutover"),
        Command::Rollback => not_implemented("rollback"),
        Command::Verify {
            receipt,
            source,
            block_size,
        } => verify::run(&receipt, Some(&source), None, block_size),
        Command::VerifyReceipt {
            file,
            source,
            target,
            block_size,
        } => verify::run(&file, source.as_deref(), target.as_deref(), block_size),
        Command::Finalize {
            data,
            grow_last,
            adapt_root,
            renames,
        } => {
            let parsed = parse_renames(&renames)?;
            finalize::run(&data, grow_last, adapt_root.as_deref(), &parsed)
        }
    }
}

/// Parses `old=new` device-rename arguments.
fn parse_renames(renames: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    renames
        .iter()
        .map(|spec| {
            spec.split_once('=')
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .ok_or_else(|| anyhow::anyhow!("--rename expects OLD=NEW, got `{spec}`"))
        })
        .collect()
}

#[cfg(feature = "ebpf")]
fn run_tracked(
    source: &std::path::Path,
    track_dev: &std::path::Path,
    to: &str,
    block_size: u32,
    epoch: u64,
    budget_ms: u64,
    freeze: Option<&str>,
) -> anyhow::Result<()> {
    send::run_live(source, track_dev, to, block_size, epoch, budget_ms, freeze)
}

#[cfg(not(feature = "ebpf"))]
fn run_tracked(
    _source: &std::path::Path,
    _track_dev: &std::path::Path,
    _to: &str,
    _block_size: u32,
    _epoch: u64,
    _budget_ms: u64,
    _freeze: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::bail!("live tracked migration needs the eBPF tracker; rebuild with --features ebpf")
}

fn not_implemented(name: &str) -> anyhow::Result<()> {
    anyhow::bail!("bootshift-migrate: `{name}` is not implemented yet (see migrate/plan.md)")
}
