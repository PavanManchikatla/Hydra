//! The durable **commit stream** (spec §2.6a) on a real coordinator disk file — where M1's
//! virtual-disk discipline meets a real `hydra-wal` segment. `INITIAL_COMMIT` carries the admission
//! hashes + the config-defined initial checkpoint (the fields slice 4 prepared); each
//! `GENERATION_COMMIT` embeds `snapshot(q)` for `last_output_position` **from the SAMPLED ring, never
//! live state** (spec §2.6a), with **I19's equalities validated on write** (the validator from M0).
//!
//! **`generation_durable_pos` advances only after `fdatasync` returns** — `WalWriter::append` does
//! the `fdatasync` before it returns, so the ordering is structural: the position is bumped only on
//! the success path after append. This is the substrate under emit-after-commit (I6/I9).

use flatbuffers::FlatBufferBuilder;
use hydra_proto::validate_generation_commit_i19;
use hydra_proto::wal;
use hydra_tokenizer::Admission;
use hydra_wal::file::FileHeader;
use hydra_wal::record::rec_type;
use hydra_wal::writer::WalWriter;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("wal: {0}")]
    Wal(#[from] hydra_wal::WalError),
    #[error("I19 violation on write: {0}")]
    I19(String),
    #[error("malformed sampler checkpoint snapshot: {0}")]
    BadCheckpoint(String),
}

/// The durability sink behind the commit stream — one `append` = one record made durable
/// (`fdatasync`) before it returns. Abstracted so the emit-after-commit gate can be proven **by
/// absence**: a sink whose `append` stalls/fails must leave `generation_durable_pos` un-advanced,
/// so nothing is ever emitted past the last durable position.
pub trait Durability: Send {
    fn append(&mut self, record_type: u16, flags: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError>;
    fn durable_len(&self) -> u64;
}

impl Durability for WalWriter {
    fn append(&mut self, record_type: u16, flags: u16, payload: &[u8]) -> Result<u64, hydra_wal::WalError> {
        WalWriter::append(self, record_type, flags, payload)
    }
    fn durable_len(&self) -> u64 {
        self.len()
    }
}

/// The durable-fence context every commit record embeds (a subset of the wire fence).
#[derive(Clone, Debug)]
pub struct WalFenceCtx {
    pub cluster_id: [u8; 16],
    pub session_id: [u8; 16],
    pub model_instance_id: [u8; 16],
    pub manifest_hash: [u8; 32],
    pub epoch: u32,
    pub recovery_id: u32,
    pub activation_attempt_id: u32,
}

fn build_fence<'a>(fbb: &mut FlatBufferBuilder<'a>, f: &WalFenceCtx) -> flatbuffers::WIPOffset<wal::WalFence<'a>> {
    let cluster_id = Some(fbb.create_vector(&f.cluster_id));
    let session_id = Some(fbb.create_vector(&f.session_id));
    let model_instance_id = Some(fbb.create_vector(&f.model_instance_id));
    let manifest_hash = Some(fbb.create_vector(&f.manifest_hash));
    wal::WalFence::create(
        fbb,
        &wal::WalFenceArgs {
            cluster_id,
            session_id,
            model_instance_id,
            manifest_hash,
            session_epoch: f.epoch,
            recovery_id: f.recovery_id,
            activation_attempt_id: f.activation_attempt_id,
        },
    )
}

/// Re-embed a serialized `SamplerCheckpointRec` (the opaque `snapshot(q)` bytes S_P produced and the
/// coordinator relays) as a nested table inside another builder — copying fields, never re-deriving
/// state (the coordinator holds no sampler; spec §1.4).
fn rebuild_checkpoint<'a>(fbb: &mut FlatBufferBuilder<'a>, snapshot: &[u8]) -> Result<flatbuffers::WIPOffset<wal::SamplerCheckpointRec<'a>>, CommitError> {
    let rec = flatbuffers::root::<wal::SamplerCheckpointRec>(snapshot).map_err(|e| CommitError::BadCheckpoint(e.to_string()))?;
    let rng_key = Some(fbb.create_vector(rec.rng_key().bytes()));
    let grammar = Some(fbb.create_vector(rec.serialized_grammar_state().bytes()));
    let penalty = Some(fbb.create_vector(rec.serialized_penalty_state().bytes()));
    let cfg = Some(fbb.create_vector(rec.sampling_config_hash().bytes()));
    let sum = Some(fbb.create_vector(rec.state_checksum().bytes()));
    Ok(wal::SamplerCheckpointRec::create(
        fbb,
        &wal::SamplerCheckpointRecArgs {
            checkpoint_id: rec.checkpoint_id(),
            rng_key,
            rng_counter: rec.rng_counter(),
            generated_through_output_pos: rec.generated_through_output_pos(),
            serialized_grammar_state: grammar,
            serialized_penalty_state: penalty,
            sampled_output_pos: rec.sampled_output_pos(),
            sampling_config_hash: cfg,
            state_checksum: sum,
        },
    ))
}

/// A finished vector of `TokenEntry` offsets in a builder.
type TokenEntryVec<'a> = flatbuffers::WIPOffset<flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<wal::TokenEntry<'a>>>>;

fn build_token_entries<'a>(fbb: &mut FlatBufferBuilder<'a>, tokens: &[(i64, u32)], origin: hydra_proto::proto::TokenOrigin) -> TokenEntryVec<'a> {
    let entries: Vec<_> = tokens
        .iter()
        .map(|&(pos, tok)| {
            wal::TokenEntry::create(
                fbb,
                &wal::TokenEntryArgs { absolute_position: pos, token_id: tok, origin, message_segment_id: 0, rng_checkpoint_counter: 0 },
            )
        })
        .collect();
    fbb.create_vector(&entries)
}

/// The coordinator's durable commit stream.
pub struct CommitStream {
    writer: Box<dyn Durability>,
    generation_durable_pos: i64,
    committed_sampler_checkpoint_id: u64,
    next_commit_id: u64,
    last_commit_id: u64,
}

impl CommitStream {
    /// Create the session's commit-stream segment file under `dir` (the file header is `fdatasync`'d
    /// and the directory `fsync`'d before any record — WAL-FORMAT §3.2).
    pub fn create(path: impl AsRef<std::path::Path>, cluster_id: [u8; 16], session_id: [u8; 16]) -> Result<CommitStream, CommitError> {
        let header = FileHeader { flags: 0, cluster_id, session_scope: session_id };
        let writer = WalWriter::create(path, &header)?;
        Ok(Self::with_durability(Box::new(writer)))
    }

    /// Build over an arbitrary [`Durability`] sink (tests: a stalling/failing `fdatasync` to prove
    /// the emit-after-commit gate by absence).
    pub fn with_durability(writer: Box<dyn Durability>) -> CommitStream {
        CommitStream {
            writer,
            generation_durable_pos: -1,
            committed_sampler_checkpoint_id: 0,
            next_commit_id: 1,
            last_commit_id: 0,
        }
    }

    pub fn generation_durable_pos(&self) -> i64 {
        self.generation_durable_pos
    }
    pub fn committed_sampler_checkpoint_id(&self) -> u64 {
        self.committed_sampler_checkpoint_id
    }
    pub fn last_commit_id(&self) -> u64 {
        self.last_commit_id
    }
    pub fn durable_len(&self) -> u64 {
        self.writer.durable_len()
    }

    /// `INITIAL_COMMIT` — admission metadata (the three hashes) + the config-defined initial
    /// checkpoint. Durable when this returns.
    pub fn append_initial_commit(
        &mut self,
        fence: &WalFenceCtx,
        admission: &Admission,
        initial_checkpoint: &[u8],
        durability_mode: u8,
    ) -> Result<(), CommitError> {
        let mut fbb = FlatBufferBuilder::new();
        let fence_off = build_fence(&mut fbb, fence);
        let tokenizer_hash = Some(fbb.create_vector(&admission.tokenizer_hash));
        let chat_template_hash = Some(fbb.create_vector(&admission.chat_template_hash));
        let rendered_prompt_bytes_hash = Some(fbb.create_vector(&admission.rendered_prompt_bytes_hash));
        let prompt: Vec<(i64, u32)> = admission.prompt_tokens.iter().enumerate().map(|(i, &t)| (i as i64, t)).collect();
        let prompt_tokens = Some(build_token_entries(&mut fbb, &prompt, hydra_proto::proto::TokenOrigin::PROMPT));
        let ckpt = rebuild_checkpoint(&mut fbb, initial_checkpoint)?;
        let ic = wal::InitialCommit::create(
            &mut fbb,
            &wal::InitialCommitArgs {
                fence: Some(fence_off),
                tokenizer_hash,
                chat_template_hash,
                rendered_prompt_bytes_hash,
                prompt_tokens,
                prompt_length: admission.prompt_tokens.len() as i64,
                initial_checkpoint: Some(ckpt),
                durability_mode,
            },
        );
        fbb.finish(ic, None);
        self.writer.append(rec_type::INITIAL_COMMIT, 0, fbb.finished_data())?;
        let rec = flatbuffers::root::<wal::InitialCommit>(fbb.finished_data()).expect("just built");
        self.committed_sampler_checkpoint_id = rec.initial_checkpoint().checkpoint_id();
        Ok(())
    }

    /// `GENERATION_COMMIT` for the group `(first_output_pos ..= last_output_pos)`, embedding
    /// `checkpoint` = `snapshot(last_output_pos)` from the SAMPLED ring. **I19 is validated before
    /// the append** — `generated_through == sampled_pos == last_output_pos` — so a record that would
    /// violate it never reaches the disk. `generation_durable_pos` advances only after the
    /// `fdatasync`'d append returns. Returns the new `commit_id`.
    pub fn append_generation_commit(
        &mut self,
        fence: &WalFenceCtx,
        first_output_pos: i64,
        last_output_pos: i64,
        tokens: &[(i64, u32)],
        checkpoint: &[u8],
    ) -> Result<u64, CommitError> {
        let commit_id = self.next_commit_id;
        let mut fbb = FlatBufferBuilder::new();
        let fence_off = build_fence(&mut fbb, fence);
        let token_off = build_token_entries(&mut fbb, tokens, hydra_proto::proto::TokenOrigin::GENERATED);
        let ckpt = rebuild_checkpoint(&mut fbb, checkpoint)?;
        let mut hasher = blake3::Hasher::new();
        for &(pos, tok) in tokens {
            hasher.update(&pos.to_le_bytes());
            hasher.update(&tok.to_le_bytes());
        }
        let entries_checksum = Some(fbb.create_vector(hasher.finalize().as_bytes()));
        let gc = wal::GenerationCommit::create(
            &mut fbb,
            &wal::GenerationCommitArgs {
                fence: Some(fence_off),
                commit_id,
                previous_commit_id: self.last_commit_id,
                first_output_pos,
                last_output_pos,
                tokens: Some(token_off),
                checkpoint: Some(ckpt),
                entries_checksum,
            },
        );
        fbb.finish(gc, None);
        let payload = fbb.finished_data();

        // I19 on write: one record or nothing (spec §2.6a). Validated BEFORE the durable append.
        validate_generation_commit_i19(payload).map_err(CommitError::I19)?;

        self.writer.append(rec_type::GENERATION_COMMIT, 0, payload)?;
        // Durable now (append fdatasync'd) — only now advance the watermarks.
        self.generation_durable_pos = last_output_pos;
        self.committed_sampler_checkpoint_id =
            flatbuffers::root::<wal::GenerationCommit>(payload).expect("just built").checkpoint().checkpoint_id();
        self.last_commit_id = commit_id;
        self.next_commit_id += 1;
        Ok(commit_id)
    }
}

/// A drained group ready for one `GENERATION_COMMIT`.
#[derive(Debug)]
pub struct GroupBatch {
    pub first_pos: i64,
    pub last_pos: i64,
    pub tokens: Vec<(i64, u32)>,
    /// `snapshot(last_pos)` — the checkpoint the commit embeds (I19).
    pub snapshot: Vec<u8>,
}

/// Group-commit accumulator (spec §3: k = 8 / 50 ms). Buffers `(output_pos, token_id, snapshot)`
/// tuples; the last tuple's snapshot is the one a flush embeds (`snapshot(last_output_pos)`). The
/// 50 ms deadline is applied by the async generation loop; this type owns the count threshold.
#[derive(Default)]
pub struct GroupCommitter {
    entries: Vec<(i64, u32)>,
    last_snapshot: Vec<u8>,
    first_pos: Option<i64>,
    k: usize,
}

impl GroupCommitter {
    pub fn new(k: usize) -> Self {
        GroupCommitter { entries: Vec::new(), last_snapshot: Vec::new(), first_pos: None, k: k.max(1) }
    }

    pub fn push(&mut self, output_pos: i64, token_id: u32, snapshot: Vec<u8>) {
        self.first_pos.get_or_insert(output_pos);
        self.entries.push((output_pos, token_id));
        self.last_snapshot = snapshot;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The count threshold has been reached (k tokens buffered).
    pub fn count_ready(&self) -> bool {
        self.entries.len() >= self.k
    }

    /// Drain the buffered group: `(first_pos, last_pos, tokens, snapshot(last_pos))`, or `None` if
    /// empty.
    pub fn take(&mut self) -> Option<GroupBatch> {
        if self.entries.is_empty() {
            return None;
        }
        let first_pos = self.first_pos.take().unwrap();
        let last_pos = self.entries.last().unwrap().0;
        let tokens = std::mem::take(&mut self.entries);
        let snapshot = std::mem::take(&mut self.last_snapshot);
        Some(GroupBatch { first_pos, last_pos, tokens, snapshot })
    }
}
