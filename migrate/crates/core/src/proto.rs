//! The wire protocol (architecture A1, implementation I3). A frame on the wire is
//! `[len: u32 LE][epoch: u64 LE][seq: u64 LE][body][crc32c: u32 LE]`, where `body` is the
//! postcard encoding of a [`Message`]. The CRC catches our own framing bugs only; it is NOT a
//! security control, and the transport is currently plaintext with no authentication (see
//! `net::lib` and the security audit). Decoding never trusts its input: a truncated, oversized or corrupt frame is a typed
//! error, never a panic, which the round-trip and garbage proptests check.

use serde::{Deserialize, Serialize};

use crate::converge::PassKind;
use crate::units::{DiskRange, Epoch, FrameSeq};

/// The current protocol version, carried in `Hello`. Any incompatible change bumps it and keeps a
/// decode test for each older version still accepted.
pub const PROTOCOL_VERSION: u16 = 1;

/// The largest frame body we will encode or accept (4 MiB chunk plus headroom).
pub const MAX_BODY: usize = (4 << 20) + 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Sender,
    Receiver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Auto,
    Track,
    Compare,
    Mirror,
}

/// How a chunk's `data` is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    None,
    Zstd,
    Lz4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportTransport {
    NvmeTcp,
    Iscsi,
}

/// A message, carried in a frame's body. The frame header holds the epoch and sequence; the
/// variant tag is the "kind".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        protocol: u16,
        role: Role,
        software: String,
        disk_len: u64,
        block_size: u32,
        used_digest: [u8; 32],
    },
    Plan {
        layout: Vec<(DiskRange, DiskRange)>,
        chunk_size: u32,
        budget_ms: u64,
        mode: Mode,
    },
    Chunk {
        pass: u32,
        /// The source range the sender read; the receiver maps it to the target through the layout.
        source: DiskRange,
        hash: [u8; 32],
        codec: Codec,
        data: Vec<u8>,
    },
    Ack {
        acked_seq: u64,
    },
    IndexRequest {
        from_chunk: u64,
        to_chunk: u64,
    },
    IndexPage {
        from_chunk: u64,
        hashes: Vec<[u8; 32]>,
    },
    PassEnd {
        pass: u32,
        kind: PassKind,
        sent_bytes: u64,
        dirty_at_end: u64,
    },
    Freeze,
    FinalRoot {
        root: [u8; 32],
    },
    /// The receiver's signed proof that the target equals the source (USP U1), sent back after it
    /// has the source's root and has computed its own.
    Receipt {
        receipt: crate::receipt::Receipt,
    },
    Commit {
        epoch: u64,
        source_root: [u8; 32],
        target_root: [u8; 32],
        receipt_digest: [u8; 32],
    },
    Committed {
        epoch: u64,
    },
    /// The source's last act after powering off/fencing; the only token that lets a committed
    /// target boot (invariant M7, A7.4).
    Surrendered {
        epoch: u64,
    },
    Rollback {
        epoch: u64,
    },
    RolledBack {
        epoch: u64,
    },
    Export {
        transport: ExportTransport,
        address: String,
        name: String,
    },
    Error {
        reason: String,
        fix: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("the frame is too short to hold a header")]
    TooShort,
    #[error("the frame body is {len} bytes, past the {MAX_BODY}-byte limit")]
    TooLarge { len: usize },
    #[error("the frame's length field ({declared}) doesn't match its {actual} bytes")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("the frame's CRC is wrong")]
    BadCrc,
    #[error("the frame body isn't a valid message")]
    BadBody,
}

const LEN_BYTES: usize = 4;
const HEADER_BYTES: usize = 8 + 8; // epoch + seq
const CRC_BYTES: usize = 4;

/// The largest whole frame on the wire: length prefix + header + body + CRC. A streaming transport
/// rejects any declared length past this before allocating for it.
pub const MAX_FRAME: usize = LEN_BYTES + HEADER_BYTES + MAX_BODY + CRC_BYTES;

/// Given at least the first [`LEN_BYTES`] of a frame, the total number of bytes the whole frame
/// occupies on the wire (length prefix + header + body + CRC), or `None` if fewer bytes are in
/// hand. A streaming transport (TCP) uses this to split a byte stream back into frames; it does not
/// validate the frame, which [`decode`] does.
pub fn frame_total_len(prefix: &[u8]) -> Option<usize> {
    if prefix.len() < LEN_BYTES {
        return None;
    }
    let inner = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    Some(LEN_BYTES + inner + CRC_BYTES)
}

/// Encodes a message into one frame.
pub fn encode(epoch: Epoch, seq: FrameSeq, message: &Message) -> Result<Vec<u8>, CodecError> {
    let body = postcard::to_allocvec(message).map_err(|_| CodecError::BadBody)?;
    if body.len() > MAX_BODY {
        return Err(CodecError::TooLarge { len: body.len() });
    }
    let inner_len = HEADER_BYTES + body.len();
    let mut frame = Vec::with_capacity(LEN_BYTES + inner_len + CRC_BYTES);
    frame.extend_from_slice(&(inner_len as u32).to_le_bytes());
    frame.extend_from_slice(&epoch.0.to_le_bytes());
    frame.extend_from_slice(&seq.0.to_le_bytes());
    frame.extend_from_slice(&body);
    let crc = crc32c::crc32c(&frame[LEN_BYTES..]);
    frame.extend_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

/// Decodes one frame. Never panics on malformed input.
pub fn decode(frame: &[u8]) -> Result<(Epoch, FrameSeq, Message), CodecError> {
    if frame.len() < LEN_BYTES + HEADER_BYTES + CRC_BYTES {
        return Err(CodecError::TooShort);
    }
    let declared = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if declared > MAX_BODY + HEADER_BYTES {
        return Err(CodecError::TooLarge {
            len: declared.saturating_sub(HEADER_BYTES),
        });
    }
    let actual = frame.len().saturating_sub(LEN_BYTES + CRC_BYTES);
    if declared != actual {
        return Err(CodecError::LengthMismatch { declared, actual });
    }
    let inner = &frame[LEN_BYTES..LEN_BYTES + declared];
    let crc_bytes = &frame[LEN_BYTES + declared..];
    let crc = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    if crc32c::crc32c(inner) != crc {
        return Err(CodecError::BadCrc);
    }
    let epoch = Epoch(u64::from_le_bytes(
        inner[0..8].try_into().map_err(|_| CodecError::TooShort)?,
    ));
    let seq = FrameSeq(u64::from_le_bytes(
        inner[8..16].try_into().map_err(|_| CodecError::TooShort)?,
    ));
    let message = postcard::from_bytes(&inner[HEADER_BYTES..]).map_err(|_| CodecError::BadBody)?;
    Ok((epoch, seq, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn messages() -> impl Strategy<Value = Message> {
        prop_oneof![
            Just(Message::Freeze),
            any::<u64>().prop_map(|s| Message::Ack { acked_seq: s }),
            any::<u64>().prop_map(|e| Message::Surrendered { epoch: e }),
            (any::<u32>(), any::<u64>(), any::<u64>()).prop_map(|(p, o, l)| Message::Chunk {
                pass: p,
                source: DiskRange::new(o, o.wrapping_add(l)),
                hash: [7u8; 32],
                codec: Codec::Zstd,
                data: vec![1, 2, 3, 4],
            }),
            (any::<String>(), any::<String>())
                .prop_map(|(reason, fix)| Message::Error { reason, fix }),
        ]
    }

    proptest! {
        /// Encoding then decoding returns the same epoch, sequence and message.
        #[test]
        fn round_trips(epoch in any::<u64>(), seq in any::<u64>(), message in messages()) {
            let frame = encode(Epoch(epoch), FrameSeq(seq), &message).expect("encode");
            let (de, ds, dm) = decode(&frame).expect("decode");
            prop_assert_eq!(de, Epoch(epoch));
            prop_assert_eq!(ds, FrameSeq(seq));
            prop_assert_eq!(dm, message);
        }

        /// Decoding arbitrary bytes never panics; it returns a value or a typed error.
        #[test]
        fn decoding_garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = decode(&bytes);
        }

        /// Flipping any byte of a valid frame is caught (CRC, length or body).
        #[test]
        fn a_flipped_byte_is_rejected(seq in any::<u64>(), index in 0usize..40) {
            let original = Message::Committed { epoch: 9 };
            let frame = encode(Epoch(9), FrameSeq(seq), &original).expect("encode");
            if index < frame.len() {
                let mut corrupt = frame.clone();
                corrupt[index] ^= 0xff;
                if corrupt != frame {
                    let decoded_same = decode(&corrupt)
                        .map(|(e, s, m)| e == Epoch(9) && s == FrameSeq(seq) && m == original)
                        .unwrap_or(false);
                    prop_assert!(!decoded_same, "a corrupted frame decoded unchanged");
                }
            }
        }
    }
}
