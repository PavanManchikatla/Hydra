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
    /// **Audit H9.** An earlier append failed after bytes may have reached the disk, so the stream
    /// is poisoned: every later append is refused **without touching the sink**. Distinct from a
    /// fresh `Wal(Io)` error, and deliberately so — "this write failed" and "this stream can no
    /// longer be written to" are different facts, and a caller that cannot tell them apart will
    /// retry the first forever and mistake the second for bad luck.
    #[error("commit stream poisoned by an earlier failed append; nothing further may be written ({why})")]
    Poisoned { why: String },
    /// **Audit M7.** A `GENERATION_COMMIT` that does not continue the durable output sequence, or
    /// whose token entries are not dense. Refused before the disk: recovery replays this ledger by
    /// position, so a hole shifts every later token and nothing reports it.
    #[error("{what}: got {got}, expected {expected} (audit M7 — the generation ledger must be contiguous)")]
    NonContiguous { what: &'static str, got: i64, expected: i64 },
    /// **Audit M5.** A sampled `token_id` outside `[0, n_vocab)` was refused before it could be
    /// buffered for a durable `GENERATION_COMMIT`. Nothing was written.
    #[error("token_id {token_id} at output_pos {output_pos} is outside the vocabulary (n_vocab {n_vocab}); refused before durability (audit M5)")]
    TokenOutOfVocab { output_pos: i64, token_id: u32, n_vocab: u32 },
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
    /// Audit H9: set by the first failed append; refuses every later one. See [`CommitError::Poisoned`].
    poisoned: Option<String>,
    generation_durable_pos: i64,
    committed_sampler_checkpoint_id: u64,
    next_commit_id: u64,
    last_commit_id: u64,
    /// Input-side watermark (spec §2.4). Advances **only** after a durable `INPUT_CHUNK_COMMIT`.
    prefill_stable_pos: i64,
}

impl CommitStream {
    /// Create the session's commit-stream segment file under `dir` (the file header is `fdatasync`'d
    /// and the directory `fsync`'d before any record — WAL-FORMAT §3.2).
    pub fn create(path: impl AsRef<std::path::Path>, cluster_id: [u8; 16], session_id: [u8; 16]) -> Result<CommitStream, CommitError> {
        let header = FileHeader { flags: 0, cluster_id, session_scope: session_id };
        let writer = WalWriter::create(path, &header)?;
        Ok(Self::with_durability(Box::new(writer)))
    }

    /// **Audit H10 — reopen an existing commit stream after a coordinator restart, restoring
    /// EVERY watermark from the durable prefix, and discarding any partial tail DURABLY.**
    ///
    /// # What did not exist before, and what that cost
    ///
    /// There was no `open`. A restarting coordinator could only `create` (which refuses an
    /// existing file) or build a fresh stream over a new sink — so the restart-and-continue path
    /// had **no implementation and therefore no oracle**: `recovery::read` reconstructed the
    /// *token ledger* from the file, and nothing reconstructed the *stream's own* state. A
    /// coordinator that resumed appending would have done so with `generation_durable_pos = -1`,
    /// `prefill_stable_pos = -1`, `committed_sampler_checkpoint_id = 0` and `next_commit_id = 1`,
    /// which means:
    ///
    /// * **commit ids restart at 1**, re-using ids already on disk and breaking the
    ///   `previous_commit_id` chain that makes the ledger self-linking;
    /// * **`prefill_stable_pos = -1` accepts a chunk that moves the input frontier backwards** —
    ///   the monotonicity check exists but has nothing to compare against;
    /// * **the emit-after-commit gate re-opens**: `generation_durable_pos = -1` means every
    ///   already-durable position looks un-emitted.
    ///
    /// # The durable discard
    ///
    /// A crash can leave a partially-written record. `WalScan` decides where the durable prefix
    /// ends (and since H8 **refuses** a log whose damage is not a tail); `WalWriter::open_append`
    /// then truncates to that boundary **and `fdatasync`s the truncation** before this returns, so
    /// a second crash cannot resurrect the discarded bytes. The discard is a durable act, not a
    /// bookkeeping one.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<CommitStream, CommitError> {
        let path = path.as_ref();
        let scan = hydra_wal::reader::WalScan::open(path)?;

        let mut generation_durable_pos: i64 = -1;
        let mut prefill_stable_pos: i64 = -1;
        let mut committed_sampler_checkpoint_id: u64 = 0;
        let mut last_commit_id: u64 = 0;

        for r in &scan.records {
            match r.record_type {
                rec_type::INITIAL_COMMIT => {
                    let ic = flatbuffers::root::<wal::InitialCommit>(&r.payload)
                        .map_err(|e| CommitError::BadCheckpoint(format!("INITIAL_COMMIT at {}: {e}", r.offset)))?;
                    committed_sampler_checkpoint_id = ic.initial_checkpoint().checkpoint_id();
                }
                rec_type::GENERATION_COMMIT => {
                    // Re-validate I19 on the way in: the reader already does it, and doing it here
                    // too costs nothing and keeps this constructor honest about what it accepts.
                    validate_generation_commit_i19(&r.payload).map_err(CommitError::I19)?;
                    let gc = flatbuffers::root::<wal::GenerationCommit>(&r.payload)
                        .map_err(|e| CommitError::BadCheckpoint(format!("GENERATION_COMMIT at {}: {e}", r.offset)))?;
                    generation_durable_pos = gc.last_output_pos();
                    committed_sampler_checkpoint_id = gc.checkpoint().checkpoint_id();
                    last_commit_id = last_commit_id.max(gc.commit_id());
                }
                rec_type::INPUT_CHUNK_COMMIT => {
                    let icc = flatbuffers::root::<wal::InputChunkCommit>(&r.payload)
                        .map_err(|e| CommitError::BadCheckpoint(format!("INPUT_CHUNK_COMMIT at {}: {e}", r.offset)))?;
                    prefill_stable_pos = icc.last_input_pos();
                }
                _ => {}
            }
        }

        // The DURABLE discard: truncate to the scanned prefix and fdatasync before any append.
        let writer = WalWriter::open_append(path, scan.durable_len)?;

        Ok(CommitStream {
            writer: Box::new(writer),
            poisoned: None,
            generation_durable_pos,
            committed_sampler_checkpoint_id,
            next_commit_id: last_commit_id + 1,
            last_commit_id,
            prefill_stable_pos,
        })
    }

    /// Build over an arbitrary [`Durability`] sink (tests: a stalling/failing `fdatasync` to prove
    /// the emit-after-commit gate by absence).
    pub fn with_durability(writer: Box<dyn Durability>) -> CommitStream {
        CommitStream {
            writer,
            poisoned: None,
            generation_durable_pos: -1,
            committed_sampler_checkpoint_id: 0,
            next_commit_id: 1,
            last_commit_id: 0,
            prefill_stable_pos: -1,
        }
    }

    /// Input-side watermark (spec §2.4): the last input position whose chunk is **durably**
    /// committed. `-1` = none. Never advanced by anything but a completed
    /// [`Self::append_input_chunk_commit`].
    pub fn prefill_stable_pos(&self) -> i64 {
        self.prefill_stable_pos
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

    /// Audit H9: has an append failed, leaving the on-disk tail unknown?
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// **Is this stream still writable at all?** (audit H9, ordered before audit M7's checks.)
    ///
    /// A poisoned stream's on-disk tail is *unknown*, which means `generation_durable_pos` no
    /// longer reliably describes the file — so reasoning about **contiguity** against it would be
    /// reasoning from a number we do not trust. "This stream is dead" is the stronger and more
    /// useful answer, and it must be the one the caller gets.
    fn ensure_writable(&self) -> Result<(), CommitError> {
        match &self.poisoned {
            Some(why) => Err(CommitError::Poisoned { why: why.clone() }),
            None => Ok(()),
        }
    }

    /// The single append path. **Every** durable write in this type goes through here, so the H9
    /// poisoning cannot be forgotten at one call site: a new record type gets the check by
    /// construction rather than by review.
    fn durable_append(&mut self, record_type: u16, payload: &[u8]) -> Result<u64, CommitError> {
        if let Some(why) = &self.poisoned {
            return Err(CommitError::Poisoned { why: why.clone() });
        }
        match self.writer.append(record_type, 0, payload) {
            Ok(off) => Ok(off),
            Err(e) => {
                // The bytes may or may not have landed; the stream is no longer writable either
                // way. Record the FIRST cause — it is the one that explains the file.
                self.poisoned = Some(e.to_string());
                Err(CommitError::Wal(e))
            }
        }
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
        // H9 before M7: a dead stream is a stronger fact than a mis-ordered position.
        self.ensure_writable()?;
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
        self.durable_append(rec_type::INITIAL_COMMIT, fbb.finished_data())?;
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
        // H9 before M7: a dead stream is a stronger fact than a mis-ordered position.
        self.ensure_writable()?;
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

        // **Audit M7 — the generation ledger must be CONTIGUOUS, and it was never checked.**
        //
        // `generation_durable_pos` advanced to whatever `last_output_pos` a record carried, so a
        // group that started anywhere at all was accepted. Two ways that happens without malice:
        // a **failed commit dropped its batch** (fixed above) and the next group began past the
        // hole; or a retry re-committed a range already on disk. Either leaves a ledger whose
        // positions are not a dense sequence — and `recovery::read` rebuilds `generated_tokens`
        // by *position order*, then replays them as the token history, so a hole silently shifts
        // every later token one place and the "byte-identical to an uninterrupted run" property
        // is false with nothing reporting it.
        //
        // The rule is exact: a group must begin at `generation_durable_pos + 1`, and its entries
        // must be dense and ascending within the group. Both are refused **before** the disk.
        if first_output_pos != self.generation_durable_pos + 1 {
            return Err(CommitError::NonContiguous {
                what: "GENERATION_COMMIT first_output_pos",
                got: first_output_pos,
                expected: self.generation_durable_pos + 1,
            });
        }
        for (i, (pos, _)) in tokens.iter().enumerate() {
            let want = first_output_pos + i as i64;
            if *pos != want {
                return Err(CommitError::NonContiguous {
                    what: "GENERATION_COMMIT token entry",
                    got: *pos,
                    expected: want,
                });
            }
        }
        if tokens.last().map(|(p, _)| *p) != Some(last_output_pos) {
            return Err(CommitError::NonContiguous {
                what: "GENERATION_COMMIT last_output_pos vs its entries",
                got: last_output_pos,
                expected: tokens.last().map(|(p, _)| *p).unwrap_or(first_output_pos),
            });
        }

        self.durable_append(rec_type::GENERATION_COMMIT, payload)?;
        // Durable now (append fdatasync'd) — only now advance the watermarks.
        self.generation_durable_pos = last_output_pos;
        self.committed_sampler_checkpoint_id =
            flatbuffers::root::<wal::GenerationCommit>(payload).expect("just built").checkpoint().checkpoint_id();
        self.last_commit_id = commit_id;
        self.next_commit_id += 1;
        Ok(commit_id)
    }

    /// `INPUT_CHUNK_COMMIT` for the input chunk `[first_input_pos ..= last_input_pos]` (spec §2.4).
    ///
    /// **The caller must already hold the chunk's admission evidence** — S_P's `APPLIED_ACK(b−1)`
    /// and, in a mode that requires it, the `DURABILITY_ACK` for every boundary edge — because this
    /// record is the *durable consequence* of that evidence, not a request for it.
    /// `boundary_durable_through` carries the per-edge durable frontier so a recovering coordinator
    /// can tell what was safe at the moment the chunk committed.
    ///
    /// **`prefill_stable_pos` advances only after the `fdatasync`'d append returns** — the same
    /// emit-after-commit discipline the generation side uses. A stalled or failing `fdatasync`
    /// leaves the watermark exactly where it was, so an interrupted prefill truncates to a position
    /// that is genuinely on disk.
    ///
    /// Refuses a non-monotonic chunk: prefill is an append-only advance of the input frontier, and
    /// a chunk that would move the watermark backwards is a caller defect, never a silent no-op.
    pub fn append_input_chunk_commit(
        &mut self,
        fence: &WalFenceCtx,
        segment_id: u32,
        chunk_id: u32,
        first_input_pos: i64,
        last_input_pos: i64,
        boundary_durable_through: &[i64],
    ) -> Result<u64, CommitError> {
        // H9 before M7: a dead stream is a stronger fact than a mis-ordered position.
        self.ensure_writable()?;
        if last_input_pos < first_input_pos {
            return Err(CommitError::I19(format!(
                "INPUT_CHUNK_COMMIT: inverted chunk [{first_input_pos}, {last_input_pos}]"
            )));
        }
        if last_input_pos <= self.prefill_stable_pos {
            return Err(CommitError::I19(format!(
                "INPUT_CHUNK_COMMIT: last_input_pos {last_input_pos} does not advance \
                 prefill_stable_pos {} — prefill only moves the input frontier forward",
                self.prefill_stable_pos
            )));
        }
        let commit_id = self.next_commit_id;
        let mut fbb = FlatBufferBuilder::new();
        let fence_off = build_fence(&mut fbb, fence);
        let durable_off = fbb.create_vector(boundary_durable_through);
        let icc = wal::InputChunkCommit::create(
            &mut fbb,
            &wal::InputChunkCommitArgs {
                fence: Some(fence_off),
                segment_id,
                chunk_id,
                first_input_pos,
                last_input_pos,
                boundary_durable_through: Some(durable_off),
            },
        );
        fbb.finish(icc, None);
        self.durable_append(rec_type::INPUT_CHUNK_COMMIT, fbb.finished_data())?;
        // Durable now — and only now does the input watermark move.
        self.prefill_stable_pos = last_input_pos;
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

    /// **Audit M7 — look at the group WITHOUT draining it, so a failed commit does not lose it.**
    ///
    /// [`Self::take`] hands the entries out and clears the buffer in one step, which is correct
    /// only if the append that follows always succeeds. It does not: a `fdatasync` can fail
    /// (`ENOSPC`, EIO), and the caller's `?` then returns with the batch **already gone** — tokens
    /// S_P sampled, that no record on disk mentions, and that the next group's `first_pos` skips
    /// straight past. **I9 says a session fails explicitly rather than silently losing committed
    /// output**; dropping the batch is the silent branch.
    ///
    /// The pair is `peek` → append → [`Self::confirm`]: the buffer is cleared **only after** the
    /// durable write returns, so a failure leaves the group exactly where it was and the caller may
    /// retry it or fail the session — but it cannot lose it by accident.
    pub fn peek(&self) -> Option<GroupBatch> {
        if self.entries.is_empty() {
            return None;
        }
        Some(GroupBatch {
            first_pos: self.first_pos.expect("first_pos is set whenever entries is non-empty"),
            last_pos: self.entries.last().expect("non-empty").0,
            tokens: self.entries.clone(),
            snapshot: self.last_snapshot.clone(),
        })
    }

    /// Clear the group that [`Self::peek`] returned, **after** its record is durable (audit M7).
    pub fn confirm(&mut self) {
        self.first_pos = None;
        self.entries.clear();
        self.last_snapshot.clear();
    }

    /// Drain the buffered group: `(first_pos, last_pos, tokens, snapshot(last_pos))`, or `None` if
    /// empty.
    ///
    /// **Prefer [`Self::peek`] + [`Self::confirm`] on any path where the append can fail** (audit
    /// M7). This remains for callers that are not writing to a disk.
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
