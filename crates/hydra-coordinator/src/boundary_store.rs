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
}

impl BoundaryStore {
    /// Create the boundary-durability segment (header `fdatasync`'d + dir `fsync`'d before any record).
    pub fn create(path: impl AsRef<std::path::Path>, cluster_id: [u8; 16], session_id: [u8; 16]) -> Result<BoundaryStore, BoundaryError> {
        let header = FileHeader { flags: 0, cluster_id, session_scope: session_id };
        let writer = WalWriter::create(path, &header)?;
        Ok(BoundaryStore { writer, durable_through_input_pos: -1 })
    }

    pub fn durable_through_input_pos(&self) -> i64 {
        self.durable_through_input_pos
    }

    /// Durably append one boundary (one input position in this slice). Returns the new durable
    /// frontier — the `DURABILITY_ACK` position the upstream stage's R3′ buffer waits on. The
    /// frontier advances **only after** the `fdatasync`'d append returns (structural emit-after-commit
    /// for the durability plane).
    pub fn append_boundary(&mut self, boundary_id: u32, first_input_pos: i64, chunk_id: u32, activations: &[f32]) -> Result<i64, BoundaryError> {
        let mut fbb = FlatBufferBuilder::new();
        let data = fbb.create_vector(&f32_to_le(activations));
        let dims = fbb.create_vector(&[activations.len() as u32]);
        let tensor = proto::Tensor::create(
            &mut fbb,
            &proto::TensorArgs { dtype: proto::DType::F32, dims: Some(dims), data: Some(data), block_scales: None },
        );
        let bc = proto::BoundaryCopy::create(
            &mut fbb,
            &proto::BoundaryCopyArgs { boundary_id, first_input_pos, n_positions: 1, chunk_id, activations: Some(tensor) },
        );
        fbb.finish(bc, None);
        self.writer.append(rec_type::BOUNDARY_COPY, 0, fbb.finished_data())?;
        // Durable now — advance the frontier (boundaries are appended in input-position order).
        self.durable_through_input_pos = self.durable_through_input_pos.max(first_input_pos);
        Ok(self.durable_through_input_pos)
    }

    /// Read the durable boundaries back (for a recovery rebuild), ascending by input position.
    pub fn read(path: impl AsRef<std::path::Path>) -> Result<Vec<DurableBoundary>, BoundaryError> {
        let scan = WalScan::open(path)?;
        let mut out = Vec::new();
        for r in scan.records.iter().filter(|r| r.record_type == rec_type::BOUNDARY_COPY) {
            let bc = flatbuffers::root::<proto::BoundaryCopy>(&r.payload).map_err(|e| BoundaryError::Malformed(e.to_string()))?;
            let t = bc.activations();
            out.push(DurableBoundary {
                boundary_id: bc.boundary_id(),
                first_input_pos: bc.first_input_pos(),
                chunk_id: bc.chunk_id(),
                activations: le_to_f32(t.data().bytes()),
            });
        }
        out.sort_by_key(|b| b.first_input_pos);
        Ok(out)
    }
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
