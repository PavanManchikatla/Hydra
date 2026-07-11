//! Hard wire limits (normative; from `hydra-proto.fbs`). A receiver validates these
//! **before allocating** any buffer — reject, never truncate.

/// Max total frame payload (prefill chunks dominate).
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;
/// Max single tensor `data` length.
pub const MAX_TENSOR_BYTES: u32 = 48 * 1024 * 1024;
/// Max positions per `Fwd`/`BoundaryCopy` frame.
pub const MAX_POSITIONS_PER_FRAME: u16 = 1024;
/// Max sampler snapshot bytes (snapshots are small by design).
pub const MAX_SNAPSHOT_BYTES: u32 = 1024 * 1024;
/// Max length of any wire string.
pub const MAX_STRING_BYTES: u32 = 4 * 1024;

/// Result of a pre-parse size check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitCheck {
    Ok,
    /// Exceeded a cap: (what, value, cap). Maps to `ErrCode::ERR_LIMIT_EXCEEDED`.
    Exceeded { what: &'static str, value: u64, cap: u64 },
}

impl LimitCheck {
    #[inline]
    pub fn is_ok(self) -> bool {
        matches!(self, LimitCheck::Ok)
    }
}

/// Check a declared payload length against `MAX_FRAME_BYTES` before allocating.
#[inline]
pub fn check_frame_len(payload_len: u32) -> LimitCheck {
    if payload_len > MAX_FRAME_BYTES {
        LimitCheck::Exceeded { what: "frame", value: payload_len as u64, cap: MAX_FRAME_BYTES as u64 }
    } else {
        LimitCheck::Ok
    }
}

/// Check a declared tensor byte length against `MAX_TENSOR_BYTES` before allocating.
#[inline]
pub fn check_tensor_len(tensor_bytes: u64) -> LimitCheck {
    if tensor_bytes > MAX_TENSOR_BYTES as u64 {
        LimitCheck::Exceeded { what: "tensor", value: tensor_bytes, cap: MAX_TENSOR_BYTES as u64 }
    } else {
        LimitCheck::Ok
    }
}

/// Check a declared position count against `MAX_POSITIONS_PER_FRAME`.
#[inline]
pub fn check_positions(n_positions: u32) -> LimitCheck {
    if n_positions > MAX_POSITIONS_PER_FRAME as u32 {
        LimitCheck::Exceeded {
            what: "positions",
            value: n_positions as u64,
            cap: MAX_POSITIONS_PER_FRAME as u64,
        }
    } else {
        LimitCheck::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_match_schema() {
        assert_eq!(MAX_FRAME_BYTES, 64 << 20);
        assert_eq!(MAX_TENSOR_BYTES, 48 << 20);
        assert_eq!(MAX_POSITIONS_PER_FRAME, 1024);
        assert_eq!(MAX_SNAPSHOT_BYTES, 1 << 20);
        assert_eq!(MAX_STRING_BYTES, 4 << 10);
    }

    #[test]
    fn frame_len_boundary() {
        assert!(check_frame_len(MAX_FRAME_BYTES).is_ok());
        assert!(!check_frame_len(MAX_FRAME_BYTES + 1).is_ok());
    }

    #[test]
    fn tensor_and_positions_boundaries() {
        assert!(check_tensor_len(MAX_TENSOR_BYTES as u64).is_ok());
        assert!(!check_tensor_len(MAX_TENSOR_BYTES as u64 + 1).is_ok());
        assert!(check_positions(MAX_POSITIONS_PER_FRAME as u32).is_ok());
        assert!(!check_positions(MAX_POSITIONS_PER_FRAME as u32 + 1).is_ok());
    }
}
