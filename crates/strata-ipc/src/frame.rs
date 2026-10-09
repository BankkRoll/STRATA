//! Binary framing.
//!
//! ```text
//! offset  size  field
//! 0       4     len         u32 LE: bytes after this field (= 5 + payload)
//! 4       1     kind        u8: 1 Hello, 2 Welcome, 3 Reject, 4 Request, 5 Response
//! 5       4     request_id  u32 LE
//! 9       len-5 payload     postcard encoding of the kind's message type
//! ```
//!
//! `len` is bounded by [`MAX_FRAME_LEN`]; a larger or smaller value poisons
//! the connection. Decoding is bounds-checked and never panics: the payload
//! must decode to exactly the declared length (no trailing bytes), and
//! postcard caps up-front allocation, so a hostile length prefix inside the
//! payload cannot reserve more memory than the frame itself holds.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::protocol::{Envelope, HandshakeReject, Hello, Message, Request, Response, Welcome};

/// Size of the `len` field.
pub const LEN_SIZE: usize = 4;
/// Size of `kind` + `request_id`.
pub const HEADER_SIZE: usize = 5;
/// Largest accepted `len` (64 MiB). A `ScanBatch` of 8192 records is ~0.5 MiB.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Frame kind byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// [`Hello`].
    Hello = 1,
    /// [`Welcome`].
    Welcome = 2,
    /// [`HandshakeReject`].
    Reject = 3,
    /// [`Request`].
    Request = 4,
    /// [`Response`].
    Response = 5,
}

impl FrameKind {
    /// Parses the kind byte.
    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Hello,
            2 => Self::Welcome,
            3 => Self::Reject,
            4 => Self::Request,
            5 => Self::Response,
            _ => return None,
        })
    }

    fn of(m: &Message) -> Self {
        match m {
            Message::Hello(_) => Self::Hello,
            Message::Welcome(_) => Self::Welcome,
            Message::Reject(_) => Self::Reject,
            Message::Request(_) => Self::Request,
            Message::Response(_) => Self::Response,
        }
    }
}

/// A framing or payload error. The stream cannot be resynchronized after
/// one, so the connection is closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// `len` is smaller than the fixed header.
    #[error("frame length {0} is shorter than the header")]
    TooShort(u32),
    /// `len` exceeds [`MAX_FRAME_LEN`] (or the payload to send would).
    #[error("frame length {len} exceeds the maximum {max}")]
    Oversized {
        /// Declared or required length.
        len: u64,
        /// The limit.
        max: u32,
    },
    /// Unknown kind byte.
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    /// The payload did not decode as the kind's message type.
    #[error("malformed payload: {0}")]
    Payload(String),
    /// The payload decoded but left unread bytes.
    #[error("{0} trailing bytes after payload")]
    TrailingBytes(usize),
}

/// Appends one encoded frame to `out`.
///
/// # Example
///
/// ```
/// use strata_ipc::frame::{decode_frame, encode_frame};
/// use strata_ipc::protocol::{Envelope, Message, Request};
/// let env = Envelope { request_id: 7, message: Message::Request(Request::Ping) };
/// let mut buf = Vec::new();
/// encode_frame(&env, &mut buf).unwrap();
/// let (back, used) = decode_frame(&buf).unwrap().unwrap();
/// assert_eq!((back, used), (env, buf.len()));
/// ```
pub fn encode_frame(env: &Envelope, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let start = out.len();
    out.extend_from_slice(&[0; LEN_SIZE]);
    out.push(FrameKind::of(&env.message) as u8);
    out.extend_from_slice(&env.request_id.to_le_bytes());
    let encoded = match &env.message {
        Message::Hello(m) => append(m, out),
        Message::Welcome(m) => append(m, out),
        Message::Reject(m) => append(m, out),
        Message::Request(m) => append(m, out),
        Message::Response(m) => append(m, out),
    };
    if let Err(e) = encoded {
        out.truncate(start);
        return Err(e);
    }
    let len = (out.len() - start - LEN_SIZE) as u64;
    if len > u64::from(MAX_FRAME_LEN) {
        out.truncate(start);
        return Err(FrameError::Oversized {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    out[start..start + LEN_SIZE].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(())
}

fn append<T: Serialize>(v: &T, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let taken = std::mem::take(out);
    match postcard::to_extend(v, taken) {
        Ok(v) => {
            *out = v;
            Ok(())
        }
        Err(e) => Err(FrameError::Payload(e.to_string())),
    }
}

/// Reads the declared length of the frame at the start of `buf`, validating
/// it, or `None` if fewer than 4 bytes are available.
pub fn peek_len(buf: &[u8]) -> Result<Option<u32>, FrameError> {
    let Some(b) = buf.get(..LEN_SIZE) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if len < HEADER_SIZE as u32 {
        return Err(FrameError::TooShort(len));
    }
    if len > MAX_FRAME_LEN {
        return Err(FrameError::Oversized {
            len: u64::from(len),
            max: MAX_FRAME_LEN,
        });
    }
    Ok(Some(len))
}

/// Decodes the first frame in `buf`.
///
/// Returns `Ok(None)` when `buf` holds only part of a frame, or the envelope
/// and the number of bytes it occupied.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Envelope, usize)>, FrameError> {
    let Some(len) = peek_len(buf)? else {
        return Ok(None);
    };
    let total = LEN_SIZE + len as usize;
    let Some(frame) = buf.get(LEN_SIZE..total) else {
        return Ok(None);
    };
    let kind = FrameKind::from_u8(frame[0]).ok_or(FrameError::UnknownKind(frame[0]))?;
    let request_id = u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]);
    let payload = &frame[HEADER_SIZE..];
    let message = match kind {
        FrameKind::Hello => Message::Hello(exact::<Hello>(payload)?),
        FrameKind::Welcome => Message::Welcome(exact::<Welcome>(payload)?),
        FrameKind::Reject => Message::Reject(exact::<HandshakeReject>(payload)?),
        FrameKind::Request => Message::Request(exact::<Request>(payload)?),
        FrameKind::Response => Message::Response(exact::<Response>(payload)?),
    };
    Ok(Some((
        Envelope {
            request_id,
            message,
        },
        total,
    )))
}

fn exact<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    let (v, rest) =
        postcard::take_from_bytes::<T>(payload).map_err(|e| FrameError::Payload(e.to_string()))?;
    if !rest.is_empty() {
        return Err(FrameError::TrailingBytes(rest.len()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::*;
    use proptest::prelude::*;
    use strata_core::{
        EntryFlags, FileRef, FileTime, NameLink, ScanRecord, Sizes, Times, WideName,
    };

    pub(crate) fn record(i: u64) -> ScanRecord {
        ScanRecord {
            id: FileRef::from_parts(i + 16, 1),
            links: vec![NameLink {
                parent: FileRef::from_parts(5, 5),
                name: WideName::from_str_lossless(&format!("file-{i:08}.bin")),
            }],
            attributes: 0x20,
            flags: EntryFlags::EMPTY,
            times: Times {
                created: FileTime(133_000_000_000_000_000 + i),
                modified: FileTime(133_000_000_000_000_000 + 2 * i),
                accessed: FileTime(133_000_000_000_000_000),
                changed: FileTime(0),
            },
            fn_created: None,
            sizes: Sizes {
                logical: i * 1000,
                allocated: (i * 1000).div_ceil(4096) * 4096,
                ..Default::default()
            },
            reparse: None,
            ads: vec![],
        }
    }

    fn samples() -> Vec<Envelope> {
        let msgs = vec![
            Message::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                build: "t".into(),
                client_pid: 42,
            }),
            Message::Welcome(Welcome {
                protocol: PROTOCOL_VERSION,
                helper_build: "h".into(),
                elevated: true,
                capabilities: Capabilities {
                    mft_scan: true,
                    ..Default::default()
                },
            }),
            Message::Reject(HandshakeReject::VersionMismatch {
                helper: 2,
                client: 1,
            }),
            Message::Request(Request::ScanVolume {
                volume: r"\\?\Volume{x}\".into(),
                options: ScanOptions::default(),
            }),
            Message::Request(Request::ReadUsn {
                volume: "v".into(),
                journal_id: 9,
                from: -1,
                max_bytes: 65536,
            }),
            Message::Request(Request::PrivilegedDelete(DeleteRequest {
                volume: "v".into(),
                file_ref: FileRef(77),
                expected_path: vec![0xD800, 0x41],
                expected_size: 3,
                expected_mtime: FileTime(9),
            })),
            Message::Response(Response::ScanBatch {
                records: (0..10).map(record).collect(),
            }),
            Message::Response(Response::UsnRecords {
                next_usn: 100,
                raw: vec![1, 2, 3],
            }),
            Message::Response(Response::Error(ErrorReply {
                code: ErrorCode::RateLimited,
                message: "slow down".into(),
                retry_after_ms: Some(10),
            })),
        ];
        msgs.into_iter()
            .enumerate()
            .map(|(i, message)| Envelope {
                request_id: i as u32,
                message,
            })
            .collect()
    }

    #[test]
    fn every_kind_round_trips() {
        let mut buf = Vec::new();
        for env in samples() {
            encode_frame(&env, &mut buf).unwrap();
        }
        let mut at = 0;
        for env in samples() {
            let (got, used) = decode_frame(&buf[at..]).unwrap().unwrap();
            assert_eq!(got, env);
            at += used;
        }
        assert_eq!(at, buf.len());
    }

    #[test]
    fn header_layout_is_byte_exact() {
        let mut buf = Vec::new();
        encode_frame(
            &Envelope {
                request_id: 0x0102_0304,
                message: Message::Request(Request::Ping),
            },
            &mut buf,
        )
        .unwrap();
        // len = 5 header bytes + 1 payload byte (variant index 0).
        assert_eq!(buf, [6, 0, 0, 0, 4, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn partial_frames_need_more_bytes() {
        let mut buf = Vec::new();
        encode_frame(&samples()[6], &mut buf).unwrap();
        for cut in 0..buf.len() {
            assert_eq!(decode_frame(&buf[..cut]).unwrap(), None, "cut {cut}");
        }
    }

    #[test]
    fn rejects_bad_headers() {
        assert_eq!(decode_frame(&[4, 0, 0, 0, 4]), Err(FrameError::TooShort(4)));
        let huge = (MAX_FRAME_LEN + 1).to_le_bytes();
        assert!(matches!(
            decode_frame(&huge),
            Err(FrameError::Oversized { .. })
        ));
        assert_eq!(
            decode_frame(&[5, 0, 0, 0, 9, 0, 0, 0, 0]),
            Err(FrameError::UnknownKind(9))
        );
        // Ping plus one stray byte.
        assert_eq!(
            decode_frame(&[7, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0]),
            Err(FrameError::TrailingBytes(1))
        );
        // Unknown Request variant index.
        assert!(matches!(
            decode_frame(&[6, 0, 0, 0, 4, 0, 0, 0, 0, 99]),
            Err(FrameError::Payload(_))
        ));
    }

    #[test]
    fn hostile_inner_length_does_not_allocate_wildly() {
        // A ScanBatch claiming u32::MAX records in a 10-byte payload.
        let mut frame = vec![0u8; 4];
        frame.push(FrameKind::Response as u8);
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.push(2); // Response::ScanBatch
        frame.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        let len = (frame.len() - 4) as u32;
        frame[..4].copy_from_slice(&len.to_le_bytes());
        assert!(matches!(decode_frame(&frame), Err(FrameError::Payload(_))));
    }

    #[test]
    fn oversized_payload_is_refused_on_encode() {
        let env = Envelope {
            request_id: 1,
            message: Message::Response(Response::UsnRecords {
                next_usn: 0,
                raw: vec![0; MAX_FRAME_LEN as usize],
            }),
        };
        let mut buf = vec![1, 2, 3];
        assert!(matches!(
            encode_frame(&env, &mut buf),
            Err(FrameError::Oversized { .. })
        ));
        assert_eq!(buf, [1, 2, 3], "failed encode leaves the buffer intact");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_frame(&bytes);
        }

        #[test]
        fn arbitrary_payloads_with_valid_headers_never_panic(
            kind in 0u8..8,
            id in any::<u32>(),
            payload in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let mut buf = ((payload.len() + 5) as u32).to_le_bytes().to_vec();
            buf.push(kind);
            buf.extend_from_slice(&id.to_le_bytes());
            buf.extend_from_slice(&payload);
            if let Ok(Some((_, used))) = decode_frame(&buf) {
                prop_assert_eq!(used, buf.len());
            }
        }

        #[test]
        fn mutated_valid_frames_never_panic(
            which in 0usize..9,
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
        ) {
            let mut buf = Vec::new();
            encode_frame(&samples()[which], &mut buf).unwrap();
            for (at, v) in flips {
                let i = at % buf.len();
                buf[i] = v;
            }
            let _ = decode_frame(&buf);
        }
    }
}
