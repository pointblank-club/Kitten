//! `send`: read the source read-only, and stream it to a paired receiver over the link. Against a
//! blank target every chunk differs, so this is a full verified copy; the compare-against-index
//! optimisation (skip chunks the receiver already has) and the tracker-driven delta passes land on
//! top of this same loop. The source is never written (M1).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bootshift_disk::{Access, BlockDevice};
use bootshift_migrate_core::{
    Codec, DiskRange, Epoch, FrameSeq, Message, PROTOCOL_VERSION, PassKind, Receipt, Role, decode,
    encode,
};
use bootshift_migrate_net::{Link, LinkError, TcpLink};

use crate::common::{device_root, open_device, recv_message};

const CHUNK: u64 = 1 << 20;
/// Stop framing new chunks while this many bytes are still queued, so a slow receiver bounds the
/// sender's memory instead of letting the outgoing buffer grow without limit.
const HIGH_WATER: usize = 32 << 20;

pub fn run(source: &Path, to: &str, block_size: u32, epoch: u64) -> Result<()> {
    let src = open_device(source, Access::ReadOnly, block_size)?;
    let src_len = src.len_bytes();
    let epoch = Epoch(epoch);

    let mut link = TcpLink::connect(to).with_context(|| format!("connecting to {to}"))?;
    link.send(encode(
        epoch,
        FrameSeq(0),
        &Message::Hello {
            protocol: PROTOCOL_VERSION,
            role: Role::Sender,
            software: concat!("bootshift-migrate ", env!("CARGO_PKG_VERSION")).to_string(),
            disk_len: src_len,
            block_size: src.logical_block_size(),
            used_digest: [0u8; 32],
        },
    )?);

    let (_, _, hello) = recv_message(&mut link, Duration::from_secs(30))?;
    let Message::Hello {
        disk_len: target_len,
        ..
    } = hello
    else {
        bail!("the receiver did not greet with Hello");
    };
    if target_len < src_len {
        bail!(
            "the target disk ({target_len} bytes) is smaller than the source ({src_len} bytes); \
             refusing before writing a byte"
        );
    }

    let mut acked = 0u64;
    let mut seq = 0u64;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut offset = 0u64;
    while offset < src_len {
        let end = (offset + CHUNK).min(src_len);
        let len = (end - offset) as usize;
        src.read_at(offset, &mut buf[..len])
            .with_context(|| format!("reading the source at offset {offset}"))?;
        let hash = *blake3::hash(&buf[..len]).as_bytes();
        seq += 1;
        link.send(encode(
            epoch,
            FrameSeq(seq),
            &Message::Chunk {
                pass: 1,
                source: DiskRange::new(offset, end),
                hash,
                codec: Codec::None,
                data: buf[..len].to_vec(),
            },
        )?);
        offset = end;
        drain_to_high_water(&mut link, &mut acked)?;
    }

    let last_seq = seq;
    seq += 1;
    link.send(encode(
        epoch,
        FrameSeq(seq),
        &Message::PassEnd {
            pass: 1,
            kind: PassKind::Final,
            sent_bytes: src_len,
            dirty_at_end: 0,
        },
    )?);
    link.flush(Duration::from_secs(120))
        .map_err(|_| anyhow::anyhow!("the link cut while flushing the final chunks"))?;

    wait_for_ack(&mut link, last_seq, Duration::from_secs(600))?;

    // Prove the whole target equals the source before declaring success, exactly as the ebpf path
    // does: send our Merkle root over the source and require a signed receipt whose target root
    // matches. Without this the default path committed on a bare ack, so any coverage gap (a dropped
    // tail, an acknowledgement watermark that legitimately did not advance) would be reported as
    // "verified" when it was not. This is the end-to-end backstop behind the receiver's gap-free ack.
    let source_root = device_root(&src, src_len)?;
    seq += 1;
    link.send(encode(
        epoch,
        FrameSeq(seq),
        &Message::FinalRoot { root: source_root },
    )?);
    link.flush(Duration::from_secs(60))
        .map_err(|_| anyhow::anyhow!("the link cut while sending the final root"))?;
    let receipt = wait_for_receipt(&mut link, Duration::from_secs(120))?;
    check_receipt(&receipt, source_root)?;

    seq += 1;
    link.send(encode(
        epoch,
        FrameSeq(seq),
        &Message::Committed { epoch: epoch.0 },
    )?);
    link.flush(Duration::from_secs(30))
        .map_err(|_| anyhow::anyhow!("the link cut before the commit was delivered"))?;

    println!(
        "send: copied and verified {src_len} bytes to {to}; receipt signed by {}, roots match",
        hex16(&receipt.receiver_key)
    );
    Ok(())
}

/// Waits for the receiver's signed byte-equality receipt after a `FinalRoot`.
fn wait_for_receipt(link: &mut TcpLink, timeout: Duration) -> Result<Receipt> {
    let deadline = Instant::now() + timeout;
    loop {
        match link.recv() {
            Ok(Some(frame)) => {
                if let Ok((_, _, Message::Receipt { receipt })) = decode(&frame) {
                    return Ok(receipt);
                }
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the receiver's receipt");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(LinkError::Cut) => bail!("the link cut before the receipt arrived"),
        }
    }
}

/// Verifies a receipt: valid signature, the source root we sent, and byte-equal roots.
fn check_receipt(receipt: &Receipt, source_root: [u8; 32]) -> Result<()> {
    if !receipt.verify() {
        bail!("the receipt's signature does not verify");
    }
    if receipt.source_root != source_root {
        bail!("the receipt's source root does not match what we sent");
    }
    if !receipt.roots_match() {
        bail!(
            "BYTE-EQUALITY FAILED: the target does not equal the source (source root {}, target root {})",
            hex16(&receipt.source_root),
            hex16(&receipt.target_root)
        );
    }
    Ok(())
}

fn hex16(bytes: &[u8; 32]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads acknowledgements and blocks while the outgoing buffer is above the high-water mark.
fn drain_to_high_water(link: &mut TcpLink, acked: &mut u64) -> Result<()> {
    loop {
        match link.recv() {
            Ok(Some(frame)) => {
                if let Ok((_, _, Message::Ack { acked_seq })) = decode(&frame) {
                    *acked = (*acked).max(acked_seq);
                }
            }
            Ok(None) => {
                if link.pending() <= HIGH_WATER {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(LinkError::Cut) => bail!("the link cut mid-transfer"),
        }
    }
}

fn wait_for_ack(link: &mut TcpLink, target: u64, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match link.recv() {
            Ok(Some(frame)) => {
                if let Ok((_, _, Message::Ack { acked_seq })) = decode(&frame) {
                    if acked_seq >= target {
                        return Ok(());
                    }
                }
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the receiver to acknowledge through seq {target}");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(LinkError::Cut) => bail!("the link cut before the final acknowledgement"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [0x5a; 32];

    fn signed(source_root: [u8; 32], target_root: [u8; 32]) -> Receipt {
        let mut receipt = Receipt::new(
            "run-test".to_string(),
            7,
            1 << 20,
            1 << 20,
            1u32 << 20,
            source_root,
            target_root,
            "t".to_string(),
            "t".to_string(),
            0,
        );
        receipt.sign_with_seed(&SEED);
        receipt
    }

    #[test]
    fn check_receipt_requires_signature_our_source_root_and_equal_roots() {
        let root = [7u8; 32];
        // Round-trip success: signed, our source root, byte-equal roots.
        assert!(check_receipt(&signed(root, root), root).is_ok());
        // Tampered / short-coverage target: source and target roots differ.
        assert!(check_receipt(&signed(root, [9u8; 32]), root).is_err());
        // A receipt for a different source than the one we sent.
        assert!(check_receipt(&signed([1u8; 32], [1u8; 32]), root).is_err());
    }
}

#[cfg(feature = "ebpf")]
mod live {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use bootshift_disk::{Access, BlockDevice};
    use bootshift_migrate_core::{
        Advice, Codec, ConvergenceModel, DirtySet, DiskRange, Epoch, FrameSeq, MAX_EXTENT, Message,
        PROTOCOL_VERSION, PassKind, PassStats, Role, encode,
    };
    use bootshift_migrate_net::{Link, TcpLink};
    use bootshift_migrate_track::{Drained, EbpfTracker, Tracker};

    use super::{CHUNK, check_receipt, drain_to_high_water, hex16, wait_for_ack, wait_for_receipt};
    use crate::common::{device_root, freeze, open_device, recv_message, thaw};

    const MAX_PASSES: usize = 40;
    const QUIET_PASSES: usize = 8;

    /// A live, tracker-driven migration: a bulk pass, then delta passes that send only what the
    /// eBPF tracker saw change, until the convergence model says a cutover is cheap. Then the source
    /// filesystem is frozen (M6), the last changes are drained and sent, and a signed byte-equality
    /// receipt is exchanged before the commit. The source disk is only ever read (M1).
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        source: &Path,
        track_dev: &Path,
        to: &str,
        block_size: u32,
        epoch: u64,
        budget_ms: u64,
        freeze_mount: Option<&str>,
    ) -> Result<()> {
        let src = open_device(source, Access::ReadOnly, block_size)?;
        let src_len = src.len_bytes();
        let epoch_e = Epoch(epoch);

        let mut tracker =
            EbpfTracker::open(track_dev, src_len).context("attaching the eBPF tracker")?;
        tracker
            .confirm_live()
            .map_err(|e| anyhow::anyhow!("the tracker is not live: {e}"))?;

        let mut link = TcpLink::connect(to).with_context(|| format!("connecting to {to}"))?;
        link.send(encode(
            epoch_e,
            FrameSeq(0),
            &Message::Hello {
                protocol: PROTOCOL_VERSION,
                role: Role::Sender,
                software: concat!("bootshift-migrate ", env!("CARGO_PKG_VERSION")).to_string(),
                disk_len: src_len,
                block_size: src.logical_block_size(),
                used_digest: [0u8; 32],
            },
        )?);
        let (_, _, hello) = recv_message(&mut link, Duration::from_secs(30))?;
        let Message::Hello {
            disk_len: target_len,
            ..
        } = hello
        else {
            bail!("the receiver did not greet with Hello");
        };
        if target_len < src_len {
            bail!("the target ({target_len} bytes) is smaller than the source ({src_len} bytes)");
        }

        let mut seq = 0u64;
        let mut dirty = DirtySet::from_ranges([DiskRange::new(0, src_len)]);
        let mut model = ConvergenceModel::new(
            Duration::from_millis(budget_ms),
            Duration::from_millis(50),
            false,
        );
        let mut kind = PassKind::Bulk;

        // Converge: send, see what changed, repeat until a cutover fits the budget.
        for pass in 0..MAX_PASSES {
            let generation = tracker
                .flip()
                .map_err(|e| anyhow::anyhow!("tracker flip: {e}"))?;
            let extents = dirty.take_extents(MAX_EXTENT);
            let sent = send_pass(&mut link, &src, &extents, &mut seq, epoch_e, kind)?;
            match tracker
                .drain(generation)
                .map_err(|e| anyhow::anyhow!("tracker drain: {e}"))?
            {
                Drained::Ranges(changed) => dirty.extend(changed),
                Drained::Broken(reason) => {
                    eprintln!(
                        "send: tracking broke ({reason:?}); next pass resends the whole disk"
                    );
                    dirty = DirtySet::from_ranges([DiskRange::new(0, src_len)]);
                }
            }
            model.record(PassStats {
                kind,
                sent_bytes: sent.0,
                duration: sent.2,
                dirty_at_end: dirty.total_bytes(),
                rate: sent.1,
            });
            kind = PassKind::Delta;
            println!(
                "send: pass {pass} sent {} KiB, {} KiB still dirty",
                sent.0 / 1024,
                dirty.total_bytes() / 1024
            );
            match model.advise() {
                Advice::Ready { predicted_downtime } => {
                    println!(
                        "send: ready to cut over (predicted data downtime {} ms)",
                        predicted_downtime.as_millis()
                    );
                    break;
                }
                Advice::Continue => continue,
                other => eprintln!("send: convergence says {other:?}; continuing"),
            }
        }

        // Quiesce and cut over.
        if let Some(mount) = freeze_mount {
            freeze(mount).with_context(|| format!("freezing {mount}"))?;
        }
        let cut_started = Instant::now();
        let quiet = match drain_until_quiet(
            &mut link,
            &src,
            &mut tracker,
            &mut dirty,
            &mut seq,
            epoch_e,
            src_len,
        ) {
            Ok(quiet) => quiet,
            Err(e) => {
                if let Some(mount) = freeze_mount {
                    let _ = thaw(mount);
                }
                return Err(e);
            }
        };
        if !quiet {
            if let Some(mount) = freeze_mount {
                let _ = thaw(mount);
            }
            bail!("the source kept changing after the freeze; cutover aborted (M6)");
        }

        // Exchange the signed byte-equality receipt before committing.
        let source_root = device_root(&src, src_len)?;
        link.send(encode(
            epoch_e,
            FrameSeq(seq + 1),
            &Message::FinalRoot { root: source_root },
        )?);
        link.flush(Duration::from_secs(60))
            .map_err(|_| anyhow::anyhow!("the link cut while sending the final root"))?;

        let receipt = wait_for_receipt(&mut link, Duration::from_secs(120));
        let receipt = match receipt {
            Ok(receipt) => receipt,
            Err(e) => {
                if let Some(mount) = freeze_mount {
                    let _ = thaw(mount);
                }
                return Err(e);
            }
        };
        let verdict = check_receipt(&receipt, source_root);
        if let Some(mount) = freeze_mount {
            thaw(mount).with_context(|| format!("thawing {mount}"))?;
        }
        verdict?;

        seq += 2;
        link.send(encode(
            epoch_e,
            FrameSeq(seq),
            &Message::Committed { epoch },
        )?);
        link.flush(Duration::from_secs(30))
            .map_err(|_| anyhow::anyhow!("the link cut before the commit was delivered"))?;

        println!(
            "send: migration committed. Data downtime {} ms. Receipt signed by {}, roots match.",
            cut_started.elapsed().as_millis(),
            hex16(&receipt.receiver_key)
        );
        Ok(())
    }

    /// After the freeze, drains and sends until a pass has nothing to send and nothing new is dirty:
    /// the source is then truly quiesced and the target matches it.
    #[allow(clippy::too_many_arguments)]
    fn drain_until_quiet(
        link: &mut TcpLink,
        src: &dyn BlockDevice,
        tracker: &mut EbpfTracker,
        dirty: &mut DirtySet,
        seq: &mut u64,
        epoch: Epoch,
        src_len: u64,
    ) -> Result<bool> {
        for _ in 0..QUIET_PASSES {
            let generation = tracker
                .flip()
                .map_err(|e| anyhow::anyhow!("tracker flip: {e}"))?;
            let extents = dirty.take_extents(MAX_EXTENT);
            let had_work = !extents.is_empty();
            if had_work {
                send_pass(link, src, &extents, seq, epoch, PassKind::Delta)?;
            }
            let drained = tracker
                .drain(generation)
                .map_err(|e| anyhow::anyhow!("tracker drain: {e}"))?;
            match drained {
                Drained::Ranges(changed) => {
                    let new_work = !changed.is_empty();
                    dirty.extend(changed);
                    if !had_work && !new_work {
                        return Ok(true);
                    }
                }
                Drained::Broken(_) => {
                    *dirty = DirtySet::from_ranges([DiskRange::new(0, src_len)]);
                }
            }
        }
        Ok(false)
    }

    /// Sends every chunk of the given source extents, then a PassEnd, and waits for the receiver to
    /// acknowledge them. Returns (bytes, bytes-per-second, duration).
    fn send_pass(
        link: &mut TcpLink,
        src: &dyn BlockDevice,
        extents: &[DiskRange],
        seq: &mut u64,
        epoch: Epoch,
        kind: PassKind,
    ) -> Result<(u64, u64, Duration)> {
        let start = Instant::now();
        if extents.is_empty() {
            return Ok((0, 1, start.elapsed()));
        }
        let mut buf = vec![0u8; CHUNK as usize];
        let mut bytes = 0u64;
        let mut acked = 0u64;
        let mut last_seq = *seq;
        for range in extents {
            let mut offset = range.start;
            while offset < range.end {
                let end = (offset + CHUNK).min(range.end);
                let len = (end - offset) as usize;
                src.read_at(offset, &mut buf[..len])
                    .with_context(|| format!("reading the source at {offset}"))?;
                let hash = *blake3::hash(&buf[..len]).as_bytes();
                *seq += 1;
                last_seq = *seq;
                link.send(encode(
                    epoch,
                    FrameSeq(*seq),
                    &Message::Chunk {
                        pass: 1,
                        source: DiskRange::new(offset, end),
                        hash,
                        codec: Codec::None,
                        data: buf[..len].to_vec(),
                    },
                )?);
                bytes += len as u64;
                offset = end;
                drain_to_high_water(link, &mut acked)?;
            }
        }
        *seq += 1;
        link.send(encode(
            epoch,
            FrameSeq(*seq),
            &Message::PassEnd {
                pass: 1,
                kind,
                sent_bytes: bytes,
                dirty_at_end: 0,
            },
        )?);
        link.flush(Duration::from_secs(120))
            .map_err(|_| anyhow::anyhow!("the link cut while flushing a pass"))?;
        wait_for_ack(link, last_seq, Duration::from_secs(300))?;
        let duration = start.elapsed();
        let secs = duration.as_secs_f64();
        let rate = if secs > 0.0 {
            (bytes as f64 / secs) as u64
        } else {
            bytes
        };
        Ok((bytes, rate.max(1), duration))
    }
}

#[cfg(feature = "ebpf")]
pub use live::run as run_live;
