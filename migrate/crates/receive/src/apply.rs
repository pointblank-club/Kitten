//! The applier (architecture A4.2, implementation I5.2): the receiver's one write path. For each
//! batch of chunks it writes the data, flushes, reads it back and checks the hash, re-hashes the
//! touched index chunks, commits the journal slot, and only then acknowledges — invariant M3's
//! "flush, verify, then record, then acknowledge".
//!
//! Resume (A4.2): on restart it reads the newest valid slot's `acked_seq`; the sender resends from
//! there, and re-applying an already-applied batch is idempotent (it writes the same bytes), so a
//! crash at any point recovers to a target equal to the source.

use std::collections::BTreeSet;

use bootshift_disk::{BlockDevice, DiskError};
use bootshift_migrate_core::{DiskRange, Epoch, FrameSeq, LayoutMap, merkle_root};

use crate::store::{INDEX_CHUNK, Slot, Store, StoreError};

/// A chunk the sender hashed over the bytes it read, for the applier to place and verify.
#[derive(Clone, Debug)]
pub struct IncomingChunk {
    pub seq: FrameSeq,
    pub source: DiskRange,
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

/// Largest plausible jump in the frame sequence between applied batches; a larger jump is
/// treated as an injected frame rather than a real advance.
const SANE_SEQ_ADVANCE: u64 = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("target device I/O")]
    Disk(#[from] DiskError),
    #[error("a chunk maps outside the target layout")]
    Unmapped,
    #[error("the read-back of a written chunk did not match its hash")]
    VerifyFailed,
    #[error("an empty batch")]
    EmptyBatch,
    #[error("a chunk's payload length did not match its declared range")]
    LengthMismatch,
    #[error("the sequence number jumped implausibly far (possible injected frame)")]
    SequenceJumped,
}

pub struct Applier {
    run_id: u64,
    epoch: Epoch,
    layout: LayoutMap,
    /// Number of 1 MiB index chunks on the target.
    target_chunks: u64,
    slot_seq: u64,
    acked: FrameSeq,
}

impl Applier {
    pub fn new(run_id: u64, epoch: Epoch, layout: LayoutMap, target_len: u64) -> Self {
        Applier {
            run_id,
            epoch,
            layout,
            target_chunks: target_len.div_ceil(INDEX_CHUNK),
            slot_seq: 0,
            acked: FrameSeq(0),
        }
    }

    /// An applier that continues an interrupted run from the newest journal slot.
    pub fn resume(
        store: &Store,
        run_id: u64,
        epoch: Epoch,
        layout: LayoutMap,
        target_len: u64,
    ) -> Result<Self, StoreError> {
        let mut applier = Applier::new(run_id, epoch, layout, target_len);
        if let Some(slot) = store.read_newest()? {
            applier.acked = slot.acked_seq;
            applier.slot_seq = slot.seq.saturating_add(1);
        }
        Ok(applier)
    }

    pub fn acked(&self) -> FrameSeq {
        self.acked
    }

    /// Applies one batch and returns the sequence now acknowledged. A batch already covered by
    /// `acked` is a no-op (idempotent resume).
    pub fn apply_batch(
        &mut self,
        store: &mut Store,
        target: &mut dyn BlockDevice,
        batch: &[IncomingChunk],
    ) -> Result<FrameSeq, ApplyError> {
        let last = batch.last().ok_or(ApplyError::EmptyBatch)?.seq;
        if last <= self.acked {
            return Ok(self.acked);
        }
        // An injected frame with an absurd sequence number would jump the acknowledgement
        // watermark and silently skip real chunks. A legitimate batch advances it by roughly its
        // chunk count; reject an implausible jump. (The real fix is authenticating the link; this
        // bounds the damage from an unauthenticated peer.)
        if last.0.saturating_sub(self.acked.0) > SANE_SEQ_ADVANCE {
            return Err(ApplyError::SequenceJumped);
        }
        // 1. Write every chunk to its mapped target offset.
        for chunk in batch {
            let target_range = self.target_range(chunk.source)?;
            // The write length must equal the range the sender declared; otherwise a chunk could
            // write past its own range (the read-back below would still pass, re-reading the same
            // bytes).
            if chunk.data.len() as u64 != target_range.len() {
                return Err(ApplyError::LengthMismatch);
            }
            target.write_at(target_range.start, &chunk.data)?;
        }
        // 2. Flush the target (M3: data durable before it is recorded).
        target.flush()?;
        // 3. Read back and verify against the sender's hash.
        for chunk in batch {
            let target_range = self.target_range(chunk.source)?;
            let mut buf = vec![0u8; chunk.data.len()];
            target.read_at(target_range.start, &mut buf)?;
            if *blake3::hash(&buf).as_bytes() != chunk.hash {
                return Err(ApplyError::VerifyFailed);
            }
        }
        // 4. Re-hash every touched 1 MiB index chunk from the durable target and store it.
        let mut touched = BTreeSet::new();
        for chunk in batch {
            let target_range = self.target_range(chunk.source)?;
            let first = target_range.start / INDEX_CHUNK;
            let last_chunk = (target_range.end - 1) / INDEX_CHUNK;
            for index in first..=last_chunk {
                touched.insert(index);
            }
        }
        for index in &touched {
            let hash = hash_index_chunk(&*target, *index)?;
            store.set_chunk_hash(*index, hash)?;
        }
        // 5. Commit the slot with the new acknowledgement and index root.
        let index_root = self.index_root(store)?;
        store.commit(Slot {
            seq: self.slot_seq,
            run_id: self.run_id,
            epoch: self.epoch,
            state: 0,
            acked_seq: last,
            index_root,
        })?;
        self.slot_seq = self.slot_seq.saturating_add(1);
        self.acked = last;
        Ok(last)
    }

    fn target_range(&self, source: DiskRange) -> Result<DiskRange, ApplyError> {
        self.layout.map_range(source).ok_or(ApplyError::Unmapped)
    }

    fn index_root(&self, store: &Store) -> Result<[u8; 32], ApplyError> {
        let mut leaves = Vec::with_capacity(self.target_chunks as usize);
        for index in 0..self.target_chunks {
            leaves.push(store.chunk_hash(index)?);
        }
        Ok(merkle_root(&leaves))
    }
}

/// blake3 over the bytes of one 1 MiB target chunk, as they are on the disk.
fn hash_index_chunk(target: &dyn BlockDevice, chunk: u64) -> Result<[u8; 32], ApplyError> {
    let start = chunk * INDEX_CHUNK;
    let end = ((chunk + 1) * INDEX_CHUNK).min(target.len_bytes());
    let mut buf = vec![0u8; (end - start) as usize];
    target.read_at(start, &mut buf)?;
    Ok(*blake3::hash(&buf).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootshift_disk::MemDisk;

    const SRC_LEN: u64 = 4 << 20; // 4 MiB

    fn source() -> MemDisk {
        // Deterministic, non-trivial source bytes.
        let mut bytes = vec![0u8; SRC_LEN as usize];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (i as u64).wrapping_mul(2654435761).to_le_bytes()[0];
        }
        MemDisk::from_bytes(bytes, 4096).expect("source")
    }

    fn identity_layout(len: u64) -> LayoutMap {
        LayoutMap::new(vec![(DiskRange::new(0, len), DiskRange::new(0, len))])
    }

    /// Chunks of the source, each `chunk_len` bytes, numbered from 1.
    fn batches(source: &MemDisk, chunk_len: u64, per_batch: usize) -> Vec<Vec<IncomingChunk>> {
        let mut chunks = Vec::new();
        let mut offset = 0u64;
        let mut seq = 1u64;
        while offset < source.len_bytes() {
            let end = (offset + chunk_len).min(source.len_bytes());
            let mut data = vec![0u8; (end - offset) as usize];
            source.read_at(offset, &mut data).expect("read source");
            let hash = *blake3::hash(&data).as_bytes();
            chunks.push(IncomingChunk {
                seq: FrameSeq(seq),
                source: DiskRange::new(offset, end),
                hash,
                data,
            });
            offset = end;
            seq += 1;
        }
        chunks.chunks(per_batch).map(|c| c.to_vec()).collect()
    }

    #[test]
    fn a_full_run_makes_the_target_equal_the_source() {
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        for batch in batches(&source, 256 * 1024, 2) {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, &batch)
                .expect("apply");
        }
        assert_eq!(target.bytes(), source.bytes());
    }

    #[test]
    fn resuming_from_the_journal_re_applies_only_what_was_unacknowledged() {
        let source = source();
        let all = batches(&source, 256 * 1024, 2);
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");

        // Apply the first half, then "restart".
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        for batch in all.iter().take(all.len() / 2) {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, batch)
                .expect("apply");
        }
        let acked_before = applier.acked();

        // Resume and finish; feed everything from acked+1 as the sender would.
        let resumed = {
            let store = Store::new(&mut scratch);
            Applier::resume(&store, 1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN).expect("resume")
        };
        assert_eq!(resumed.acked(), acked_before);
        let mut resumed = resumed;
        for batch in &all {
            let mut store = Store::new(&mut scratch);
            resumed
                .apply_batch(&mut store, &mut target, batch)
                .expect("apply");
        }
        assert_eq!(target.bytes(), source.bytes());
    }
}
