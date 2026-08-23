//! P2·7 — **chunked-prefill sizing, decided at admission.**
//!
//! Spec §2.4 says an input segment is committed in chunks "bounded by memory + contention-group
//! airtime at admission". Until P2·4 that sentence had no inputs; it does now, and this module is
//! where they meet.
//!
//! **Planned against objective (b), per the §7.23 ruling.** Prefill is *the* place in v1 where
//! multiple positions genuinely are in flight — a chunk's positions stream through the pipeline
//! together — so the pipelined-throughput reading applies here even though decode is gated on
//! single-stream latency (a). This is labelled rather than implicit, because using (b) anywhere
//! else in v1 would be wrong.
//!
//! # What actually bounds a chunk
//!
//! Not the steady-state bandwidth: a chunk of *n* positions carries *n* boundary residuals and
//! takes roughly *n* × per-position compute, so the **rate** is the same whatever *n* is. Chunk
//! size is not a bandwidth lever.
//!
//! What it does move:
//!
//! * **Peak retained memory.** Under R3′ a stage retains each boundary until it is releasable, and
//!   §2.4 releases per **chunk**, against boundary durability — not at end-of-prefill. So the
//!   retain buffer scales with the chunk, and that is the real ceiling.
//! * **Commit cadence.** Every chunk costs one `fdatasync`'d `INPUT_CHUNK_COMMIT`. Tiny chunks turn
//!   prefill into a sequence of disk barriers; huge chunks make an interruption expensive, because
//!   recovery truncates to `prefill_stable_pos` and redoes everything after it.
//!
//! So the plan is: **the largest chunk whose retained boundaries fit the memory budget**, floored so
//! the commit barrier is amortised, and never larger than the segment itself.
//!
//! Pure, like the rest of the crate: budgets are handed in.

/// Below this many positions a chunk spends more time in `fdatasync` than in compute.
pub const MIN_CHUNK_POSITIONS: u32 = 8;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ChunkPlanError {
    #[error("boundary payload of {0} B is not a positive size")]
    BadBoundaryBytes(u64),
    #[error("segment has {0} positions — nothing to prefill")]
    EmptySegment(u32),
    #[error(
        "retain budget {budget_bytes} B holds only {fits} boundary/ies of {boundary_bytes} B, below \
         the {min} -position floor — this session cannot be prefilled at any chunk size that is \
         worth committing; REFUSED rather than shrunk into a disk-barrier storm"
    )]
    BudgetBelowFloor { budget_bytes: u64, boundary_bytes: u64, fits: u64, min: u32 },
}

/// Inputs for sizing one segment's prefill.
#[derive(Debug, Clone, Copy)]
pub struct ChunkPlanInput {
    /// Positions in the input segment.
    pub segment_positions: u32,
    /// Wire bytes of one boundary residual (`n_embd × payload width`).
    pub boundary_bytes: u64,
    /// Bytes the tightest stage may hold in its R3′ retain buffer — an **admission** number, from
    /// P2·4's headroom-respecting budget, not the raw device memory.
    pub retain_budget_bytes: u64,
}

/// A sized prefill plan, with the reasoning kept attached to the number.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkPlan {
    /// Positions per chunk (the final chunk may be shorter).
    pub chunk_positions: u32,
    pub n_chunks: u32,
    /// Peak bytes retained under R3′ for one chunk.
    pub peak_retained_bytes: u64,
    /// True when the segment fits in one chunk — chunking is then a no-op, and saying so is better
    /// than pretending the plan did something.
    pub single_chunk: bool,
}

impl ChunkPlan {
    /// `[first, last]` input positions of chunk `i`, both inclusive.
    pub fn chunk_range(&self, i: u32, segment_positions: u32) -> Option<(i64, i64)> {
        if i >= self.n_chunks {
            return None;
        }
        let first = (i * self.chunk_positions) as i64;
        let last = (((i + 1) * self.chunk_positions).min(segment_positions) - 1) as i64;
        Some((first, last))
    }
}

pub fn plan_chunks(input: &ChunkPlanInput) -> Result<ChunkPlan, ChunkPlanError> {
    if input.boundary_bytes == 0 {
        return Err(ChunkPlanError::BadBoundaryBytes(input.boundary_bytes));
    }
    if input.segment_positions == 0 {
        return Err(ChunkPlanError::EmptySegment(0));
    }
    let fits = input.retain_budget_bytes / input.boundary_bytes;
    if fits < MIN_CHUNK_POSITIONS as u64 && (input.segment_positions as u64) > fits {
        // Refuse rather than shrink: a chunk below the floor turns prefill into a disk-barrier
        // storm, and quietly accepting that would be the same silent degradation P2·4 refuses.
        return Err(ChunkPlanError::BudgetBelowFloor {
            budget_bytes: input.retain_budget_bytes,
            boundary_bytes: input.boundary_bytes,
            fits,
            min: MIN_CHUNK_POSITIONS,
        });
    }
    let chunk = (fits.min(input.segment_positions as u64) as u32).max(MIN_CHUNK_POSITIONS).min(input.segment_positions);
    let n_chunks = input.segment_positions.div_ceil(chunk);
    Ok(ChunkPlan {
        chunk_positions: chunk,
        n_chunks,
        peak_retained_bytes: chunk as u64 * input.boundary_bytes,
        single_chunk: n_chunks == 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 0.5B boundary residual: 896 dims x f32.
    const B: u64 = 896 * 4;

    fn inp(positions: u32, budget: u64) -> ChunkPlanInput {
        ChunkPlanInput { segment_positions: positions, boundary_bytes: B, retain_budget_bytes: budget }
    }

    #[test]
    fn a_segment_that_fits_the_budget_is_one_chunk_and_says_so() {
        let p = plan_chunks(&inp(64, 64 * B)).unwrap();
        assert_eq!(p.chunk_positions, 64);
        assert_eq!(p.n_chunks, 1);
        assert!(p.single_chunk, "chunking is a no-op here and the plan should not pretend otherwise");
    }

    #[test]
    fn the_chunk_is_the_largest_that_fits_the_retain_budget() {
        // 100 boundaries' worth of budget against a 4096-position prompt.
        let p = plan_chunks(&inp(4096, 100 * B)).unwrap();
        assert_eq!(p.chunk_positions, 100);
        assert_eq!(p.peak_retained_bytes, 100 * B);
        assert_eq!(p.n_chunks, 41, "4096 / 100 rounded up");
        assert!(!p.single_chunk);
    }

    #[test]
    fn peak_retained_memory_never_exceeds_the_admission_budget() {
        // THE memory property: R3′ releases per CHUNK, so the retain buffer is the chunk. Whatever
        // the segment length, the plan must never plan to hold more than admission allowed.
        for positions in [16u32, 100, 1000, 4096, 100_000] {
            for budget_boundaries in [8u64, 33, 512] {
                let p = plan_chunks(&inp(positions, budget_boundaries * B)).unwrap();
                assert!(
                    p.peak_retained_bytes <= budget_boundaries * B,
                    "positions={positions} budget={budget_boundaries}: retained {} > budget {}",
                    p.peak_retained_bytes,
                    budget_boundaries * B
                );
            }
        }
    }

    #[test]
    fn the_chunks_tile_the_segment_exactly_with_no_gap_and_no_overlap() {
        // Prefill must apply every input position exactly once — the same covering property the
        // layer allocator has, for the same reason.
        for positions in [1u32, 7, 8, 9, 100, 4096] {
            let p = plan_chunks(&inp(positions, 33 * B)).unwrap();
            let mut expect_next = 0i64;
            for i in 0..p.n_chunks {
                let (first, last) = p.chunk_range(i, positions).unwrap();
                assert_eq!(first, expect_next, "positions={positions} chunk {i} must start where the last ended");
                assert!(last >= first, "positions={positions} chunk {i} is inverted");
                expect_next = last + 1;
            }
            assert_eq!(expect_next, positions as i64, "positions={positions}: chunks must cover the segment exactly");
            assert!(p.chunk_range(p.n_chunks, positions).is_none(), "no chunk past the end");
        }
    }

    #[test]
    fn a_budget_below_the_floor_is_refused_not_shrunk() {
        // A chunk under the floor spends more time in fdatasync than in compute. Quietly accepting
        // it would be the same silent degradation P2·4 refuses — so this is a structured refusal.
        let e = plan_chunks(&inp(4096, 3 * B)).unwrap_err();
        match e {
            ChunkPlanError::BudgetBelowFloor { fits, min, .. } => {
                assert_eq!(fits, 3);
                assert_eq!(min, MIN_CHUNK_POSITIONS);
            }
            other => panic!("expected a structured refusal, got {other:?}"),
        }
        assert!(e.to_string().contains("REFUSED"), "the refusal must say so: {e}");
    }

    #[test]
    fn a_short_segment_under_the_floor_is_still_plannable() {
        // A 3-position prompt with a 3-boundary budget is fine: the floor exists to stop
        // pathological SUBDIVISION, not to reject a segment that is simply small.
        let p = plan_chunks(&inp(3, 3 * B)).unwrap();
        assert_eq!(p.chunk_positions, 3);
        assert_eq!(p.n_chunks, 1);
        assert!(p.single_chunk);
    }

    #[test]
    fn degenerate_inputs_are_refused() {
        assert_eq!(
            plan_chunks(&ChunkPlanInput { segment_positions: 10, boundary_bytes: 0, retain_budget_bytes: 1 }).unwrap_err(),
            ChunkPlanError::BadBoundaryBytes(0)
        );
        assert_eq!(plan_chunks(&inp(0, 1000 * B)).unwrap_err(), ChunkPlanError::EmptySegment(0));
    }

    #[test]
    fn a_larger_budget_never_produces_more_chunks() {
        // Monotonicity: more admitted memory can only reduce the number of disk barriers. A plan
        // that got *worse* with more budget would mean the sizing rule is not doing what it claims.
        let mut prev = u32::MAX;
        for budget_boundaries in [8u64, 16, 32, 64, 128, 256, 512] {
            let p = plan_chunks(&inp(4096, budget_boundaries * B)).unwrap();
            assert!(p.n_chunks <= prev, "budget {budget_boundaries} produced more chunks than a smaller one");
            prev = p.n_chunks;
        }
    }
}
