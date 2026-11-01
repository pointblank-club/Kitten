//! Helpers shared by `send` and `receive`: opening a disk or image, and waiting for one message.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bootshift_disk::{Access, BlockDevice, RealDevice};
use bootshift_migrate_core::{Epoch, FrameSeq, Message, decode, merkle_root};
use bootshift_migrate_net::{Link, LinkError, TcpLink};

/// The chunk size the receipt's Merkle tree is built over (1 MiB), matching the receiver's index.
pub const RECEIPT_CHUNK: u64 = 1 << 20;

/// Opens `path` as a block device (block size from the kernel) or, if it is a regular file, as a
/// disk image with `block_size`-byte blocks. The source is only ever opened read-only (M1).
pub fn open_device(path: &Path, access: Access, block_size: u32) -> Result<RealDevice> {
    let meta = std::fs::metadata(path).with_context(|| format!("looking at {}", path.display()))?;
    if meta.file_type().is_block_device() {
        RealDevice::open(path, access)
            .with_context(|| format!("opening block device {}", path.display()))
    } else {
        RealDevice::open_image(path, access, block_size)
            .with_context(|| format!("opening image {}", path.display()))
    }
}

/// Blocks until one message arrives, timing out or failing if the link cuts first.
pub fn recv_message(link: &mut TcpLink, timeout: Duration) -> Result<(Epoch, FrameSeq, Message)> {
    let deadline = Instant::now() + timeout;
    loop {
        match link.recv() {
            Ok(Some(frame)) => return decode(&frame).context("decoding a frame from the peer"),
            Ok(None) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the peer");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(LinkError::Cut) => bail!("the link cut while waiting for the peer"),
        }
    }
}

/// The blake3 Merkle root over a device's 1 MiB chunks: the leaf set a receipt proves equal on both
/// sides. The sender runs it over the source, the receiver over the target.
pub fn device_root(device: &dyn BlockDevice, len: u64) -> Result<[u8; 32]> {
    let chunks = len.div_ceil(RECEIPT_CHUNK);
    let mut leaves = Vec::with_capacity(chunks as usize);
    let mut buf = vec![0u8; RECEIPT_CHUNK as usize];
    for index in 0..chunks {
        let start = index * RECEIPT_CHUNK;
        let end = ((index + 1) * RECEIPT_CHUNK).min(len);
        let slice = &mut buf[..(end - start) as usize];
        device
            .read_at(start, slice)
            .with_context(|| format!("reading chunk at {start} for the receipt root"))?;
        leaves.push(*blake3::hash(slice).as_bytes());
    }
    Ok(merkle_root(&leaves))
}

/// An ISO-8601 UTC timestamp for the receipt, from `date -u`, or a unix-seconds fallback.
pub fn now_iso() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("unix:{secs}")
        })
}

/// Freezes a mounted filesystem so the final pass reads a consistent, unchanging image (M6).
#[cfg(feature = "ebpf")]
pub fn freeze(mountpoint: &str) -> Result<()> {
    run_quiesce("--freeze", mountpoint)
}

/// Thaws a frozen filesystem. Safe to call even if it was never frozen.
#[cfg(feature = "ebpf")]
pub fn thaw(mountpoint: &str) -> Result<()> {
    run_quiesce("--unfreeze", mountpoint)
}

#[cfg(feature = "ebpf")]
fn run_quiesce(flag: &str, mountpoint: &str) -> Result<()> {
    let status = Command::new("fsfreeze")
        .arg(flag)
        .arg(mountpoint)
        .status()
        .with_context(|| format!("running fsfreeze {flag} {mountpoint}"))?;
    if !status.success() {
        bail!("fsfreeze {flag} {mountpoint} failed with {status}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use bootshift_disk::MemDisk;

    #[test]
    fn device_root_matches_equal_disks_and_differs_when_tampered() {
        // Three full receipt chunks of deterministic bytes.
        let bytes: Vec<u8> = (0..(3 * RECEIPT_CHUNK)).map(|i| (i % 251) as u8).collect();
        let source = MemDisk::from_bytes(bytes.clone(), 4096).expect("source");
        let target = MemDisk::from_bytes(bytes.clone(), 4096).expect("target");
        let len = source.len_bytes();

        let source_root = device_root(&source, len).expect("source root");
        let target_root = device_root(&target, len).expect("target root");
        assert_eq!(
            source_root, target_root,
            "byte-equal disks must share a root"
        );

        // A single flipped byte in the last chunk must change the root: this is what makes a
        // tampered or short-coverage target fail the receipt's byte-equality check.
        let mut tampered = bytes;
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let other = MemDisk::from_bytes(tampered, 4096).expect("tampered");
        let other_root = device_root(&other, len).expect("tampered root");
        assert_ne!(source_root, other_root, "a tampered target must differ");
    }

    #[test]
    fn device_root_over_the_source_range_matches_a_larger_target() {
        // MIG-01: the receiver must hash the target over exactly the source length, not the whole
        // (possibly larger) target device, or a larger target would never verify.
        let src_len = 2 * RECEIPT_CHUNK;
        let bytes: Vec<u8> = (0..src_len).map(|i| (i % 251) as u8).collect();
        let source = MemDisk::from_bytes(bytes.clone(), 4096).expect("source");

        // A larger target: identical over [0, src_len), with an extra nonzero tail.
        let mut target_bytes = bytes;
        target_bytes.resize((src_len + RECEIPT_CHUNK) as usize, 0xab);
        let target = MemDisk::from_bytes(target_bytes, 4096).expect("target");

        let source_root = device_root(&source, src_len).expect("source root");
        let over_source = device_root(&target, src_len).expect("target root over source range");
        assert_eq!(
            source_root, over_source,
            "a larger target must verify over the source range"
        );

        // Hashing the whole larger target is the regression: it would never match.
        let over_whole = device_root(&target, target.len_bytes()).expect("full target root");
        assert_ne!(
            source_root, over_whole,
            "hashing the whole larger target is the false negative we fixed"
        );
    }
}
