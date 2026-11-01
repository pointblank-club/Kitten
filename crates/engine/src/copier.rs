//! Moves a partition's bytes in place, chunk by chunk, so a power cut at any moment can be
//! resumed from the journal.

mod job;

use std::ops::Range;

use bootshift_core::journal::Progress;
use bootshift_disk::{BlockDevice, DiskError, Journal, JournalError};

pub use job::{ChunkPlan, CopyScope, InvalidJob, MAX_CHUNK, MoveJob};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyOptions {
    pub max_chunk: u64,
    /// Copy past unreadable source blocks, writing zeros in their place. Only ever set after
    /// the user has been told exactly which blocks are lost and has agreed.
    pub accept_unreadable: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            max_chunk: MAX_CHUNK,
            accept_unreadable: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error(transparent)]
    InvalidJob(#[from] InvalidJob),
    #[error("the block at byte {offset} can't be read")]
    Unreadable { offset: u64 },
    #[error("data written at byte {offset} did not read back the same")]
    VerifyFailed { offset: u64 },
    #[error("chunk {0} was copied but its progress was never committed")]
    UncommittedChunk(u64),
    #[error(transparent)]
    Disk(#[from] DiskError),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// A copied, flushed and verified chunk whose progress must be committed before the next one.
#[derive(Debug, PartialEq, Eq)]
pub struct ChunkDone {
    pub next_chunk: u64,
    pub hash_chain: [u8; 32],
}

/// Receives progress while data moves: the recovery screen and, later, the GUI. Keep
/// implementations quick; they run between a chunk's journal commit and the next read.
pub trait ProgressSink {
    /// A new operation started, `index` of `count`, described in plain words.
    fn operation(&mut self, index: u32, count: u32, what: &str);
    /// `done` of `total` bytes of the current move are copied, verified and journaled.
    fn copied(&mut self, done: u64, total: u64);
}

/// Discards progress.
pub struct SilentProgress;

impl ProgressSink for SilentProgress {
    fn operation(&mut self, _: u32, _: u32, _: &str) {}
    fn copied(&mut self, _: u64, _: u64) {}
}

pub struct MoveOutcome {
    pub hash_chain: [u8; 32],
    /// Source blocks that couldn't be read and were written as zeros.
    pub unreadable: Vec<u64>,
}

/// Copies one chunk at a time and won't start the next until the last is acknowledged.
///
/// The next chunk's destination may overlap this chunk's source. If it were written before this
/// chunk's progress was durable, a resume would copy this chunk again from overwritten data.
pub struct ChunkCopier {
    plan: ChunkPlan,
    options: CopyOptions,
    next_chunk: u64,
    hash_chain: [u8; 32],
    awaiting_commit: bool,
    unreadable: Vec<u64>,
    done_bytes: u64,
    total_bytes: u64,
}

impl ChunkCopier {
    pub fn resume(
        job: MoveJob,
        block_size: u32,
        next_chunk: u64,
        hash_chain: [u8; 32],
        options: CopyOptions,
    ) -> Result<Self, CopyError> {
        let plan = ChunkPlan::new(job, block_size, options.max_chunk)?;
        let bytes_in = |range: std::ops::Range<u64>| {
            range
                .map(|index| total_len(&plan.pieces(index)) as u64)
                .sum()
        };
        let done_bytes = bytes_in(0..next_chunk.min(plan.chunk_count()));
        let total_bytes =
            done_bytes + bytes_in(next_chunk.min(plan.chunk_count())..plan.chunk_count());
        Ok(Self {
            plan,
            options,
            next_chunk,
            hash_chain,
            awaiting_commit: false,
            unreadable: Vec::new(),
            done_bytes,
            total_bytes,
        })
    }

    /// Bytes of data copied so far, and in total, for this move.
    pub fn progress(&self) -> (u64, u64) {
        (self.done_bytes, self.total_bytes)
    }

    /// Copies, flushes and verifies the next chunk that holds data. Returns `None` once every
    /// chunk is done.
    pub fn copy_next_chunk<D: BlockDevice + ?Sized>(
        &mut self,
        device: &mut D,
    ) -> Result<Option<ChunkDone>, CopyError> {
        if self.awaiting_commit {
            return Err(CopyError::UncommittedChunk(self.next_chunk - 1));
        }
        while self.next_chunk < self.plan.chunk_count() {
            let index = self.next_chunk;
            let pieces = self.plan.pieces(index);
            if pieces.is_empty() {
                self.next_chunk += 1;
                continue;
            }

            let data = self.read_source(device, &pieces)?;
            self.write_destination(device, &pieces, &data)?;
            device.flush()?;
            self.verify_destination(device, &pieces, &data)?;

            self.hash_chain = extend_chain(self.hash_chain, index, &data);
            self.next_chunk = index + 1;
            self.done_bytes += data.len() as u64;
            self.awaiting_commit = true;
            return Ok(Some(ChunkDone {
                next_chunk: self.next_chunk,
                hash_chain: self.hash_chain,
            }));
        }
        Ok(None)
    }

    pub fn acknowledge(&mut self, done: ChunkDone) {
        debug_assert_eq!(done.next_chunk, self.next_chunk);
        self.awaiting_commit = false;
    }

    fn read_source<D: BlockDevice + ?Sized>(
        &mut self,
        device: &D,
        pieces: &[Range<u64>],
    ) -> Result<Vec<u8>, CopyError> {
        let source = self.plan.job().source;
        let block = u64::from(device.logical_block_size());
        let mut data = vec![0; total_len(pieces)];
        let mut filled = 0;
        for piece in pieces {
            let len = (piece.end - piece.start) as usize;
            let buf = &mut data[filled..filled + len];
            match device.read_at(source + piece.start, buf) {
                Ok(()) => {}
                Err(DiskError::Io { .. }) => {
                    let lost = read_block_by_block(device, source + piece.start, buf, block)?;
                    // The bulk read failed, but the per-block retry may have recovered every block
                    // (a transient controller/link fault), leaving `lost` empty and the data good.
                    // Only a genuinely unreadable block is fatal. Indexing `lost[0]` unconditionally
                    // here would panic — and this runs as PID 1 in the recovery system.
                    if !self.options.accept_unreadable {
                        if let Some(&offset) = lost.first() {
                            return Err(CopyError::Unreadable { offset });
                        }
                    }
                    self.unreadable.extend(lost);
                }
                Err(error) => return Err(error.into()),
            }
            filled += len;
        }
        Ok(data)
    }

    fn write_destination<D: BlockDevice + ?Sized>(
        &self,
        device: &mut D,
        pieces: &[Range<u64>],
        data: &[u8],
    ) -> Result<(), CopyError> {
        let destination = self.plan.job().destination;
        let mut written = 0;
        for piece in pieces {
            let len = (piece.end - piece.start) as usize;
            device.write_at(destination + piece.start, &data[written..written + len])?;
            written += len;
        }
        Ok(())
    }

    fn verify_destination<D: BlockDevice + ?Sized>(
        &self,
        device: &D,
        pieces: &[Range<u64>],
        expected: &[u8],
    ) -> Result<(), CopyError> {
        let destination = self.plan.job().destination;
        let actual = read_pieces(device, destination, pieces)?;
        match actual.iter().zip(expected).position(|(a, b)| a != b) {
            None => Ok(()),
            Some(index) => Err(CopyError::VerifyFailed {
                offset: destination + offset_in_pieces(pieces, index),
            }),
        }
    }
}

/// Moves `job` on `device`, committing progress to `journal` after every chunk. `start` is the
/// progress to resume from; its other fields are carried into every commit.
pub fn move_range<D: BlockDevice + ?Sized>(
    device: &mut D,
    journal: &mut Journal,
    job: MoveJob,
    start: &Progress,
    options: CopyOptions,
) -> Result<MoveOutcome, CopyError> {
    move_range_with(device, journal, job, start, options, &mut SilentProgress)
}

/// [`move_range`], reporting each committed chunk to `sink`.
pub fn move_range_with<D: BlockDevice + ?Sized>(
    device: &mut D,
    journal: &mut Journal,
    job: MoveJob,
    start: &Progress,
    options: CopyOptions,
    sink: &mut dyn ProgressSink,
) -> Result<MoveOutcome, CopyError> {
    let block_size = device.logical_block_size();
    let mut copier =
        ChunkCopier::resume(job, block_size, start.next_chunk, start.hash_chain, options)?;
    while let Some(done) = copier.copy_next_chunk(device)? {
        let progress = Progress {
            next_chunk: done.next_chunk,
            hash_chain: done.hash_chain,
            ..start.clone()
        };
        journal.commit(device, &progress)?;
        copier.acknowledge(done);
        let (done_bytes, total_bytes) = copier.progress();
        sink.copied(done_bytes, total_bytes);
    }
    Ok(MoveOutcome {
        hash_chain: copier.hash_chain,
        unreadable: copier.unreadable,
    })
}

/// Recomputes the hash chain from the destination. It matches the chain the copier committed
/// only if every copied byte is still in place.
pub fn verify_move<D: BlockDevice + ?Sized>(
    device: &D,
    job: MoveJob,
    options: &CopyOptions,
) -> Result<[u8; 32], CopyError> {
    let plan = ChunkPlan::new(job, device.logical_block_size(), options.max_chunk)?;
    let destination = plan.job().destination;
    let mut chain = [0; 32];
    for index in 0..plan.chunk_count() {
        let pieces = plan.pieces(index);
        if !pieces.is_empty() {
            chain = extend_chain(chain, index, &read_pieces(device, destination, &pieces)?);
        }
    }
    Ok(chain)
}

fn extend_chain(chain: [u8; 32], index: u64, data: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&chain);
    hasher.update(&index.to_le_bytes());
    hasher.update(data);
    *hasher.finalize().as_bytes()
}

fn read_pieces<D: BlockDevice + ?Sized>(
    device: &D,
    base: u64,
    pieces: &[Range<u64>],
) -> Result<Vec<u8>, CopyError> {
    let mut data = vec![0; total_len(pieces)];
    let mut filled = 0;
    for piece in pieces {
        let len = (piece.end - piece.start) as usize;
        device.read_at(base + piece.start, &mut data[filled..filled + len])?;
        filled += len;
    }
    Ok(data)
}

/// Reads what it can one block at a time, leaving unreadable blocks zeroed, and returns their
/// offsets.
fn read_block_by_block<D: BlockDevice + ?Sized>(
    device: &D,
    offset: u64,
    buf: &mut [u8],
    block: u64,
) -> Result<Vec<u64>, CopyError> {
    let mut lost = Vec::new();
    for (index, block_buf) in buf.chunks_mut(block as usize).enumerate() {
        let block_offset = offset + index as u64 * block;
        match device.read_at(block_offset, block_buf) {
            Ok(()) => {}
            Err(DiskError::Io { .. }) => {
                block_buf.fill(0);
                lost.push(block_offset);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(lost)
}

fn total_len(pieces: &[Range<u64>]) -> usize {
    pieces.iter().map(|p| (p.end - p.start) as usize).sum()
}

/// Maps an index into the concatenated piece data back to an offset within the partition.
fn offset_in_pieces(pieces: &[Range<u64>], mut index: usize) -> u64 {
    for piece in pieces {
        let len = (piece.end - piece.start) as usize;
        if index < len {
            return piece.start + index as u64;
        }
        index -= len;
    }
    0
}
