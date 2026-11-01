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
            // Always continue the journal slot numbering past the newest slot so our commits win.
            applier.slot_seq = slot.seq.saturating_add(1);
            // Only inherit the acknowledgement watermark if the slot belongs to THIS run. A reused
            // scratch device can hold a previous run's slot; trusting its watermark would make us
            // skip writing this run's chunks and silently leave stale data on the target. On a
            // mismatch, start from zero and re-apply everything.
            // NOTE: needs the two-VM migrate-rig to validate the reused-scratch path end to end.
            if slot.run_id == run_id && slot.epoch.0 == epoch.0 {
                applier.acked = slot.acked_seq;
            }
        }
        Ok(applier)
    }

    pub fn acked(&self) -> FrameSeq {
        self.acked
    }

    /// Account for a control frame (a PassEnd between passes) that consumed a sequence number but
    /// carried no chunk. The sender increments one shared seq counter for both chunks and the
    /// trailing PassEnd, so after a pass acknowledged through K the PassEnd takes K+1 and the next
    /// pass's first chunk is K+2. Without bridging K+1 here, the coverage walk in `apply_batch`
    /// would stall at that gap and silently discard every following pass. Only advances across the
    /// single seq immediately after the watermark, so it can never skip a real (data) seq.
    /// NOTE: the two-VM migrate-rig is the authoritative validation of multi-pass convergence.
    pub fn cover_control_seq(&mut self, seq: FrameSeq) {
        if seq.0 == self.acked.0.saturating_add(1) {
            self.acked = seq;
        }
    }

    /// Applies one batch and returns the sequence now acknowledged. A batch already covered by
    /// `acked` is a no-op (idempotent resume).
    pub fn apply_batch(
        &mut self,
        store: &mut Store,
        target: &mut dyn BlockDevice,
        batch: &[IncomingChunk],
    ) -> Result<FrameSeq, ApplyError> {
        if batch.is_empty() {
            return Err(ApplyError::EmptyBatch);
        }
        // The acknowledgement watermark may only advance across sequence numbers this batch actually
        // covers, with no gap. It becomes the largest `M` such that every seq in `(acked, M]` is
        // present in the batch. Without this, an injected frame whose seq merely exceeds `acked`
        // would push the watermark past real, still-unwritten chunks, leaving their target ranges
        // stale while the sender treats them as durable (a silent hole).
        //
        // Duplicates, reordering and replays within a batch are fine: a lossy-link reconnect makes
        // the sender replay earlier chunks into the same unflushed batch, so the batch can legitimately
        // contain a backward jump (e.g. 1,2,1,2,3,4). They are re-written idempotently and the
        // watermark simply stops at the first missing seq.
        //
        // NOTE: this does not authenticate the peer (the unauthenticated-transport gap is tracked
        // separately); an attacker who supplies every intermediate chunk contiguously can still write
        // its own bytes. It stops the forward-jump hole. Full end-to-end validation still needs the
        // two-VM migrate-rig against a live sender.
        let mut present: BTreeSet<u64> = BTreeSet::new();
        for chunk in batch {
            if chunk.seq.0 > self.acked.0 {
                present.insert(chunk.seq.0);
            }
        }
        let mut new_acked = self.acked.0;
        while present.contains(&new_acked.saturating_add(1)) {
            new_acked = new_acked.saturating_add(1);
        }
        if new_acked == self.acked.0 {
            // Nothing new is contiguously covered: a fully re-sent (already acknowledged) batch, or
            // an injected frame that does not extend the run. Re-ack without advancing or writing.
            return Ok(self.acked);
        }
        // Defence in depth: a single legitimate batch never advances the watermark by a wild amount.
        if new_acked.saturating_sub(self.acked.0) > SANE_SEQ_ADVANCE {
            return Err(ApplyError::SequenceJumped);
        }
        let new_acked = FrameSeq(new_acked);
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
            acked_seq: new_acked,
            index_root,
        })?;
        self.slot_seq = self.slot_seq.saturating_add(1);
        self.acked = new_acked;
        Ok(new_acked)
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

    #[test]
    fn resume_ignores_a_journal_slot_from_a_different_run() {
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");

        // Run 1 applies and commits slots on this scratch device.
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        for batch in batches(&source, 256 * 1024, 2).iter().take(2) {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, batch)
                .expect("apply");
        }
        assert!(applier.acked().0 > 0, "run 1 should have advanced its ack");

        // A new run reuses the same scratch device. It must NOT inherit run 1's watermark, or it
        // would skip writing its own chunks and leave stale data on the target.
        let store = Store::new(&mut scratch);
        let resumed_other = Applier::resume(&store, 2, Epoch(7), identity_layout(SRC_LEN), SRC_LEN)
            .expect("resume other run");
        assert_eq!(
            resumed_other.acked(),
            FrameSeq(0),
            "a different run_id must start from zero"
        );

        // A different epoch is likewise not resumed.
        let store = Store::new(&mut scratch);
        let resumed_epoch =
            Applier::resume(&store, 1, Epoch(99), identity_layout(SRC_LEN), SRC_LEN)
                .expect("resume other epoch");
        assert_eq!(
            resumed_epoch.acked(),
            FrameSeq(0),
            "a different epoch must start from zero"
        );

        // The matching run resumes as before.
        let store = Store::new(&mut scratch);
        let resumed_same = Applier::resume(&store, 1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN)
            .expect("resume same run");
        assert!(
            resumed_same.acked().0 > 0,
            "the same run_id/epoch must resume its watermark"
        );
    }

    #[test]
    fn a_pass_end_seq_gap_does_not_discard_the_next_pass() {
        // Models the real multi-pass protocol: the sender's PassEnd consumes a seq that is never
        // sent as a chunk, so the next pass's first chunk is two past the last acknowledged chunk.
        // Without cover_control_seq bridging the PassEnd seq, the coverage walk would stall and the
        // whole delta pass would be silently dropped (the convergence regression).
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        let chunks = single_chunks(&source, 256 * 1024); // seqs 1..=16

        // Bulk pass: apply seqs 1..=2, then its PassEnd consumes seq 3.
        {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, &chunks[0..2])
                .expect("bulk pass");
        }
        assert_eq!(applier.acked(), FrameSeq(2));
        applier.cover_control_seq(FrameSeq(3)); // the bulk PassEnd's seq
        assert_eq!(
            applier.acked(),
            FrameSeq(3),
            "the PassEnd seq must be bridged"
        );

        // Delta pass: its first chunk is seq 4 (never 3). Re-cover source blocks 2 and 3 at seqs 4,5.
        let delta: Vec<IncomingChunk> = chunks[2..4]
            .iter()
            .enumerate()
            .map(|(i, c)| IncomingChunk {
                seq: FrameSeq(4 + i as u64),
                source: c.source,
                hash: c.hash,
                data: c.data.clone(),
            })
            .collect();
        let acked = {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, &delta)
                .expect("delta pass")
        };
        assert_eq!(
            acked,
            FrameSeq(5),
            "the delta pass must be applied, not discarded at the PassEnd gap"
        );
    }

    /// Individual single-chunk "batches" of the source, numbered from 1.
    fn single_chunks(source: &MemDisk, chunk_len: u64) -> Vec<IncomingChunk> {
        batches(source, chunk_len, 1)
            .into_iter()
            .flatten()
            .collect()
    }

    #[test]
    fn a_frame_skipping_ahead_of_the_ack_does_not_advance_the_watermark() {
        // The watermark-poisoning case: a frame whose seq is beyond acked + 1 must not advance the
        // acknowledgement past the chunks it skipped, and must not write to the target.
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let before = target.bytes().to_vec();
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        let chunks = single_chunks(&source, 256 * 1024);
        // acked starts at 0, so acked + 1 == 1; a batch holding only seq 6 skips 1..=5.
        let jumped = vec![chunks[5].clone()];
        let mut store = Store::new(&mut scratch);
        let acked = applier
            .apply_batch(&mut store, &mut target, &jumped)
            .expect("no-op, not an error");
        assert_eq!(
            acked,
            FrameSeq(0),
            "the watermark must not move past the gap"
        );
        assert_eq!(
            target.bytes(),
            before.as_slice(),
            "the skipped-ahead chunk must not be written"
        );
    }

    #[test]
    fn a_batch_with_an_internal_gap_acknowledges_only_the_contiguous_prefix() {
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        let chunks = single_chunks(&source, 256 * 1024);
        // seq 1 then seq 3: starts at acked + 1 but skips seq 2; only seq 1 may be acknowledged.
        let gapped = vec![chunks[0].clone(), chunks[2].clone()];
        let mut store = Store::new(&mut scratch);
        let acked = applier
            .apply_batch(&mut store, &mut target, &gapped)
            .expect("apply the covered prefix");
        assert_eq!(acked, FrameSeq(1));
    }

    #[test]
    fn a_reconnect_replaying_earlier_chunks_in_one_batch_is_accepted() {
        // After a link cut the sender replays into the receiver's still-unflushed batch, so one
        // batch can contain a backward jump like 1,2,1,2,3,4. The watermark must still reach 4.
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        let c = single_chunks(&source, 256 * 1024);
        let replayed = vec![
            c[0].clone(),
            c[1].clone(),
            c[0].clone(),
            c[1].clone(),
            c[2].clone(),
            c[3].clone(),
        ];
        let mut store = Store::new(&mut scratch);
        let acked = applier
            .apply_batch(&mut store, &mut target, &replayed)
            .expect("apply replayed batch");
        assert_eq!(acked, FrameSeq(4));
    }

    #[test]
    fn a_resend_that_re_includes_acked_chunks_is_accepted() {
        // A lossy-link resume may replay from before our ack; re-including acked chunks is fine.
        let source = source();
        let mut target = MemDisk::zeroed(SRC_LEN as usize, 4096).expect("target");
        let mut scratch = MemDisk::zeroed(32 << 20, 4096).expect("scratch");
        let mut applier = Applier::new(1, Epoch(7), identity_layout(SRC_LEN), SRC_LEN);
        let chunks = single_chunks(&source, 256 * 1024);
        // Apply seq 1..=2.
        {
            let mut store = Store::new(&mut scratch);
            applier
                .apply_batch(&mut store, &mut target, &chunks[0..2])
                .expect("apply 1..=2");
        }
        // Resend seq 2..=4 (re-includes the acked seq 2, then extends contiguously).
        {
            let mut store = Store::new(&mut scratch);
            let acked = applier
                .apply_batch(&mut store, &mut target, &chunks[1..4])
                .expect("apply 2..=4");
            assert_eq!(acked, FrameSeq(4));
        }
    }
}
