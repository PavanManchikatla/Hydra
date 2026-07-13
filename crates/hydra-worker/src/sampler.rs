//! The S_P sampler (BLUEPRINT M2 slice 3; spec §2.6a/§2.6b, invariants **I14/I15/I17/I19-producer**).
//!
//! **Sampling is an action, not a side effect** — the RNG advances *only* inside a stochastic
//! sample step (greedy draws none). The generator is **Philox4x32-10**, counter-based and keyed per
//! session (`rng_key`, `rng_counter` exactly as `SamplerCheckpoint` defines), so a given
//! `(key, counter)` always yields the same draw — reproducible across processes and restarts.
//!
//! Every sample produces a **post-sample snapshot** (`SamplerCheckpointRec`, spec §2.6b) capturing
//! the state *after* that position: because S_P runs ahead of the commit boundary, `SAMPLED{q}`
//! must carry `snapshot(q)`, never live state, so the eventual `GENERATION_COMMIT` embeds the
//! boundary-matching snapshot (I19 producer side). The snapshot is restorable — a fresh S_P
//! installed from it resumes the exact state (I17). Grammar state is out of scope (empty).
//!
//! **Ownership boundary (spec §1.4):** the coordinator builds *only* the config-defined initial
//! checkpoint; S_P produces every subsequent snapshot. No sampling logic lives in the coordinator.

use hydra_proto::wal;

/// Philox constants (Salmon et al., "Parallel Random Numbers: As Easy as 1, 2, 3").
const M0: u32 = 0xD251_1F53;
const M1: u32 = 0xCD9E_8D57;
const W0: u32 = 0x9E37_79B9;
const W1: u32 = 0xBB67_AE85;

/// Philox4x32-10: a counter-based, keyed bijection. `uniform(counter)` maps a 64-bit counter to a
/// deterministic `[0, 1)` draw under the session key — no internal mutable state, so the same
/// `(key, counter)` always reproduces the same value.
#[derive(Clone, Copy, Debug)]
pub struct Philox {
    k0: u32,
    k1: u32,
}

impl Philox {
    pub fn keyed(rng_key: u64) -> Self {
        Philox { k0: rng_key as u32, k1: (rng_key >> 32) as u32 }
    }

    fn block(&self, counter: u64) -> [u32; 4] {
        let mut c = [counter as u32, (counter >> 32) as u32, 0u32, 0u32];
        let (mut k0, mut k1) = (self.k0, self.k1);
        for _ in 0..10 {
            let hi0 = ((M0 as u64 * c[0] as u64) >> 32) as u32;
            let lo0 = (M0 as u64 * c[0] as u64) as u32;
            let hi1 = ((M1 as u64 * c[2] as u64) >> 32) as u32;
            let lo1 = (M1 as u64 * c[2] as u64) as u32;
            c = [hi1 ^ c[1] ^ k0, lo1, hi0 ^ c[3] ^ k1, lo0];
            k0 = k0.wrapping_add(W0);
            k1 = k1.wrapping_add(W1);
        }
        c
    }

    /// A `[0, 1)` draw for `counter` (24-bit mantissa, ample for token CDF selection).
    pub fn uniform(&self, counter: u64) -> f32 {
        (self.block(counter)[0] >> 8) as f32 * (1.0 / 16_777_216.0)
    }
}

/// The session sampling policy. `sampling_config_hash` (spec §2.6b) is the canonical digest both
/// sides fence on; a `SAMPLE_NEXT` whose hash disagrees is fatal drift, never silently repaired.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub top_p: f32,
    pub repeat_penalty: f32,
    pub penalty_last_n: usize,
    pub seed: u64,
}

impl SamplingConfig {
    /// Deterministic argmax decoding (the exact-tier reference; no RNG draw, no penalty).
    pub fn greedy() -> Self {
        SamplingConfig { temperature: 0.0, top_p: 1.0, repeat_penalty: 1.0, penalty_last_n: 0, seed: 0 }
    }

    pub fn is_greedy(&self) -> bool {
        self.temperature == 0.0
    }

    /// Canonical config digest (spec `sampling_config_hash`).
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"hydra.sampling_config.v1");
        h.update(&self.temperature.to_le_bytes());
        h.update(&self.top_p.to_le_bytes());
        h.update(&self.repeat_penalty.to_le_bytes());
        h.update(&(self.penalty_last_n as u64).to_le_bytes());
        h.update(&self.seed.to_le_bytes());
        *h.finalize().as_bytes()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SamplerError {
    #[error("checkpoint drift: SAMPLE_NEXT expected checkpoint {expected}, installed {installed}")]
    CheckpointDrift { expected: u64, installed: u64 },
    #[error("sampling-config-hash drift (spec §2.6b): SAMPLE_NEXT config does not match installed")]
    ConfigDrift,
    #[error("no retained logits for output_pos {0} (I14: sample only from retained logits)")]
    NoRetainedLogits(i64),
    #[error("malformed sampler checkpoint snapshot: {0}")]
    BadSnapshot(String),
    #[error("snapshot checksum mismatch (state_checksum)")]
    BadChecksum,
}

/// The result of a sample step: the token, its post-sample snapshot bytes (a serialized
/// `SamplerCheckpointRec`), and the 32-byte state digest.
#[derive(Clone, Debug)]
pub struct Sampled {
    pub output_pos: i64,
    pub token_id: u32,
    pub snapshot: Vec<u8>,
    pub state_digest: [u8; 32],
}

/// S_P's live sampler. Holds the RNG state, the repetition-penalty window, and the installed
/// checkpoint id. `sample` is the only method that advances the RNG (and only for a stochastic
/// draw). Pure of I/O — the worker owns one of these and drives it from decoded frames.
#[derive(Clone, Debug)]
pub struct Sampler {
    checkpoint_id: u64,
    rng_key: u64,
    rng_counter: u64,
    penalty_window: Vec<u32>,
    generated_through: i64,
    sampled_pos: i64,
    config: SamplingConfig,
    config_hash: [u8; 32],
}

impl Sampler {
    /// A fresh sampler for the config-defined **initial** checkpoint (rng_counter 0, empty penalty).
    pub fn initial(checkpoint_id: u64, config: SamplingConfig) -> Self {
        let config_hash = config.hash();
        Sampler {
            checkpoint_id,
            rng_key: config.seed,
            rng_counter: 0,
            penalty_window: Vec::new(),
            generated_through: -1,
            sampled_pos: -1,
            config,
            config_hash,
        }
    }

    pub fn checkpoint_id(&self) -> u64 {
        self.checkpoint_id
    }
    pub fn config_hash(&self) -> [u8; 32] {
        self.config_hash
    }
    pub fn sampled_pos(&self) -> i64 {
        self.sampled_pos
    }
    pub fn rng_counter(&self) -> u64 {
        self.rng_counter
    }

    /// Fence a `SAMPLE_NEXT`'s `expected_sampler_checkpoint_id` + `sampling_config_hash` against the
    /// installed state (spec §2.6b). Drift is fatal — the caller must reject loudly, never repair.
    pub fn check_fence(&self, expected_checkpoint_id: u64, config_hash: &[u8]) -> Result<(), SamplerError> {
        if expected_checkpoint_id != self.checkpoint_id {
            return Err(SamplerError::CheckpointDrift { expected: expected_checkpoint_id, installed: self.checkpoint_id });
        }
        if config_hash != self.config_hash {
            return Err(SamplerError::ConfigDrift);
        }
        Ok(())
    }

    /// Draw the token for `output_pos` from `logits`, advancing the RNG **only** for a stochastic
    /// draw (greedy advances nothing), and produce the post-sample snapshot.
    pub fn sample(&mut self, output_pos: i64, logits: &[f32]) -> Sampled {
        let token = if self.config.is_greedy() {
            argmax(logits)
        } else {
            let mut work = apply_repeat_penalty(logits, &self.penalty_window, self.config.repeat_penalty);
            softmax_in_place(&mut work, self.config.temperature);
            top_p_filter(&mut work, self.config.top_p);
            let u = Philox::keyed(self.rng_key).uniform(self.rng_counter);
            self.rng_counter += 1; // a stochastic sample step advances the counter
            select_cdf(&work, u)
        };

        // Update penalty window (bounded by penalty_last_n).
        if self.config.penalty_last_n > 0 {
            self.penalty_window.push(token);
            let overflow = self.penalty_window.len().saturating_sub(self.config.penalty_last_n);
            if overflow > 0 {
                self.penalty_window.drain(0..overflow);
            }
        }
        self.generated_through = output_pos;
        self.sampled_pos = output_pos;

        let snapshot = self.serialize();
        let state_digest = *blake3::hash(&snapshot).as_bytes();
        Sampled { output_pos, token_id: token, snapshot, state_digest }
    }

    /// Serialize the current state as a `SamplerCheckpointRec` (spec §2.6b snapshot model). The
    /// embedded `state_checksum` is BLAKE3 over the semantic fields (verified on install).
    pub fn serialize(&self) -> Vec<u8> {
        build_checkpoint_rec(
            self.checkpoint_id,
            self.rng_key,
            self.rng_counter,
            self.generated_through,
            &penalty_bytes(&self.penalty_window),
            self.sampled_pos,
            &self.config_hash,
        )
    }

    /// Install an exact state from a serialized `SamplerCheckpointRec` (I17 consumer side). The
    /// live config is retained (the checkpoint's `sampling_config_hash` must match it — otherwise
    /// the installed state and the config disagree, which is drift). Idempotent for the same
    /// checkpoint id + state.
    pub fn install(&mut self, snapshot: &[u8]) -> Result<(), SamplerError> {
        let rec = flatbuffers::root::<wal::SamplerCheckpointRec>(snapshot)
            .map_err(|e| SamplerError::BadSnapshot(e.to_string()))?;
        let recomputed = *blake3::hash(&reserialize_for_checksum(&rec)).as_bytes();
        if rec.state_checksum().bytes() != recomputed {
            return Err(SamplerError::BadChecksum);
        }
        if rec.sampling_config_hash().bytes() != self.config_hash {
            return Err(SamplerError::ConfigDrift);
        }
        self.checkpoint_id = rec.checkpoint_id();
        self.rng_key = rng_key_from_bytes(rec.rng_key().bytes());
        self.rng_counter = rec.rng_counter();
        self.generated_through = rec.generated_through_output_pos();
        self.sampled_pos = rec.sampled_output_pos();
        self.penalty_window = penalty_from_bytes(rec.serialized_penalty_state().bytes());
        Ok(())
    }
}

// ------------------------- snapshot (de)serialization -------------------------

fn penalty_bytes(window: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(window.len() * 4);
    for &t in window {
        v.extend_from_slice(&t.to_le_bytes());
    }
    v
}
fn penalty_from_bytes(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}
fn rng_key_from_bytes(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
    u64::from_le_bytes(a)
}

/// BLAKE3 preimage for `state_checksum` — the semantic fields in a fixed order (grammar empty).
fn checksum_preimage(
    checkpoint_id: u64,
    rng_key: u64,
    rng_counter: u64,
    generated_through: i64,
    penalty: &[u8],
    sampled_pos: i64,
    config_hash: &[u8],
) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"hydra.sampler_checkpoint.v1");
    p.extend_from_slice(&checkpoint_id.to_le_bytes());
    p.extend_from_slice(&rng_key.to_le_bytes());
    p.extend_from_slice(&rng_counter.to_le_bytes());
    p.extend_from_slice(&generated_through.to_le_bytes());
    p.extend_from_slice(&sampled_pos.to_le_bytes());
    p.extend_from_slice(penalty);
    p.extend_from_slice(config_hash);
    p
}

fn build_checkpoint_rec(
    checkpoint_id: u64,
    rng_key: u64,
    rng_counter: u64,
    generated_through: i64,
    penalty: &[u8],
    sampled_pos: i64,
    config_hash: &[u8],
) -> Vec<u8> {
    let checksum = *blake3::hash(&checksum_preimage(
        checkpoint_id, rng_key, rng_counter, generated_through, penalty, sampled_pos, config_hash,
    ))
    .as_bytes();
    let mut fbb = flatbuffers::FlatBufferBuilder::new();
    let rng_key_v = fbb.create_vector(&rng_key.to_le_bytes());
    let grammar_v = fbb.create_vector::<u8>(&[]); // grammar deferred (empty)
    let penalty_v = fbb.create_vector(penalty);
    let cfg_v = fbb.create_vector(config_hash);
    let sum_v = fbb.create_vector(&checksum);
    let rec = wal::SamplerCheckpointRec::create(
        &mut fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id,
            rng_key: Some(rng_key_v),
            rng_counter,
            generated_through_output_pos: generated_through,
            serialized_grammar_state: Some(grammar_v),
            serialized_penalty_state: Some(penalty_v),
            sampled_output_pos: sampled_pos,
            sampling_config_hash: Some(cfg_v),
            state_checksum: Some(sum_v),
        },
    );
    fbb.finish(rec, None);
    fbb.finished_data().to_vec()
}

fn reserialize_for_checksum(rec: &wal::SamplerCheckpointRec<'_>) -> Vec<u8> {
    checksum_preimage(
        rec.checkpoint_id(),
        rng_key_from_bytes(rec.rng_key().bytes()),
        rec.rng_counter(),
        rec.generated_through_output_pos(),
        rec.serialized_penalty_state().bytes(),
        rec.sampled_output_pos(),
        rec.sampling_config_hash().bytes(),
    )
}

/// Build the config-defined **initial** checkpoint snapshot — the *one* checkpoint the coordinator
/// is allowed to construct (spec §1.4 ownership boundary). rng_counter 0, empty penalty.
pub fn initial_checkpoint_bytes(checkpoint_id: u64, config: &SamplingConfig) -> Vec<u8> {
    build_checkpoint_rec(checkpoint_id, config.seed, 0, -1, &[], -1, &config.hash())
}

// ------------------------- numeric core -------------------------

fn argmax(logits: &[f32]) -> u32 {
    let mut bi = 0usize;
    for i in 1..logits.len() {
        if logits[i] > logits[bi] {
            bi = i;
        }
    }
    bi as u32
}

fn apply_repeat_penalty(logits: &[f32], window: &[u32], penalty: f32) -> Vec<f32> {
    let mut out = logits.to_vec();
    if penalty != 1.0 {
        for &tok in window {
            if let Some(l) = out.get_mut(tok as usize) {
                *l = if *l > 0.0 { *l / penalty } else { *l * penalty };
            }
        }
    }
    out
}

fn softmax_in_place(logits: &mut [f32], temperature: f32) {
    let t = temperature.max(1e-6);
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0f32;
    for l in logits.iter_mut() {
        *l = ((*l - max) / t).exp();
        sum += *l;
    }
    if sum > 0.0 {
        for l in logits.iter_mut() {
            *l /= sum;
        }
    }
}

/// Nucleus (top-p) filter: zero all probabilities outside the smallest set whose cumulative mass
/// reaches `top_p`, then renormalize. `top_p >= 1.0` is a no-op.
fn top_p_filter(probs: &mut [f32], top_p: f32) {
    if top_p >= 1.0 {
        return;
    }
    let mut idx: Vec<usize> = (0..probs.len()).collect();
    idx.sort_by(|&a, &b| probs[b].partial_cmp(&probs[a]).unwrap_or(std::cmp::Ordering::Equal));
    let mut cum = 0.0f32;
    let mut keep = vec![false; probs.len()];
    for &i in &idx {
        keep[i] = true;
        cum += probs[i];
        if cum >= top_p {
            break;
        }
    }
    let mut sum = 0.0f32;
    for i in 0..probs.len() {
        if keep[i] {
            sum += probs[i];
        } else {
            probs[i] = 0.0;
        }
    }
    if sum > 0.0 {
        for p in probs.iter_mut() {
            *p /= sum;
        }
    }
}

/// Select a token by inverse-CDF from a (normalized) distribution and uniform draw `u`.
fn select_cdf(probs: &[f32], u: f32) -> u32 {
    let mut cum = 0.0f32;
    for (i, &p) in probs.iter().enumerate() {
        cum += p;
        if u < cum {
            return i as u32;
        }
    }
    // Fallback (fp drift at the tail): last non-zero.
    probs.iter().rposition(|&p| p > 0.0).unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn philox_is_deterministic_and_keyed() {
        let a = Philox::keyed(42);
        let b = Philox::keyed(42);
        let c = Philox::keyed(43);
        assert_eq!(a.uniform(0), b.uniform(0), "same key+counter => same draw");
        assert_eq!(a.uniform(7), b.uniform(7));
        assert_ne!(a.uniform(0), c.uniform(0), "different key => different draw");
        assert_ne!(a.uniform(0), a.uniform(1), "counter advances the stream");
        for ctr in 0..100 {
            let u = a.uniform(ctr);
            assert!((0.0..1.0).contains(&u), "uniform in [0,1): {u}");
        }
    }

    #[test]
    fn greedy_is_argmax_and_advances_no_rng() {
        let mut s = Sampler::initial(1, SamplingConfig::greedy());
        let logits = [0.1f32, 9.0, 0.3, -1.0];
        let before = s.rng_counter();
        let out = s.sample(0, &logits);
        assert_eq!(out.token_id, 1, "argmax");
        assert_eq!(s.rng_counter(), before, "greedy draws no randomness");
    }

    #[test]
    fn stochastic_sample_advances_rng_and_snapshot_round_trips() {
        let cfg = SamplingConfig { temperature: 0.8, top_p: 0.95, repeat_penalty: 1.1, penalty_last_n: 8, seed: 12345 };
        let mut s = Sampler::initial(7, cfg.clone());
        let logits: Vec<f32> = (0..64).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = s.sample(0, &logits);
        assert_eq!(s.rng_counter(), 1, "stochastic draw advances the counter");

        // A fresh sampler installed from the snapshot resumes the exact state (I17).
        let mut fresh = Sampler::initial(7, cfg);
        fresh.install(&out.snapshot).expect("install");
        assert_eq!(fresh.rng_counter(), s.rng_counter());
        assert_eq!(fresh.sampled_pos(), s.sampled_pos());
        // ...and continues the identical stream.
        let a = s.sample(1, &logits);
        let b = fresh.sample(1, &logits);
        assert_eq!(a.token_id, b.token_id, "installed sampler continues identically");
    }

    #[test]
    fn checkpoint_and_config_drift_are_fatal() {
        let s = Sampler::initial(5, SamplingConfig::greedy());
        assert!(matches!(s.check_fence(6, &s.config_hash()), Err(SamplerError::CheckpointDrift { .. })));
        assert!(matches!(s.check_fence(5, &[0u8; 32]), Err(SamplerError::ConfigDrift)));
        assert!(s.check_fence(5, &s.config_hash()).is_ok());
    }

    #[test]
    fn corrupt_snapshot_checksum_is_rejected() {
        let cfg = SamplingConfig::greedy();
        let mut s = Sampler::initial(1, cfg.clone());
        let out = s.sample(0, &[0.1, 0.2, 0.9]);
        let mut bad = out.snapshot.clone();
        *bad.last_mut().unwrap() ^= 0xff;
        let mut fresh = Sampler::initial(1, cfg);
        assert!(matches!(fresh.install(&bad), Err(SamplerError::BadChecksum) | Err(SamplerError::BadSnapshot(_))));
    }
}
