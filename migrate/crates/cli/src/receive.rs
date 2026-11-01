//! `receive`: listen for the source, and apply every chunk it sends to the target disk, verifying
//! each against the sender's hash before it is recorded (M3), then acknowledge the last durable
//! sequence. A crash resumes from the newest scratch slot; the sender resends from the last ack, so
//! a batch already applied is a no-op (A4.2). The journal and index live on the scratch device.

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use bootshift_disk::{Access, BlockDevice, RealDevice};
use bootshift_migrate_core::{
    DiskRange, Epoch, FrameSeq, LayoutMap, Message, PROTOCOL_VERSION, Receipt, Role, decode, encode,
};
use bootshift_migrate_net::{Link, LinkError, TcpLink};
use bootshift_migrate_receive::{Applier, IncomingChunk, Store};

use crate::common::{RECEIPT_CHUNK, device_root, now_iso, open_device};

/// Apply once this many chunk bytes have gathered, or at the end of a pass.
const BATCH_BYTES: usize = 16 << 20;

/// The receiver's demo signing key seed. A real receiver uses a persistent identity key.
const RECEIVER_KEY_SEED: [u8; 32] = [0x5a; 32];

/// Tracks whether the target has been proved byte-equal to the source and still is. A FinalRoot
/// records whether the roots matched; any chunk applied after that proof invalidates it; a commit
/// is only honoured while the proof still holds. This is what makes a "verified" commit mean
/// "nothing changed after the proof".
#[derive(Default)]
struct CommitGate {
    verified: bool,
    final_seen: bool,
}

impl CommitGate {
    /// A data chunk arrived. After a proof has been taken, it no longer describes the target.
    fn on_chunk(&mut self) {
        if self.final_seen {
            self.verified = false;
        }
    }

    /// A FinalRoot was processed; `roots_match` is whether the target equalled the source.
    fn on_final_root(&mut self, roots_match: bool) {
        self.verified = roots_match;
        self.final_seen = true;
    }

    /// Whether a commit may be reported as success right now.
    fn may_commit(&self) -> bool {
        self.verified
    }
}

pub fn run(
    data: &Path,
    scratch: &Path,
    listen: &str,
    block_size: u32,
    epoch: u64,
    run_id: u64,
    receipt_out: Option<&Path>,
) -> Result<()> {
    let mut target = open_device(data, Access::ReadWrite, block_size)?;
    let mut scratch_dev = open_device(scratch, Access::ReadWrite, block_size)?;
    let data_len = target.len_bytes();
    let epoch = Epoch(epoch);
    let layout = LayoutMap::new(vec![(
        DiskRange::new(0, data_len),
        DiskRange::new(0, data_len),
    )]);
    let mut applier = {
        let store = Store::new(&mut scratch_dev);
        Applier::resume(&store, run_id, epoch, layout, data_len)
            .context("reading the newest scratch slot")?
    };

    let listener = TcpListener::bind(listen).with_context(|| format!("binding {listen}"))?;
    println!(
        "receive: listening on {listen}; target {} ({data_len} bytes), resuming from seq {}",
        data.display(),
        applier.acked().0
    );
    let mut link = TcpLink::accept(&listener).context("accepting the source")?;

    let mut batch: Vec<IncomingChunk> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut source_len = data_len;
    // Tracks whether the target has been proved byte-equal to the source and still is: a commit is
    // only honoured while that proof holds, so a bare Committed (no proof) or a write after the
    // proof cannot be reported as success.
    let mut gate = CommitGate::default();
    loop {
        match link.recv() {
            Ok(Some(frame)) => {
                let (frame_epoch, seq, message) = decode(&frame).context("decoding a frame")?;
                // Reject frames that do not belong to this run's epoch (replay / cross-run
                // confusion). Legitimate frames always carry the current epoch.
                if frame_epoch.0 != epoch.0 {
                    continue;
                }
                match message {
                    Message::Hello { disk_len, .. } => {
                        // Validate the source length against the real target before trusting it: it
                        // later drives device_root (buffer/leaf allocation sized by source_len), so an
                        // attacker-chosen huge disk_len would otherwise be a remote OOM/abort. The
                        // source must also fit the target, or the migration cannot complete.
                        if disk_len > data_len {
                            anyhow::bail!(
                                "the source claims {disk_len} bytes but the target is only {data_len}; \
                                 refusing"
                            );
                        }
                        source_len = disk_len;
                        link.send(encode(
                            epoch,
                            FrameSeq(0),
                            &Message::Hello {
                                protocol: PROTOCOL_VERSION,
                                role: Role::Receiver,
                                software: concat!("bootshift-migrate ", env!("CARGO_PKG_VERSION"))
                                    .to_string(),
                                disk_len: data_len,
                                block_size: target.logical_block_size(),
                                used_digest: [0u8; 32],
                            },
                        )?);
                    }
                    Message::Chunk {
                        source, hash, data, ..
                    } => {
                        // Data after a proof invalidates it: the target has changed since the root
                        // was computed, so a later commit must not claim the old verification.
                        gate.on_chunk();
                        batch_bytes += data.len();
                        batch.push(IncomingChunk {
                            seq,
                            source,
                            hash,
                            data,
                        });
                        if batch_bytes >= BATCH_BYTES {
                            apply(
                                &mut applier,
                                &mut scratch_dev,
                                &mut target,
                                &mut batch,
                                &mut link,
                                epoch,
                            )?;
                            batch_bytes = 0;
                        }
                    }
                    Message::PassEnd { .. } => {
                        apply(
                            &mut applier,
                            &mut scratch_dev,
                            &mut target,
                            &mut batch,
                            &mut link,
                            epoch,
                        )?;
                        batch_bytes = 0;
                        println!(
                            "receive: pass complete, acknowledged through seq {}",
                            applier.acked().0
                        );
                    }
                    Message::FinalRoot { root: source_root } => {
                        // Flush anything pending, then prove byte-equality with a signed receipt.
                        apply(
                            &mut applier,
                            &mut scratch_dev,
                            &mut target,
                            &mut batch,
                            &mut link,
                            epoch,
                        )?;
                        batch_bytes = 0;
                        // Compute the target root over exactly the SOURCE byte range the sender's
                        // FinalRoot covers, not the full target device: when the target is larger
                        // than the source, hashing the whole device would never match (false
                        // negative). `source_len` is the sender's disk_len from the Hello.
                        // NOTE: needs the two-VM migrate-rig to validate end to end.
                        let target_root = device_root(&target, source_len)
                            .context("computing the target root for the receipt")?;
                        let now = now_iso();
                        let mut receipt = Receipt::new(
                            format!("run-{run_id}"),
                            epoch.0,
                            source_len,
                            data_len,
                            RECEIPT_CHUNK as u32,
                            source_root,
                            target_root,
                            now.clone(),
                            now,
                            0,
                        );
                        receipt.sign_with_seed(&RECEIVER_KEY_SEED);
                        let matched = receipt.roots_match();
                        gate.on_final_root(matched);
                        if let Some(path) = receipt_out {
                            let json = serde_json::to_vec_pretty(&receipt)
                                .context("serialising the receipt")?;
                            std::fs::write(path, json).with_context(|| {
                                format!("writing the receipt to {}", path.display())
                            })?;
                            println!("receive: wrote receipt to {}", path.display());
                        }
                        link.send(encode(epoch, FrameSeq(0), &Message::Receipt { receipt })?);
                        println!(
                            "receive: sent signed receipt (roots {})",
                            if matched { "match" } else { "DIFFER" }
                        );
                    }
                    Message::Committed { .. } => {
                        // Refuse a commit that is not currently backed by a matching FinalRoot: the
                        // target has not been proved equal to the source (or changed after the
                        // proof), so success would be a false claim.
                        if !gate.may_commit() {
                            anyhow::bail!(
                                "the source sent a commit without a verified final root; refusing \
                                 to report success (the target was not proved equal to the source)"
                            );
                        }
                        println!("receive: source committed; the target is written and verified");
                        return Ok(());
                    }
                    Message::Error { reason, fix } => {
                        anyhow::bail!("the source reported an error: {reason} (fix: {fix})");
                    }
                    _ => {}
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(LinkError::Cut) => {
                println!(
                    "receive: the source disconnected, acknowledged through seq {}",
                    applier.acked().0
                );
                return Ok(());
            }
        }
    }
}

/// Applies the gathered batch (write, flush, read-back verify, re-hash index, commit) and sends the
/// acknowledgement for the last durable sequence.
fn apply(
    applier: &mut Applier,
    scratch: &mut RealDevice,
    target: &mut RealDevice,
    batch: &mut Vec<IncomingChunk>,
    link: &mut TcpLink,
    epoch: Epoch,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let acked = {
        let mut store = Store::new(scratch);
        applier
            .apply_batch(&mut store, target, batch)
            .context("applying and verifying a batch")?
    };
    batch.clear();
    link.send(encode(epoch, acked, &Message::Ack { acked_seq: acked.0 })?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CommitGate;

    #[test]
    fn nothing_is_committable_before_a_proof() {
        let gate = CommitGate::default();
        assert!(!gate.may_commit());
    }

    #[test]
    fn a_matching_proof_is_committable() {
        let mut gate = CommitGate::default();
        gate.on_chunk(); // normal streaming before the proof does not matter
        gate.on_final_root(true);
        assert!(gate.may_commit());
    }

    #[test]
    fn a_non_matching_proof_is_not_committable() {
        let mut gate = CommitGate::default();
        gate.on_final_root(false);
        assert!(!gate.may_commit());
    }

    #[test]
    fn a_write_after_the_proof_invalidates_it() {
        let mut gate = CommitGate::default();
        gate.on_final_root(true);
        assert!(gate.may_commit());
        gate.on_chunk(); // data landed after the proof was taken
        assert!(
            !gate.may_commit(),
            "a post-proof write must not stay verified"
        );
    }
}
