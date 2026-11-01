//! Rehearsal (implementation I7, plan Phase 10): take a writable device-mapper snapshot of the
//! replica so the migrated system can be booted and tested in a throwaway VM while replication keeps
//! running — every write the test makes goes to a copy-on-write device, never to the replica. The
//! receiver owns the target disk, so it can stack dm freely: a `snapshot-origin` target that the
//! applier writes through, and a `snapshot` target that is the frozen, writable view.
//!
//! Thin, idempotent wrappers over dmsetup (and the mirror-mode nvmet export, to reach another VM).

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::mirror;

/// Starts a rehearsal: a `snapshot-origin` over `data` (so writes through it copy old blocks aside)
/// and a `snapshot` backed by `cow`. The frozen, writable view appears at `/dev/mapper/<name>-snap`;
/// the applier keeps replicating through `/dev/mapper/<name>-origin`. With `export`, the snapshot is
/// also offered over nvme-tcp so another machine can boot it.
pub fn start(
    data: &str,
    cow: &str,
    name: &str,
    export: Option<(&str, u16, &str, Option<&str>)>,
) -> Result<()> {
    let sectors = device_sectors(data)?;
    create(
        &format!("{name}-origin"),
        &format!("0 {sectors} snapshot-origin {data}"),
    )?;
    create(
        &format!("{name}-snap"),
        &format!("0 {sectors} snapshot {data} {cow} P 8"),
    )?;
    let snap = format!("/dev/mapper/{name}-snap");
    println!("rehearse: snapshot ready at {snap}");
    println!("rehearse: replication continues through /dev/mapper/{name}-origin");
    if let Some((listen, port, nqn, host_nqn)) = export {
        mirror::export(&snap, listen, port, nqn, host_nqn)?;
    }
    Ok(())
}

/// Ends a rehearsal: removes the export (if any) and the snapshot and origin, leaving the replica
/// exactly as replication left it.
pub fn end(name: &str, teardown_export: Option<(u16, &str)>) -> Result<()> {
    if let Some((port, nqn)) = teardown_export {
        let _ = mirror::teardown(nqn, port);
    }
    remove(&format!("{name}-snap"));
    remove(&format!("{name}-origin"));
    println!("rehearse: ended; the snapshot is gone and the replica is untouched");
    Ok(())
}

fn device_sectors(dev: &str) -> Result<u64> {
    let output = Command::new("blockdev")
        .args(["--getsz", dev])
        .output()
        .with_context(|| format!("running blockdev --getsz {dev}"))?;
    if !output.status.success() {
        bail!(
            "blockdev --getsz {dev} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .with_context(|| format!("parsing the sector count of {dev}"))
}

fn create(name: &str, table: &str) -> Result<()> {
    // Idempotent: a leftover device from an interrupted run is removed first.
    remove(name);
    let status = Command::new("dmsetup")
        .args(["create", name, "--table", table])
        .status()
        .with_context(|| format!("running dmsetup create {name}"))?;
    if !status.success() {
        bail!("dmsetup create {name} failed with {status}");
    }
    Ok(())
}

fn remove(name: &str) {
    // Ignore "no such device": it is the normal case for the idempotent pre-create cleanup.
    let _ = Command::new("dmsetup").args(["remove", name]).output();
}
