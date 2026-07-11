//! Transport framing (outside FlatBuffers, little-endian), per `hydra-proto.fbs`:
//!
//! ```text
//! u32 magic = 0x48594652 ("HYFR") | u16 wire_version | u16 flags
//! u32 payload_len (<= MAX_FRAME_BYTES) | payload: Frame flatbuffer
//! [32]u8 blake3 of (header || payload)
//! ```
//!
//! A receiver validates magic, version (major must match; unknown minor => accept),
//! `payload_len` against `MAX_FRAME_BYTES`, and the BLAKE3 tag **before** parsing the
//! flatbuffer or allocating the payload buffer.

use crate::limits::{check_frame_len, MAX_FRAME_BYTES};

pub const FRAME_MAGIC: u32 = 0x4859_4652; // "HYFR"
pub const WIRE_VERSION: u16 = 1; // (major << 8) | minor; current = major 0, minor 1
pub const HEADER_LEN: usize = 12; // magic(4) + version(2) + flags(2) + payload_len(4)
pub const TAG_LEN: usize = 32; // BLAKE3

#[inline]
fn version_major(v: u16) -> u8 {
    (v >> 8) as u8
}

/// Frame-level errors. Several map to a structured `ErrCode` reply (never a silent drop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("short buffer: have {have}, need {need}")]
    Truncated { have: usize, need: usize },
    #[error("bad magic: {0:#010x}")]
    BadMagic(u32),
    /// Maps to `ErrCode::ERR_UNSUPPORTED_VERSION`.
    #[error("unsupported wire major: frame {frame_major}, ours {our_major}")]
    UnsupportedVersion { frame_major: u8, our_major: u8 },
    /// Maps to `ErrCode::ERR_LIMIT_EXCEEDED`.
    #[error("payload_len {payload_len} exceeds cap {cap}")]
    LimitExceeded { payload_len: u32, cap: u32 },
    /// Maps to `ErrCode::ERR_BAD_CHECKSUM`.
    #[error("blake3 tag mismatch")]
    BadChecksum,
}

/// Parsed, validated frame header. Produced **without** touching the payload bytes, so an
/// oversized/garbage frame is rejected before any payload allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub flags: u16,
    pub payload_len: u32,
}

impl FrameHeader {
    /// Validate magic, version major, and `payload_len` against the cap — pre-allocation.
    /// Does not require the payload to be present.
    pub fn parse(buf: &[u8]) -> Result<FrameHeader, FrameError> {
        if buf.len() < HEADER_LEN {
            return Err(FrameError::Truncated { have: buf.len(), need: HEADER_LEN });
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != FRAME_MAGIC {
            return Err(FrameError::BadMagic(magic));
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version_major(version) != version_major(WIRE_VERSION) {
            return Err(FrameError::UnsupportedVersion {
                frame_major: version_major(version),
                our_major: version_major(WIRE_VERSION),
            });
        }
        // unknown *minor* is accepted (forward-compatible)
        let flags = u16::from_le_bytes([buf[6], buf[7]]);
        let payload_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        if !check_frame_len(payload_len).is_ok() {
            return Err(FrameError::LimitExceeded { payload_len, cap: MAX_FRAME_BYTES });
        }
        Ok(FrameHeader { flags, payload_len })
    }

    /// Total on-wire size of a frame with this header.
    #[inline]
    pub fn frame_size(&self) -> usize {
        HEADER_LEN + self.payload_len as usize + TAG_LEN
    }
}

/// Encode a complete frame: `header || payload || blake3(header || payload)`.
pub fn encode_frame(flags: u16, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let payload_len = payload.len() as u64;
    if payload_len > MAX_FRAME_BYTES as u64 {
        return Err(FrameError::LimitExceeded { payload_len: payload_len as u32, cap: MAX_FRAME_BYTES });
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let tag = blake3::hash(&out); // over header || payload
    out.extend_from_slice(tag.as_bytes());
    Ok(out)
}

/// Verify a complete frame's BLAKE3 tag and return the payload slice.
/// Header (incl. cap) is validated first, so a bad `payload_len` is rejected before hashing.
pub fn verify_frame(buf: &[u8]) -> Result<(FrameHeader, &[u8]), FrameError> {
    let hdr = FrameHeader::parse(buf)?;
    let total = hdr.frame_size();
    if buf.len() < total {
        return Err(FrameError::Truncated { have: buf.len(), need: total });
    }
    let body_end = HEADER_LEN + hdr.payload_len as usize;
    let expected = blake3::hash(&buf[..body_end]);
    let got = &buf[body_end..body_end + TAG_LEN];
    if got != expected.as_bytes() {
        return Err(FrameError::BadChecksum);
    }
    Ok((hdr, &buf[HEADER_LEN..body_end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = b"hello hydra frame";
        let frame = encode_frame(0x1234, payload).unwrap();
        let (hdr, got) = verify_frame(&frame).unwrap();
        assert_eq!(hdr.flags, 0x1234);
        assert_eq!(hdr.payload_len as usize, payload.len());
        assert_eq!(got, payload);
    }

    #[test]
    fn bad_magic_rejected() {
        let mut frame = encode_frame(0, b"x").unwrap();
        frame[0] ^= 0xFF;
        assert!(matches!(verify_frame(&frame), Err(FrameError::BadMagic(_))));
    }

    #[test]
    fn oversized_payload_len_rejected_pre_alloc() {
        // Hand-craft a header claiming a huge payload; parse must reject without the payload.
        let mut buf = Vec::new();
        buf.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf.extend_from_slice(&WIRE_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        assert!(matches!(
            FrameHeader::parse(&buf),
            Err(FrameError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn major_version_mismatch_rejected() {
        let mut frame = encode_frame(0, b"x").unwrap();
        // bump the major byte (high byte of the u16 at offset 4)
        frame[5] = frame[5].wrapping_add(1);
        assert!(matches!(
            verify_frame(&frame),
            Err(FrameError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn corrupt_payload_fails_checksum() {
        let mut frame = encode_frame(0, b"tamper me").unwrap();
        frame[HEADER_LEN] ^= 0x01; // flip a payload bit
        assert!(matches!(verify_frame(&frame), Err(FrameError::BadChecksum)));
    }

    #[test]
    fn unknown_minor_accepted() {
        let mut frame = encode_frame(0, b"y").unwrap();
        // bump minor (low byte of version) and re-tag so only the version differs
        let mut hdr_and_body = frame[..HEADER_LEN + 1].to_vec();
        hdr_and_body[4] = hdr_and_body[4].wrapping_add(1); // minor++
        let tag = blake3::hash(&hdr_and_body);
        let mut f2 = hdr_and_body.clone();
        f2.extend_from_slice(tag.as_bytes());
        let _ = &mut frame;
        assert!(verify_frame(&f2).is_ok());
    }
}
