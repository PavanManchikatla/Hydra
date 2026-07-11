//! # hydra-proto
//!
//! Authoritative Hydra wire + WAL schemas (BLUEPRINT §1.2). The flatc-generated code in
//! [`generated`] is the source of truth — handwritten structs that shadow these tables are
//! forbidden. This crate adds the thin typed layers the spec requires around the generated code:
//!
//! - [`limits`]  — hard wire caps, validated **before** allocation.
//! - [`framing`] — the `HYFR` frame header + BLAKE3 tag, validated before parsing.
//! - [`pos`]     — [`InputPos`]/[`OutputPos`] newtypes (position discipline, spec I13).
//!
//! The generated FlatBuffers namespaces are re-exported as [`proto`] and [`wal`].

pub mod generated;
pub mod limits;
pub mod framing;
pub mod pos;

pub use pos::{InputPos, OutputPos};

/// Generated wire schema (`hydra.proto`): `Frame`, `Fence`, `Body` union, enums, error codes.
pub use generated::hydra_proto_generated::hydra::proto;

/// Generated WAL-record schema (`hydra.wal`): commit-stream and control records.
pub use generated::wal_records_generated::hydra::wal;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_enums_present() {
        // Sanity: the generated code is wired in and the error-code registry is reachable.
        assert_eq!(proto::ErrCode::OK.0, 0);
        assert_eq!(proto::ErrCode::ERR_LIMIT_EXCEEDED.0, 6);
        assert_eq!(proto::DType::F16.0, 1);
    }
}
