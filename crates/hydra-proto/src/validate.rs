//! Cross-cutting validation of wire/WAL payloads that the schema alone can't express.

use crate::wal::GenerationCommit;

/// Validate a `GENERATION_COMMIT` payload against invariant **I19** (spec §2.6a):
/// the embedded sampler checkpoint must satisfy
/// `generated_through == sampled_pos == last_output_pos` — one record or nothing.
/// Called on both write and read (WAL-FORMAT.md §2). `payload` is a finished FlatBuffer with
/// `GenerationCommit` as its root.
pub fn validate_generation_commit_i19(payload: &[u8]) -> Result<(), String> {
    let gc = flatbuffers::root::<GenerationCommit>(payload)
        .map_err(|e| format!("not a valid GenerationCommit flatbuffer: {e}"))?;
    let ckpt = gc.checkpoint();
    let last = gc.last_output_pos();
    let generated_through = ckpt.generated_through_output_pos();
    let sampled = ckpt.sampled_output_pos();
    if generated_through != last || sampled != last {
        return Err(format!(
            "I19: generated_through={generated_through}, sampled_pos={sampled}, \
             last_output_pos={last} (require all equal)"
        ));
    }
    Ok(())
}
