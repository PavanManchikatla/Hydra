//! On-disk record framing (WAL-FORMAT.md §1). All integers little-endian.
//!
//! ```text
//! record := magic:u16 = 0x5243 ("RC") | record_type:u16 | flags:u16 | reserved:u16
//!         | payload_len:u32 (<= 64 MiB) | payload:[payload_len]u8
//!         | pad to 8-byte alignment (zero) | record_blake3:[32]u8   # magic..padding
//! ```

use crate::WalError;

pub const RECORD_MAGIC: u16 = 0x5243; // "RC"
pub const RECORD_HEADER_LEN: usize = 12; // magic2 + type2 + flags2 + reserved2 + payload_len4
pub const RECORD_TAG_LEN: usize = 32; // BLAKE3
pub const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024; // WAL-FORMAT §1 (payload_len <= 64 MiB)

/// Record flags (WAL-FORMAT §1).
pub const FLAG_CRITICAL: u16 = 0x0001; // unknown + critical => refuse to open

/// Record-type registry (WAL-FORMAT §2; never renumber, append only).
pub mod rec_type {
    // commit stream
    pub const INITIAL_COMMIT: u16 = 1;
    pub const SEGMENT_COMMIT: u16 = 2;
    pub const GENERATION_COMMIT: u16 = 3;
    pub const INPUT_CHUNK_COMMIT: u16 = 4;
    /// A durably-copied stage boundary residual (D1 substrate, spec §5/§7). Payload = the
    /// authoritative `hydra.proto.BoundaryCopy` flatbuffer (no shadow struct).
    pub const BOUNDARY_COPY: u16 = 5;
    // control WAL
    pub const BEGIN_RECOVERY: u16 = 10;
    pub const RESET_RECOVERY_ATTEMPT: u16 = 11;
    pub const ACTIVATION_COMMIT_INTENT: u16 = 12;
    pub const ACTIVATION_COMPLETE: u16 = 13;
    pub const ACTIVATION_ABORT: u16 = 14;
    pub const ACTIVATION_UNSERVABLE: u16 = 15;
    pub const SESSION_TERMINATE: u16 = 16;
    pub const CANCEL_CUTOFF: u16 = 17;
    pub const PLACEMENT_INSTALL: u16 = 18;
    // aux
    pub const EVENT_LOG: u16 = 30;
}

/// 8-byte alignment padding after `header || payload` (WAL-FORMAT §1).
#[inline]
pub fn pad_len(payload_len: usize) -> usize {
    let unpadded = RECORD_HEADER_LEN + payload_len;
    (8 - (unpadded % 8)) % 8
}

/// Total on-disk size of a record with the given payload length.
#[inline]
pub fn record_size(payload_len: usize) -> usize {
    RECORD_HEADER_LEN + payload_len + pad_len(payload_len) + RECORD_TAG_LEN
}

/// Parsed record header (12 bytes). Cheap; does not touch the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub record_type: u16,
    pub flags: u16,
    pub reserved: u16,
    pub payload_len: u32,
}

impl RecordHeader {
    #[inline]
    pub fn is_critical(&self) -> bool {
        self.flags & FLAG_CRITICAL != 0
    }
}

/// Encode a full record (`header || payload || pad || blake3`) into a fresh buffer.
pub fn encode_record(record_type: u16, flags: u16, payload: &[u8]) -> Result<Vec<u8>, WalError> {
    let plen = payload.len();
    if plen as u64 > MAX_PAYLOAD_LEN as u64 {
        return Err(WalError::PayloadTooLarge { len: plen as u64, cap: MAX_PAYLOAD_LEN });
    }
    let pad = pad_len(plen);
    let mut buf = Vec::with_capacity(record_size(plen));
    buf.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    buf.extend_from_slice(&record_type.to_le_bytes());
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&(plen as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf.resize(buf.len() + pad, 0u8); // zero padding
    let tag = blake3::hash(&buf); // over magic..padding
    buf.extend_from_slice(tag.as_bytes());
    Ok(buf)
}

/// Outcome of attempting to read one record at a byte offset.
#[derive(Debug)]
pub enum ReadStep<'a> {
    /// A complete, checksum-valid record.
    Record { header: RecordHeader, payload: &'a [u8], total_len: usize },
    /// Not enough bytes for a complete record here — a torn/incomplete tail.
    Incomplete,
    /// Framing is intact (magic + length parse, full record present) but the BLAKE3 tag
    /// does not match — a bit-flip. Whether this is a discardable tail or fatal corruption
    /// depends on whether a valid record follows (decided by the scanner).
    BadChecksum { total_len: usize },
    /// Magic wrong / length beyond cap — treat as torn tail garbage.
    BadFraming,
}

/// Try to read one record from `buf` starting at offset 0.
pub fn read_record(buf: &[u8]) -> ReadStep<'_> {
    if buf.len() < RECORD_HEADER_LEN {
        return ReadStep::Incomplete;
    }
    let magic = u16::from_le_bytes([buf[0], buf[1]]);
    if magic != RECORD_MAGIC {
        return ReadStep::BadFraming;
    }
    let record_type = u16::from_le_bytes([buf[2], buf[3]]);
    let flags = u16::from_le_bytes([buf[4], buf[5]]);
    let reserved = u16::from_le_bytes([buf[6], buf[7]]);
    let payload_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if payload_len > MAX_PAYLOAD_LEN {
        return ReadStep::BadFraming;
    }
    let plen = payload_len as usize;
    let total = record_size(plen);
    if buf.len() < total {
        return ReadStep::Incomplete; // torn: payload/pad/tag not fully present
    }
    let body_end = RECORD_HEADER_LEN + plen + pad_len(plen);
    let expected = blake3::hash(&buf[..body_end]);
    let got = &buf[body_end..body_end + RECORD_TAG_LEN];
    if got != expected.as_bytes() {
        return ReadStep::BadChecksum { total_len: total };
    }
    ReadStep::Record {
        header: RecordHeader { record_type, flags, reserved, payload_len },
        payload: &buf[RECORD_HEADER_LEN..RECORD_HEADER_LEN + plen],
        total_len: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_and_size() {
        // header is 12; payload 0 -> unpadded 12, pad 4, total 12+0+4+32 = 48
        assert_eq!(pad_len(0), 4);
        assert_eq!(record_size(0), 48);
        // payload 4 -> unpadded 16 (aligned), pad 0, total 16+32 = 48
        assert_eq!(pad_len(4), 0);
        assert_eq!(record_size(4), 48);
        // payload 5 -> unpadded 17, pad 7, total 12+5+7+32 = 56
        assert_eq!(pad_len(5), 7);
        assert_eq!(record_size(5), 56);
    }

    #[test]
    fn roundtrip() {
        let rec = encode_record(rec_type::GENERATION_COMMIT, 0, b"payload-bytes").unwrap();
        assert_eq!(rec.len(), record_size(b"payload-bytes".len()));
        match read_record(&rec) {
            ReadStep::Record { header, payload, total_len } => {
                assert_eq!(header.record_type, rec_type::GENERATION_COMMIT);
                assert_eq!(payload, b"payload-bytes");
                assert_eq!(total_len, rec.len());
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn detects_bitflip() {
        let mut rec = encode_record(rec_type::BEGIN_RECOVERY, 0, b"abc").unwrap();
        rec[RECORD_HEADER_LEN] ^= 0x01; // flip a payload bit
        assert!(matches!(read_record(&rec), ReadStep::BadChecksum { .. }));
    }

    #[test]
    fn short_buffer_incomplete() {
        let rec = encode_record(rec_type::BEGIN_RECOVERY, 0, b"abcdef").unwrap();
        assert!(matches!(read_record(&rec[..rec.len() - 1]), ReadStep::Incomplete));
        assert!(matches!(read_record(&rec[..RECORD_HEADER_LEN - 1]), ReadStep::Incomplete));
    }
}
