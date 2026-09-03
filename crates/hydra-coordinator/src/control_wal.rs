//! **M4·0 — the coordinator's control WAL (spec §1.2): WAL-before-wire, made real.**
//!
//! `hydra_state::Coordinator` emits [`Effect::WriteWal`] and refuses to act on the record until it
//! observes [`CoordEvent::WalDurable`]. Until this module existed **nothing executed that effect**:
//! the SM was constructed only by the simulator, and the shipping path hand-rolled a
//! `COMMIT_ACTIVATION` → `FINALIZE_ACTIVATION` pair with no intent record, no completion record,
//! and no abort record. So §6.5's restart classification — which reads exactly these records to
//! decide what a restarted coordinator was in the middle of — had nothing to read.
//!
//! The record types are the spec's own (`wal-records.fbs` ids 12–16) and the payloads are the
//! authoritative FlatBuffers tables; there are no shadow structs (BLUEPRINT §2 item 4).

use flatbuffers::FlatBufferBuilder;
use hydra_proto::wal;
use hydra_state::{ActivationTuple, WalRecord};
use hydra_wal::file::FileHeader;
use hydra_wal::record::rec_type;
use hydra_wal::writer::WalWriter;

use crate::commit_stream::{build_fence, WalFenceCtx};

#[derive(Debug, thiserror::Error)]
pub enum ControlWalError {
    #[error("wal: {0}")]
    Wal(#[from] hydra_wal::WalError),
}

/// The coordinator's durable control log.
///
/// Separate file from the commit stream in this slice. The spec permits sharing one physical log
/// (the record types are disjoint) and that remains open; keeping them apart here means a commit
/// stream poisoned by an I/O error (audit H9) does not also take down the control plane's ability
/// to record an abort, which is the record you most want written when things are going wrong.
pub struct ControlWal {
    writer: WalWriter,
}

impl ControlWal {
    pub fn create(path: impl AsRef<std::path::Path>, cluster_id: [u8; 16], session_id: [u8; 16]) -> Result<ControlWal, ControlWalError> {
        let header = FileHeader { flags: hydra_wal::file::FLAG_CONTAINS_CONTROL_WAL, cluster_id, session_scope: session_id };
        Ok(ControlWal { writer: WalWriter::create(path, &header)? })
    }

    /// Reopen after a restart, discarding any partial tail durably, and return the records that
    /// survived — the input to §6.5's classification.
    pub fn open(path: impl AsRef<std::path::Path>, cluster_id: &[u8; 16], session_id: &[u8; 16]) -> Result<(ControlWal, Vec<WalRecord>), ControlWalError> {
        let path = path.as_ref();
        // Audit M8: a control log for another session must not classify this one's restart.
        let scan = hydra_wal::reader::WalScan::open_for_session(path, cluster_id, session_id)?;
        let mut records: Vec<WalRecord> = Vec::new();
        for r in &scan.records {
            if let Some(mut rec) = decode_control_record(r.record_type, &r.payload) {
                // A COMPLETE stores only its tuple's completion hash; the whole tuple (kind,
                // checkpoint id) is the INTENT's, and §6.5a's derivation and H2's evidence both
                // need it whole — resolve it from the INTENT that hashes to the same value.
                if let WalRecord::ActivationComplete { tuple, .. } = &mut rec {
                    if let Ok(cr) = flatbuffers::root::<wal::ActivationCompleteRec>(&r.payload) {
                        let h = cr.tuple_hash().bytes();
                        if let Some(t) = records.iter().rev().find_map(|x| match x {
                            WalRecord::ActivationCommitIntent { tuple: t } if t.completion_hash().as_slice() == h => Some(t.clone()),
                            _ => None,
                        }) {
                            *tuple = t;
                        }
                    }
                }
                records.push(rec);
            }
        }
        let writer = WalWriter::open_append(path, scan.durable_len)?;
        Ok((ControlWal { writer }, records))
    }

    /// Append one control record and return **only after it is durable** (`fdatasync`).
    ///
    /// The caller feeds `CoordEvent::WalDurable` **after** this returns, never before: that
    /// ordering is the entire content of "WAL-before-wire", and the window between the write and
    /// the durability is the crash window §6.5 exists to classify.
    pub fn append(&mut self, fence: &WalFenceCtx, record: &WalRecord) -> Result<(), ControlWalError> {
        let (rt, payload) = encode_control_record(fence, record);
        self.writer.append(rt, 0, &payload)?;
        Ok(())
    }

    pub fn durable_len(&self) -> u64 {
        self.writer.len()
    }
}

/// The fence a record is stamped with is the record's OWN (epoch, recovery_id, attempt) — never the
/// caller's static context. The decoder reads those three fields back from the fence (a COMPLETE
/// stores only a tuple hash, a BEGIN stores no recovery id of its own), so a record written under
/// a static fence read back as (0, 0, 0) after a real restart: the product's first restart oracle
/// found every COMPLETE resurfacing as `{epoch 0, rid 0, attempt 0}` and the fence-forward BEGIN's
/// recovery id as 0 (2026-09-03). The in-memory harness restarts never reread the disk, so none of
/// them could see it (rule 27).
fn record_fence(base: &WalFenceCtx, record: &WalRecord) -> WalFenceCtx {
    let mut f = base.clone();
    match record {
        WalRecord::ActivationCommitIntent { tuple } | WalRecord::ActivationComplete { tuple, .. } => {
            f.epoch = tuple.epoch;
            f.recovery_id = tuple.recovery_id;
            f.activation_attempt_id = tuple.attempt;
        }
        WalRecord::ActivationAbort { epoch, recovery_id, attempt } => {
            f.epoch = *epoch;
            f.recovery_id = *recovery_id;
            f.activation_attempt_id = *attempt;
        }
        WalRecord::BeginRecovery { target, recovery_id, .. } => {
            f.epoch = *target;
            f.recovery_id = *recovery_id;
            f.activation_attempt_id = 0;
        }
        WalRecord::ResetRecoveryAttempt { target, new_recovery_id, .. } => {
            f.epoch = *target;
            f.recovery_id = *new_recovery_id;
            f.activation_attempt_id = 0;
        }
        WalRecord::ActivationUnservable { .. } | WalRecord::SessionTerminate => {}
    }
    f
}

fn encode_control_record(fence: &WalFenceCtx, record: &WalRecord) -> (u16, Vec<u8>) {
    let mut fbb = FlatBufferBuilder::new();
    let stamped = record_fence(fence, record);
    let fence = &stamped;
    match record {
        WalRecord::ActivationCommitIntent { tuple } => {
            let f = build_fence(&mut fbb, fence);
            let t = build_tuple(&mut fbb, tuple);
            let rec = wal::ActivationIntentRec::create(&mut fbb, &wal::ActivationIntentRecArgs { fence: Some(f), tuple: Some(t) });
            fbb.finish(rec, None);
            (rec_type::ACTIVATION_COMMIT_INTENT, fbb.finished_data().to_vec())
        }
        WalRecord::ActivationComplete { tuple, completion_id } => {
            let f = build_fence(&mut fbb, fence);
            // The tuple hash is the completion evidence a stage will be asked to match (audit H2).
            // ONE definition, in `hydra-state`, computed by both peers (audit H2).
            let hash = tuple.completion_hash();
            let th = fbb.create_vector(&hash);
            let rec = wal::ActivationCompleteRec::create(
                &mut fbb,
                &wal::ActivationCompleteRecArgs { fence: Some(f), completion_id: *completion_id, tuple_hash: Some(th) },
            );
            fbb.finish(rec, None);
            (rec_type::ACTIVATION_COMPLETE, fbb.finished_data().to_vec())
        }
        WalRecord::ActivationAbort { attempt, .. } => {
            let f = build_fence(&mut fbb, fence);
            let rec = wal::ActivationAbortRec::create(
                &mut fbb,
                &wal::ActivationAbortRecArgs { fence: Some(f), aborted_attempt_id: *attempt, next_attempt_id: attempt.saturating_add(1) },
            );
            fbb.finish(rec, None);
            (rec_type::ACTIVATION_ABORT, fbb.finished_data().to_vec())
        }
        WalRecord::ActivationUnservable { completion_id } => {
            let f = build_fence(&mut fbb, fence);
            let ranks = fbb.create_vector::<u16>(&[]);
            let rec = wal::ActivationUnservableRec::create(
                &mut fbb,
                &wal::ActivationUnservableRecArgs {
                    fence: Some(f),
                    completion_id: *completion_id,
                    failed_stage_ranks: Some(ranks),
                    predecessor_completion_id: 0,
                },
            );
            fbb.finish(rec, None);
            (rec_type::ACTIVATION_UNSERVABLE, fbb.finished_data().to_vec())
        }
        // M4·0b — the records §6.5's classifier reads to tell a recovery from an activation.
        // The recovery id travels in the record's FENCE (spec §1.1), not beside it — which is why
        // the field is not read here and is recovered from `fence().recovery_id()` on decode.
        WalRecord::BeginRecovery { base, target, recovery_id: _, truncate_to } => {
            let f = build_fence(&mut fbb, fence);
            let rec = wal::BeginRecoveryRec::create(
                &mut fbb,
                &wal::BeginRecoveryRecArgs {
                    fence: Some(f),
                    base_epoch: *base,
                    target_epoch: *target,
                    truncate_to_input_pos: *truncate_to,
                },
            );
            fbb.finish(rec, None);
            (rec_type::BEGIN_RECOVERY, fbb.finished_data().to_vec())
        }
        WalRecord::ResetRecoveryAttempt { target, old_recovery_id, new_recovery_id, truncate_to } => {
            let f = build_fence(&mut fbb, fence);
            let _ = target;
            let rec = wal::ResetAttemptRec::create(
                &mut fbb,
                &wal::ResetAttemptRecArgs {
                    fence: Some(f),
                    old_recovery_id: *old_recovery_id,
                    new_recovery_id: *new_recovery_id,
                    truncate_to_input_pos: *truncate_to,
                    committed_checkpoint_id: 0,
                },
            );
            fbb.finish(rec, None);
            (rec_type::RESET_RECOVERY_ATTEMPT, fbb.finished_data().to_vec())
        }
        WalRecord::SessionTerminate => {
            let f = build_fence(&mut fbb, fence);
            let reason = fbb.create_string("session terminated");
            let rec = wal::SessionTerminateRec::create(&mut fbb, &wal::SessionTerminateRecArgs { fence: Some(f), reason: Some(reason) });
            fbb.finish(rec, None);
            (rec_type::SESSION_TERMINATE, fbb.finished_data().to_vec())
        }
    }
}

/// Read a control record back. Returns `None` for a type this log does not own.
fn decode_control_record(rt: u16, payload: &[u8]) -> Option<WalRecord> {
    match rt {
        rec_type::ACTIVATION_COMMIT_INTENT => {
            let r = flatbuffers::root::<wal::ActivationIntentRec>(payload).ok()?;
            Some(WalRecord::ActivationCommitIntent { tuple: tuple_from_wal(r.tuple()) })
        }
        rec_type::ACTIVATION_COMPLETE => {
            let r = flatbuffers::root::<wal::ActivationCompleteRec>(payload).ok()?;
            // The tuple is not stored whole in a COMPLETE record — only its hash — so the record
            // that survives a restart carries the completion id and the evidence, which is what
            // §6.5 and H2 need. The tuple itself is recovered from the INTENT record.
            let f = r.fence();
            Some(WalRecord::ActivationComplete {
                tuple: ActivationTuple {
                    kind: hydra_state::ActivationKind::Recovery,
                    epoch: f.session_epoch(),
                    recovery_id: f.recovery_id(),
                    attempt: f.activation_attempt_id(),
                    sampler_checkpoint_id: 0,
                },
                completion_id: r.completion_id(),
            })
        }
        rec_type::ACTIVATION_ABORT => {
            let r = flatbuffers::root::<wal::ActivationAbortRec>(payload).ok()?;
            let f = r.fence();
            Some(WalRecord::ActivationAbort { epoch: f.session_epoch(), recovery_id: f.recovery_id(), attempt: r.aborted_attempt_id() })
        }
        rec_type::ACTIVATION_UNSERVABLE => {
            let r = flatbuffers::root::<wal::ActivationUnservableRec>(payload).ok()?;
            Some(WalRecord::ActivationUnservable { completion_id: r.completion_id() })
        }
        rec_type::BEGIN_RECOVERY => {
            let r = flatbuffers::root::<wal::BeginRecoveryRec>(payload).ok()?;
            Some(WalRecord::BeginRecovery {
                base: r.base_epoch(),
                target: r.target_epoch(),
                // The recovery id lives in the record's fence (spec §1.1), not beside it.
                recovery_id: r.fence().recovery_id(),
                truncate_to: r.truncate_to_input_pos(),
            })
        }
        rec_type::RESET_RECOVERY_ATTEMPT => {
            let r = flatbuffers::root::<wal::ResetAttemptRec>(payload).ok()?;
            Some(WalRecord::ResetRecoveryAttempt {
                target: r.fence().session_epoch(),
                old_recovery_id: r.old_recovery_id(),
                new_recovery_id: r.new_recovery_id(),
                truncate_to: r.truncate_to_input_pos(),
            })
        }
        rec_type::SESSION_TERMINATE => Some(WalRecord::SessionTerminate),
        _ => None,
    }
}

fn build_tuple<'a>(fbb: &mut FlatBufferBuilder<'a>, t: &ActivationTuple) -> flatbuffers::WIPOffset<hydra_proto::proto::ActivationTuple<'a>> {
    let gens = fbb.create_vector::<u64>(&[]);
    let applied = fbb.create_vector::<i64>(&[]);
    let checksum = fbb.create_vector::<u8>(&[0u8; 32]);
    hydra_proto::proto::ActivationTuple::create(
        fbb,
        &hydra_proto::proto::ActivationTupleArgs {
            kind: match t.kind {
                hydra_state::ActivationKind::Initial => hydra_proto::proto::ActivationKind::INITIAL,
                hydra_state::ActivationKind::Recovery => hydra_proto::proto::ActivationKind::RECOVERY,
            },
            epoch: t.epoch,
            recovery_id: t.recovery_id,
            activation_attempt_id: t.attempt,
            placement_version: 0,
            logical_context_id: 0,
            shard_generations: Some(gens),
            recovery_goal_input_pos: 0,
            expected_applied_input_pos: Some(applied),
            expected_next_output_pos: 0,
            sampler_checkpoint_id: t.sampler_checkpoint_id,
            sampler_state_checksum: Some(checksum),
        },
    )
}

fn tuple_from_wal(t: hydra_proto::proto::ActivationTuple<'_>) -> ActivationTuple {
    ActivationTuple {
        kind: match t.kind() {
            hydra_proto::proto::ActivationKind::RECOVERY => hydra_state::ActivationKind::Recovery,
            _ => hydra_state::ActivationKind::Initial,
        },
        epoch: t.epoch(),
        recovery_id: t.recovery_id(),
        attempt: t.activation_attempt_id(),
        sampler_checkpoint_id: t.sampler_checkpoint_id(),
    }
}
