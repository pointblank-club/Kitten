//! `verify-receipt` (implementation I8): check a signed receipt, and, given the disks, recompute
//! the Merkle roots and confirm they match what it claims. This is the proof anyone can run after
//! the fact (USP U1): the receipt is self-contained, so holding both disks is enough to confirm the
//! target equals the source and the receiver signed it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use bootshift_disk::{Access, BlockDevice};
use bootshift_migrate_core::Receipt;

use crate::common::{device_root, open_device};

pub fn run(
    file: &Path,
    source: Option<&Path>,
    target: Option<&Path>,
    block_size: u32,
) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let receipt: Receipt = serde_json::from_str(&text).context("parsing the receipt JSON")?;

    let mut failures = 0u32;
    let mut check = |label: &str, ok: bool| {
        println!("  [{}] {label}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    check("the receiver's signature is valid", receipt.verify());
    check("the source and target roots match", receipt.roots_match());

    // Both roots cover exactly the migrated source byte range (the sender's FinalRoot and the
    // receiver's target root are both computed over source_len), not the whole device, so a disk
    // larger than the source still verifies. Hashing device.len_bytes() here was the same bug as
    // MIG-01 in receive.rs. NOTE: wants the two-VM migrate-rig for full end-to-end validation.
    let covered = receipt.source_len;
    if let Some(path) = source {
        let device = open_device(path, Access::ReadOnly, block_size)?;
        let ok =
            device.len_bytes() >= covered && device_root(&device, covered)? == receipt.source_root;
        check("this source disk matches the receipt's source root", ok);
    }
    if let Some(path) = target {
        let device = open_device(path, Access::ReadOnly, block_size)?;
        let ok =
            device.len_bytes() >= covered && device_root(&device, covered)? == receipt.target_root;
        check("this target disk matches the receipt's target root", ok);
    }

    if failures > 0 {
        bail!("the receipt did NOT verify ({failures} check(s) failed)");
    }
    println!(
        "the receipt verifies: the target equals the source, signed by receiver key {}",
        hex8(&receipt.receiver_key)
    );
    Ok(())
}

fn hex8(bytes: &[u8; 32]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}
