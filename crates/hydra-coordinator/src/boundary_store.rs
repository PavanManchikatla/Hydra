//! The **boundary durability store** (D1 substrate, spec §5/§7).
//!
//! In durability mode D1, a stage's forwarded boundary is durably **copied** (`BOUNDARY_COPY`) to a
//! durability target before the upstream stage may release it (the R3′ release rule; the retention
//! half lives in `hydra_worker::retain::R3Buffer`). This store is that target: it persists each
//! boundary to a real `hydra-wal` file (torn-write-safe, `fdatasync`'d) and returns the durable
//! frontier — the `DURABILITY_ACK` position. On S_P loss the replacement's KV is rebuilt from these
//! durable boundaries (seam 3), **not** by full-token replay — the D1 difference.
//!
//! The durable payload is the **authoritative `hydra.proto.BoundaryCopy` flatbuffer** (no shadow
//! struct), under WAL record type `BOUNDARY_COPY` (id 5).

use flatbuffers::FlatBufferBuilder;
use hydra_proto::proto;
use hydra_wal::file::FileHeader;
use hydra_wal::reader::WalScan;
use hydra_wal::record::rec_type;
use hydra_wal::writer::WalWriter;

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("wal: {0}")]
    Wal(#[from] hydra_wal::WalError),
    #[error("malformed BOUNDARY_COPY record: {0}")]
    Malformed(String),
    /// **Audit H5 / M7 — a boundary that does not continue the durable sequence.** The frontier
    /// this store returns is a `DURABILITY_ACK`, and an upstream stage **releases its retain
    /// buffer** on it (R3′). A frontier that ran ahead of a hole would therefore free the very
    /// boundary a recovery needs, permanently.
    #[error("boundary at input_pos {got} does not continue the durable sequence (frontier {frontier}); refusing")]
    NotContiguous { got: i64, frontier: i64 },
    /// **Audit H5 — the stored record must be fenced to this session/epoch.** A boundary from
    /// another session or a superseded epoch, replayed into a rebuild, is a wrong-context KV.
    #[error("boundary fence mismatch: {what}")]
    FenceMismatch { what: &'static str },
    /// **Audit H5 (the H9 shape, on the durability plane).** An earlier append failed, so the
    /// on-disk tail is unknown and the frontier can no longer be trusted to describe the file.
    #[error("boundary store poisoned by an earlier failed append ({why})")]
    Poisoned { why: String },
}

/// One durable boundary read back for a recovery replay.
#[derive(Debug, Clone, PartialEq)]
pub struct DurableBoundary {
    pub boundary_id: u32,
    pub first_input_pos: i64,
    pub chunk_id: u32,
    pub activations: Vec<f32>,
}

/// The coordinator's (or a designated target's) durable boundary log.
pub struct BoundaryStore {
    writer: WalWriter,
    durable_through_input_pos: i64,
    /// **Audit H5 — the session/epoch this store is fenced to.** Every stored record must belong
    /// to it; a boundary from another session or a superseded epoch is not durability, it is a
    /// wrong-context KV waiting to be replayed into a rebuild.
    fence: BoundaryFence,
    /// Audit H5: positions already made durable, for dedupe. A duplicate is idempotent (the same
    /// position, already on disk) — never a second record and never a frontier advance.
    seen: std::collections::BTreeSet<i64>,
    /// Audit H5 (the H9 shape): set by the first failed append; refuses every later one.
    poisoned: Option<String>,
}

/// The identity a boundary store is fenced to (audit H5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryFence {
    pub cluster_id: [u8; 16],
    pub session_id: [u8; 16],
    pub epoch: u32,
}

impl BoundaryStore {
    /// Create the boundary-durability segment (header `fdatasync`'d + dir `fsync`'d before any record).
    pub fn create(path: impl AsRef<std::path::Path>, cluster_id: [u8; 16], session_id: [u8; 16]) -> Result<BoundaryStore, BoundaryError> {
        Self::create_fenced(path, BoundaryFence { cluster_id, session_id, epoch: 0 })
    }

    /// Create a store fenced to `fence` (audit H5). Prefer this: the two-argument [`Self::create`]
    /// defaults the epoch to 0, which is right for a fresh session and wrong for anything else.
    pub fn create_fenced(path: impl AsRef<std::path::Path>, fence: BoundaryFence) -> Result<BoundaryStore, BoundaryError> {
        let header = FileHeader { flags: 0, cluster_id: fence.cluster_id, session_scope: fence.session_id };
        let writer = WalWriter::create(path, &header)?;
        Ok(BoundaryStore {
            writer,
            durable_through_input_pos: -1,
            fence,
            seen: std::collections::BTreeSet::new(),
            poisoned: None,
        })
    }

    /// The fence this store is bound to.
    pub fn fence(&self) -> BoundaryFence {
        self.fence
    }

    /// Audit H5: has an append failed, leaving the on-disk tail unknown?
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    pub fn durable_through_input_pos(&self) -> i64 {
        self.durable_through_input_pos
    }

    /// Durably append one boundary (one input position in this slice). Returns the new durable
    /// frontier — the `DURABILITY_ACK` position the upstream stage's R3′ buffer waits on. The
    /// frontier advances **only after** the `fdatasync`'d append returns (structural emit-after-commit
    /// for the durability plane).
    /// **Audit H5 + M7 — four defects lived in the three lines this replaces.**
    ///
    /// 1. **A `max()` frontier.** `durable_through = max(durable_through, pos)` means a boundary
    ///    that arrives out of order **jumps the frontier over the gap**. The returned value is a
    ///    `DURABILITY_ACK`, and R3′ tells the upstream stage it may **release its retain buffer**
    ///    up to it — so acking 5 when 4 never landed frees the boundary a recovery needs, and it
    ///    is gone. *Acking durability over holes*, in the audit's words.
    /// 2. **Swallowed append errors.** The durability-target serve loops wrote
    ///    `append_boundary(..).unwrap_or(-1)` and carried on. Combined with (1), a failed write
    ///    followed by a successful later one acked straight over the failure.
    /// 3. **No dedupe.** A retransmitted boundary appended a second copy of the same position.
    /// 4. **No session/epoch fence on stored records** — a boundary from another session or a
    ///    superseded epoch was stored and later replayed into a rebuild as if it belonged.
    ///
    /// All four are now refusals. **Contiguity (M7) is the frontier rule**: a boundary must be
    /// `frontier + 1`, or a duplicate of something already durable (idempotent, no second record);
    /// anything else is `NotContiguous` and the frontier does not move. An error poisons the store
    /// (the H9 shape) because after a failed write the file's tail — and therefore the meaning of
    /// the frontier — is unknown.
    pub fn append_boundary(&mut self, boundary_id: u32, first_input_pos: i64, chunk_id: u32, activations: &[f32]) -> Result<i64, BoundaryError> {
        self.append_boundary_fenced(
            BoundaryFence { cluster_id: self.fence.cluster_id, session_id: self.fence.session_id, epoch: self.fence.epoch },
            boundary_id,
            first_input_pos,
            chunk_id,
            activations,
        )
    }

    /// [`Self::append_boundary`] with the sender's fence stated explicitly — the form a real
    /// durability target uses, since the fence arrives on the wire with the `BOUNDARY_COPY` frame
    /// and must be checked against the store's own before anything is written (audit H5).
    pub fn append_boundary_fenced(
        &mut self,
        fence: BoundaryFence,
        boundary_id: u32,
        first_input_pos: i64,
        chunk_id: u32,
        activations: &[f32],
    ) -> Result<i64, BoundaryError> {
        if let Some(why) = &self.poisoned {
            return Err(BoundaryError::Poisoned { why: why.clone() });
        }
        if fence.cluster_id != self.fence.cluster_id {
            return Err(BoundaryError::FenceMismatch { what: "cluster_id" });
        }
        if fence.session_id != self.fence.session_id {
            return Err(BoundaryError::FenceMismatch { what: "session_id" });
        }
        if fence.epoch != self.fence.epoch {
            return Err(BoundaryError::FenceMismatch { what: "session_epoch" });
        }
        if first_input_pos < 0 {
            return Err(BoundaryError::NotContiguous { got: first_input_pos, frontier: self.durable_through_input_pos });
        }
        // Dedupe: a position already durable is acked from the frontier, with nothing written.
        if self.seen.contains(&first_input_pos) {
            return Ok(self.durable_through_input_pos);
        }
        // Contiguity (M7): the only position that may extend the durable sequence is the next one.
        if first_input_pos != self.durable_through_input_pos + 1 {
            return Err(BoundaryError::NotContiguous { got: first_input_pos, frontier: self.durable_through_input_pos });
        }

        let payload = encode_boundary_record(boundary_id, first_input_pos, chunk_id, activations);
        if let Err(e) = self.writer.append(rec_type::BOUNDARY_COPY, 0, &payload) {
            self.poisoned = Some(e.to_string());
            return Err(BoundaryError::Wal(e));
        }
        // Durable now — and only now does the frontier move, by exactly one position.
        self.durable_through_input_pos = first_input_pos;
        self.seen.insert(first_input_pos);
        Ok(self.durable_through_input_pos)
    }

    /// Read the durable boundaries back (for a recovery rebuild), ascending by input position.
    pub fn read(path: impl AsRef<std::path::Path>) -> Result<Vec<DurableBoundary>, BoundaryError> {
        let scan = WalScan::open(path)?;
        let mut out = Vec::new();
        for r in scan.records.iter().filter(|r| r.record_type == rec_type::BOUNDARY_COPY) {
            out.push(decode_boundary_record(&r.payload)?);
        }
        out.sort_by_key(|b| b.first_input_pos);
        // **M7 on the read side.** A rebuild replays these into a KV, so a gap is not a missing
        // record — it is a KV that skips a position and every later position attending over a
        // history that never existed. The write side refuses gaps; this asserts the file agrees,
        // because the file is what a *different process* recovers from.
        for (i, b) in out.iter().enumerate() {
            if b.first_input_pos != i as i64 {
                return Err(BoundaryError::NotContiguous { got: b.first_input_pos, frontier: i as i64 - 1 });
            }
        }
        Ok(out)
    }
}

/// The `BOUNDARY_COPY` record payload: the same `proto::BoundaryCopy` FlatBuffer a `BOUNDARY_COPY`
/// frame carries, with the C3 shape `dims = [1, n_embd]`.
pub fn encode_boundary_record(boundary_id: u32, first_input_pos: i64, chunk_id: u32, activations: &[f32]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let data = fbb.create_vector(&f32_to_le(activations));
    // C3: the declared shape is `[n_positions, n_embd]` — the same record a `BOUNDARY_COPY`
    // frame carries, so a replayed boundary decodes through the same cross-check on the wire.
    let dims = fbb.create_vector(&[1u32, activations.len() as u32]);
    let tensor = proto::Tensor::create(
        &mut fbb,
        &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
    );
    let bc = proto::BoundaryCopy::create(
        &mut fbb,
        &proto::BoundaryCopyArgs { boundary_id, first_input_pos, n_positions: 1, chunk_id, activations: Some(tensor) },
    );
    fbb.finish(bc, None);
    fbb.finished_data().to_vec()
}

/// Parse one `BOUNDARY_COPY` record payload read back from the **disk** — untrusted input under
/// rule 17 ("anything the process did not compute itself"; a boundary store is a file).
///
/// **Audit C3 on the disk side.** A stored record whose declared shape disagrees with its bytes is
/// refused, not replayed: `dtype == F32`, `dims == [n_positions, n_embd]`, `n_positions == 1`
/// (M4), `n_embd ≥ 1`, `bytes == n_positions × 4 × n_embd`. The wire-side
/// `hydra_worker::wire::check_boundary_tensor` carries the authoritative write-up; this is the
/// same check, spelled the same way on purpose so the class is recognisable at a glance.
///
/// This is the `boundary-record` fuzz target (`hydra-fuzz`). Note what it does **not** prove: that
/// the `n_embd` matches the engine a recovery will replay into — that is the worker's pre-FFI
/// cross-check, which every replayed boundary still passes through as a `BOUNDARY_COPY`/`FWD`.
pub fn decode_boundary_record(payload: &[u8]) -> Result<DurableBoundary, BoundaryError> {
    let bc = flatbuffers::root::<proto::BoundaryCopy>(payload).map_err(|e| BoundaryError::Malformed(e.to_string()))?;
    let t = bc.activations();
    let dims = t.dims();
    let bytes = t.data().bytes().len() as u64;
    let shape_ok = t.dtype() == proto::DType::F32
        && t.block_scales().is_none()
        && dims.len() == 2
        && dims.get(0) as u64 == bc.n_positions() as u64
        && bc.n_positions() == 1
        && dims.get(1) > 0
        && bytes == dims.get(0) as u64 * 4 * dims.get(1) as u64;
    if !shape_ok {
        return Err(BoundaryError::Malformed(format!(
            "BOUNDARY_COPY record at input_pos {}: declared shape/bytes disagree (dtype={:?}, n_positions={}, dims={:?}, bytes={})",
            bc.first_input_pos(),
            t.dtype(),
            bc.n_positions(),
            dims.iter().collect::<Vec<_>>(),
            bytes
        )));
    }
    Ok(DurableBoundary {
        boundary_id: bc.boundary_id(),
        first_input_pos: bc.first_input_pos(),
        chunk_id: bc.chunk_id(),
        activations: le_to_f32(t.data().bytes()),
    })
}

fn f32_to_le(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for &x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn le_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}
