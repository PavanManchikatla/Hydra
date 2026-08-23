//! The signed shard manifest (BLUEPRINT §1.7 / §1.9): the contract a worker verifies before loading
//! a shard. It binds, per model: a **per-tensor BLAKE3** map, the three **admission hashes**
//! (tokenizer / chat-template / inference-config, computed over the GGUF's own metadata so this stays
//! engine-free), and the **layer-range map** — all **Ed25519-signed**. A worker that cannot verify the
//! signature, or whose shard bytes don't match, **refuses the shard** (a structured error, never a
//! warning — the security posture extends to model distribution).
//!
//! Serialization is a **canonical length-prefixed binary** form (no serde, no float ordering issues),
//! so the signed bytes are a deterministic function of the inputs and a re-split of the same GGUF with
//! the same key yields a byte-identical manifest (Ed25519 signing is itself deterministic, RFC 8032).

use ring::signature::{self, Ed25519KeyPair, KeyPair};

use crate::gguf::{Gguf, GgufValue};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("signature verification failed (shard manifest not trusted)")]
    BadSignature,
    #[error("manifest is unsigned")]
    Unsigned,
    #[error("ed25519 key error: {0}")]
    Key(String),
    #[error("truncated manifest at offset {0}")]
    Truncated(usize),
    #[error("bad manifest magic/version")]
    BadHeader,
    #[error("invalid utf-8 in manifest")]
    BadUtf8,
    /// **C1.** The manifest is signed by a key that is not the one this cluster trusts.
    #[error("shard manifest is signed by an UNTRUSTED key (got {got}, cluster trusts {want})")]
    UntrustedSigner { got: String, want: String },
    /// **H14.** The manifest verifies, but it is not the manifest this session's fence tuple names.
    #[error("manifest identity mismatch: blake3(manifest) = {got}, fence tuple says {want}")]
    ManifestHashMismatch { got: String, want: String },
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect::<String>() + "…"
}

type R<T> = Result<T, ManifestError>;

const MANIFEST_MAGIC: &[u8; 8] = b"HYDRAMF1";

/// One shard's entry: its stage index, the inclusive-exclusive layer range it carries, the shard
/// file name, the BLAKE3 of the whole shard file, and the per-tensor BLAKE3 list (sorted by name).
#[derive(Debug, Clone, PartialEq)]
pub struct ShardEntry {
    pub stage: u32,
    pub layer_first: u32,
    pub layer_last: u32,
    pub file_name: String,
    pub shard_blake3: [u8; 32],
    /// (tensor name, BLAKE3 of its data), sorted by name.
    pub tensors: Vec<(String, [u8; 32])>,
}

/// The full manifest. `signature`/`signer_pubkey` are absent until [`Manifest::sign`].
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub architecture: String,
    pub n_layer_total: u32,
    pub tokenizer_hash: [u8; 32],
    pub chat_template_hash: [u8; 32],
    pub inference_config_hash: [u8; 32],
    pub shards: Vec<ShardEntry>,
    pub signer_pubkey: [u8; 32],
    pub signature: Option<[u8; 64]>,
}

impl Manifest {
    /// The **canonical signed bytes** — everything except the signature, in a fixed order. Signing
    /// and verification both run over exactly these bytes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut w = Vec::new();
        w.extend_from_slice(MANIFEST_MAGIC);
        put_str(&mut w, &self.architecture);
        put_u32(&mut w, self.n_layer_total);
        w.extend_from_slice(&self.tokenizer_hash);
        w.extend_from_slice(&self.chat_template_hash);
        w.extend_from_slice(&self.inference_config_hash);
        w.extend_from_slice(&self.signer_pubkey);
        put_u32(&mut w, self.shards.len() as u32);
        for s in &self.shards {
            put_u32(&mut w, s.stage);
            put_u32(&mut w, s.layer_first);
            put_u32(&mut w, s.layer_last);
            put_str(&mut w, &s.file_name);
            w.extend_from_slice(&s.shard_blake3);
            put_u32(&mut w, s.tensors.len() as u32);
            for (name, hash) in &s.tensors {
                put_str(&mut w, name);
                w.extend_from_slice(hash);
            }
        }
        w
    }

    /// Sign the canonical bytes with `keypair`; records the signature + public key.
    pub fn sign(&mut self, keypair: &Ed25519KeyPair) {
        self.signer_pubkey = keypair.public_key().as_ref().try_into().expect("ed25519 pubkey is 32 bytes");
        let msg = self.canonical_bytes();
        let sig = keypair.sign(&msg);
        self.signature = Some(sig.as_ref().try_into().expect("ed25519 sig is 64 bytes"));
    }

    /// Verify the signature **against a pinned, caller-supplied trusted public key**.
    ///
    /// # C1 — why there is no argument-less form
    ///
    /// This function used to be `verify(&self)`, checking the signature against
    /// **`self.signer_pubkey` — the key embedded in the manifest itself.** That is not a
    /// verification; it is a self-attestation. Anyone can `generate_keypair()`, sign a manifest
    /// naming any shard bytes they like, and it verifies perfectly — because the artifact carries
    /// its own answer key. Every downstream check (per-tensor BLAKE3, the layer-range map, the
    /// admission hashes) then validates against *the attacker's* numbers and passes.
    ///
    /// The argument-less form is **deliberately removed from the public API rather than deprecated**:
    /// while it exists it is the shorter call, and the shorter call is the one that gets written.
    /// A signature check whose trust anchor is the thing being checked is worse than no check,
    /// because it terminates the reader's inquiry — the same shape as an over-promising test name.
    ///
    /// The trust anchor is the caller's: for a worker it is the cluster's signing key, provisioned
    /// at pairing alongside the mTLS identity.
    pub fn verify_against(&self, trusted_pubkey: &[u8; 32]) -> R<()> {
        if self.signer_pubkey != *trusted_pubkey {
            return Err(ManifestError::UntrustedSigner {
                got: hex8(&self.signer_pubkey),
                want: hex8(trusted_pubkey),
            });
        }
        let sig = self.signature.as_ref().ok_or(ManifestError::Unsigned)?;
        let pk = signature::UnparsedPublicKey::new(&signature::ED25519, trusted_pubkey);
        pk.verify(&self.canonical_bytes(), sig).map_err(|_| ManifestError::BadSignature)
    }

    /// **H13 — the signature is checked BEFORE the structure is parsed.** This is the only entry
    /// point a consumer of an untrusted manifest file may use.
    ///
    /// `Manifest::from_bytes` walks length-prefixed strings and counted arrays; even with clamped
    /// reservations that is attacker-directed work performed on unauthenticated bytes. Ed25519
    /// verification, by contrast, is a fixed-cost operation over a byte slice that does not care
    /// what the bytes mean. So the order is: split off the 64-byte trailer, verify the remainder
    /// against the pinned key, and only then interpret a single byte of it.
    ///
    /// **H14 — identity binding.** `expected_manifest_hash` is the session fence tuple's
    /// `manifest_hash` (spec §1.1). Without it, a *validly signed manifest for a different model* is
    /// accepted: the cluster's own signing key legitimately signs every model it publishes, so a
    /// signature alone says "this is one of ours", never "this is the one this session agreed on".
    /// Binding `blake3(manifest_bytes)` to the fence tuple closes that substitution — and because
    /// the architecture, `n_layer_total` and all three admission hashes live inside the signed
    /// canonical bytes, pinning that one hash pins the model's identity with them.
    pub fn verify_and_parse(
        bytes: &[u8],
        trusted_pubkey: &[u8; 32],
        expected_manifest_hash: &[u8; 32],
    ) -> R<Manifest> {
        // 1. Split the trailer. No allocation, no interpretation.
        if bytes.len() < 64 {
            return Err(ManifestError::Truncated(0));
        }
        let (msg, sig_bytes) = bytes.split_at(bytes.len() - 64);
        let sig: [u8; 64] = sig_bytes.try_into().expect("split_at(len-64) yields 64 bytes");

        // 2. Verify BEFORE parsing (H13).
        let pk = signature::UnparsedPublicKey::new(&signature::ED25519, trusted_pubkey);
        pk.verify(msg, &sig).map_err(|_| ManifestError::BadSignature)?;

        // 3. Identity (H14) — still before parsing, since it is a hash of the whole file.
        let got = *blake3::hash(bytes).as_bytes();
        if got != *expected_manifest_hash {
            return Err(ManifestError::ManifestHashMismatch {
                got: hex8(&got),
                want: hex8(expected_manifest_hash),
            });
        }

        // 4. Now the bytes are authenticated AND are the ones this session named. Parse them.
        let m = Manifest::from_bytes(bytes)?;

        // 5. The embedded signer must be the trusted one. The signature already proves the trusted
        //    key signed these bytes — but an attacker holding that key could embed a *different*
        //    pubkey, producing a manifest that reads as third-party-signed while verifying as ours.
        //    Refusing that keeps the artifact's self-description honest.
        if m.signer_pubkey != *trusted_pubkey {
            return Err(ManifestError::UntrustedSigner {
                got: hex8(&m.signer_pubkey),
                want: hex8(trusted_pubkey),
            });
        }
        Ok(m)
    }

    /// The full on-disk manifest = canonical bytes + the 64-byte signature.
    pub fn to_bytes(&self) -> R<Vec<u8>> {
        let sig = self.signature.as_ref().ok_or(ManifestError::Unsigned)?;
        let mut w = self.canonical_bytes();
        w.extend_from_slice(sig);
        Ok(w)
    }

    /// Parse a manifest written by [`Manifest::to_bytes`].
    ///
    /// ⚠️ **This performs NO verification.** Its output is untrusted structure. A consumer handling
    /// a manifest that came from anywhere but its own process must use
    /// [`Manifest::verify_and_parse`], which checks the signature *before* reaching this function.
    /// This stays public only for round-tripping manifests this process just produced.
    pub fn from_bytes(bytes: &[u8]) -> R<Manifest> {
        let mut r = Reader { b: bytes, i: 0 };
        if r.take(8)? != MANIFEST_MAGIC {
            return Err(ManifestError::BadHeader);
        }
        let architecture = r.string()?;
        let n_layer_total = r.u32()?;
        let tokenizer_hash = r.arr32()?;
        let chat_template_hash = r.arr32()?;
        let inference_config_hash = r.arr32()?;
        let signer_pubkey = r.arr32()?;
        let n_shards = r.u32()?;
        // H13 / the §7.28-D2 shape again: a declared count may never reserve more memory than the
        // remaining input could justify. A shard entry is at least 4+4+4 + 4(name len) + 32 + 4.
        let mut shards = Vec::with_capacity(r.reserve_for(n_shards as u64, 52));
        for _ in 0..n_shards {
            let stage = r.u32()?;
            let layer_first = r.u32()?;
            let layer_last = r.u32()?;
            let file_name = r.string()?;
            let shard_blake3 = r.arr32()?;
            let n_t = r.u32()?;
            // A tensor entry is at least a 4-byte name length + 32 bytes of hash.
            let mut tensors = Vec::with_capacity(r.reserve_for(n_t as u64, 36));
            for _ in 0..n_t {
                let name = r.string()?;
                let hash = r.arr32()?;
                tensors.push((name, hash));
            }
            shards.push(ShardEntry { stage, layer_first, layer_last, file_name, shard_blake3, tensors });
        }
        // The remaining 64 bytes are the signature.
        let sig: [u8; 64] = r.take(64)?.try_into().map_err(|_| ManifestError::Truncated(r.i))?;
        Ok(Manifest {
            architecture,
            n_layer_total,
            tokenizer_hash,
            chat_template_hash,
            inference_config_hash,
            shards,
            signer_pubkey,
            signature: Some(sig),
        })
    }
}

/// Generate a fresh Ed25519 keypair (PKCS#8 bytes + the keypair). The PKCS#8 is what a signer stores.
pub fn generate_keypair() -> R<(Vec<u8>, Ed25519KeyPair)> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|e| ManifestError::Key(format!("{e:?}")))?;
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|e| ManifestError::Key(format!("{e:?}")))?;
    Ok((pkcs8.as_ref().to_vec(), kp))
}

/// The 32-byte Ed25519 public key of a keypair — the value a cluster pins as its trust anchor.
///
/// Exposed here so a consumer that needs to *name a trust anchor* (audit C1) does not have to take
/// a direct `ring` dependency to do it. Making the correct call the easy one is part of the fix.
pub fn public_key_of(kp: &Ed25519KeyPair) -> [u8; 32] {
    kp.public_key().as_ref().try_into().expect("ed25519 pubkey is 32 bytes")
}

/// The trust anchor stored alongside a signing key: load PKCS#8, return only the public half.
pub fn pubkey_from_pkcs8(pkcs8: &[u8]) -> R<[u8; 32]> {
    Ok(public_key_of(&keypair_from_pkcs8(pkcs8)?))
}

/// Load a keypair from stored PKCS#8 bytes.
pub fn keypair_from_pkcs8(pkcs8: &[u8]) -> R<Ed25519KeyPair> {
    Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| ManifestError::Key(format!("{e:?}")))
}

// ------------------------- admission hashes (engine-free) -------------------------

/// BLAKE3 over the GGUF's **tokenizer** metadata (`tokenizer.ggml.*` — tokens, scores, token types,
/// merges), in a canonical order — the model's tokenizer identity without loading the engine.
pub fn tokenizer_hash(g: &Gguf) -> [u8; 32] {
    hash_meta_prefix(g, "tokenizer.ggml.")
}

/// BLAKE3 over the chat template (`tokenizer.chat_template`), or the hash of empty if absent.
pub fn chat_template_hash(g: &Gguf) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    if let Some(GgufValue::Str(t)) = g.meta("tokenizer.chat_template") {
        h.update(t.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// BLAKE3 over the architecture hyper-parameter metadata (`{arch}.*`, excluding tokenizer fence) — the
/// inference-config identity (context length, dims, heads, rope, etc.).
pub fn inference_config_hash(g: &Gguf) -> [u8; 32] {
    let arch = g.architecture().unwrap_or("");
    let prefix = format!("{arch}.");
    let mut keyed: Vec<(&String, &GgufValue)> = g
        .metadata
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix) && !k.starts_with("tokenizer."))
        .map(|(k, v)| (k, v))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = blake3::Hasher::new();
    for (k, v) in keyed {
        h.update(k.as_bytes());
        h.update(&[0]);
        hash_value(&mut h, v);
    }
    *h.finalize().as_bytes()
}

fn hash_meta_prefix(g: &Gguf, prefix: &str) -> [u8; 32] {
    let mut keyed: Vec<(&String, &GgufValue)> =
        g.metadata.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| (k, v)).collect();
    keyed.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = blake3::Hasher::new();
    for (k, v) in keyed {
        h.update(k.as_bytes());
        h.update(&[0]);
        hash_value(&mut h, v);
    }
    *h.finalize().as_bytes()
}

/// Hash a metadata value canonically (type tag + payload). Recurses into arrays.
fn hash_value(h: &mut blake3::Hasher, v: &GgufValue) {
    match v {
        GgufValue::U8(x) => { h.update(&[0, *x]); }
        GgufValue::I8(x) => { h.update(&[1, *x as u8]); }
        GgufValue::U16(x) => { h.update(&[2]); h.update(&x.to_le_bytes()); }
        GgufValue::I16(x) => { h.update(&[3]); h.update(&x.to_le_bytes()); }
        GgufValue::U32(x) => { h.update(&[4]); h.update(&x.to_le_bytes()); }
        GgufValue::I32(x) => { h.update(&[5]); h.update(&x.to_le_bytes()); }
        GgufValue::F32(x) => { h.update(&[6]); h.update(&x.to_bits().to_le_bytes()); }
        GgufValue::Bool(x) => { h.update(&[7, *x as u8]); }
        GgufValue::Str(s) => { h.update(&[8]); h.update(&(s.len() as u64).to_le_bytes()); h.update(s.as_bytes()); }
        GgufValue::U64(x) => { h.update(&[10]); h.update(&x.to_le_bytes()); }
        GgufValue::I64(x) => { h.update(&[11]); h.update(&x.to_le_bytes()); }
        GgufValue::F64(x) => { h.update(&[12]); h.update(&x.to_bits().to_le_bytes()); }
        GgufValue::Array(a) => {
            h.update(&[9]);
            h.update(&a.elem_type.to_le_bytes());
            h.update(&(a.values.len() as u64).to_le_bytes());
            for e in &a.values {
                hash_value(h, e);
            }
        }
    }
}

// ------------------------- canonical encode/decode helpers -------------------------

fn put_u32(w: &mut Vec<u8>, v: u32) {
    w.extend_from_slice(&v.to_le_bytes());
}
fn put_str(w: &mut Vec<u8>, s: &str) {
    put_u32(w, s.len() as u32);
    w.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }
    /// Clamp an attacker-declared count to what the remaining input could justify — the same
    /// invariant `gguf::Cursor::reserve_for` enforces, for the same reason (§7.28 D2).
    fn reserve_for(&self, declared: u64, min_bytes_each: usize) -> usize {
        (declared as usize).min(self.remaining() / min_bytes_each.max(1))
    }
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or(ManifestError::Truncated(self.i))?;
        if end > self.b.len() {
            return Err(ManifestError::Truncated(self.i));
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn arr32(&mut self) -> R<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn string(&mut self) -> R<String> {
        let n = self.u32()? as u64;
        // Bounds-check before the `as usize` cast and before `to_vec()` allocates.
        if n > self.remaining() as u64 {
            return Err(ManifestError::Truncated(self.i));
        }
        String::from_utf8(self.take(n as usize)?.to_vec()).map_err(|_| ManifestError::BadUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            architecture: "qwen2".into(),
            n_layer_total: 24,
            tokenizer_hash: [1; 32],
            chat_template_hash: [2; 32],
            inference_config_hash: [3; 32],
            shards: vec![ShardEntry {
                stage: 0,
                layer_first: 0,
                layer_last: 14,
                file_name: "shard0.gguf".into(),
                shard_blake3: [9; 32],
                tensors: vec![("blk.0.attn_q.weight".into(), [4; 32]), ("token_embd.weight".into(), [5; 32])],
            }],
            signer_pubkey: [0; 32],
            signature: None,
        }
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper_detection() {
        let (_pk, kp) = generate_keypair().unwrap();
        let trusted = public_key_of(&kp);
        let mut m = sample();
        m.sign(&kp);
        m.verify_against(&trusted).expect("freshly-signed manifest verifies against its signer");

        // Round-trip through bytes.
        let bytes = m.to_bytes().unwrap();
        let back = Manifest::from_bytes(&bytes).unwrap();
        back.verify_against(&trusted).expect("round-tripped manifest verifies");
        assert_eq!(back, m);

        // Tamper with a tensor hash → verification MUST fail (a worker refuses the shard).
        let mut tampered = back.clone();
        tampered.shards[0].tensors[0].1 = [0xff; 32];
        assert!(matches!(tampered.verify_against(&trusted), Err(ManifestError::BadSignature)));
    }

    #[test]
    fn unsigned_manifest_refused() {
        let m = sample();
        let trusted = m.signer_pubkey; // the all-zero placeholder — isolates "unsigned" from "untrusted"
        assert!(matches!(m.verify_against(&trusted), Err(ManifestError::Unsigned)));
    }

    /// **Audit C1, at the unit level.** A manifest signed by *some* key is refused by a *different*
    /// trust anchor — the case the removed argument-less `verify()` could not express, because it
    /// had no anchor to disagree with.
    #[test]
    fn a_manifest_signed_by_another_key_is_refused_by_this_clusters_anchor() {
        let (_a, attacker) = generate_keypair().unwrap();
        let (_b, cluster) = generate_keypair().unwrap();
        let mut m = sample();
        m.sign(&attacker);

        // Internally consistent — the old self-attestation would have passed it.
        m.verify_against(&public_key_of(&attacker)).expect("the forgery is self-consistent");

        match m.verify_against(&public_key_of(&cluster)) {
            Err(ManifestError::UntrustedSigner { .. }) => {}
            other => panic!("a foreign-signed manifest must be refused as UntrustedSigner, got {other:?}"),
        }
    }

    /// **Audit H13 + H14 at the unit level.** `verify_and_parse` refuses an unsigned/foreign file
    /// without parsing it, and refuses a genuine one whose hash the caller did not name.
    #[test]
    fn verify_and_parse_binds_both_the_signer_and_the_manifest_identity() {
        let (_pk, kp) = generate_keypair().unwrap();
        let trusted = public_key_of(&kp);
        let mut m = sample();
        m.sign(&kp);
        let bytes = m.to_bytes().unwrap();
        let hash = *blake3::hash(&bytes).as_bytes();

        // Control: correct key + correct identity.
        Manifest::verify_and_parse(&bytes, &trusted, &hash).expect("genuine manifest, correctly bound");

        // H14: right key, wrong identity — the "genuine manifest for another model" substitution.
        match Manifest::verify_and_parse(&bytes, &trusted, &[0xAB; 32]) {
            Err(ManifestError::ManifestHashMismatch { .. }) => {}
            other => panic!("expected ManifestHashMismatch, got {other:?}"),
        }

        // C1: wrong key.
        let (_o, other_kp) = generate_keypair().unwrap();
        assert!(matches!(
            Manifest::verify_and_parse(&bytes, &public_key_of(&other_kp), &hash),
            Err(ManifestError::BadSignature)
        ));

        // H13: a structurally hostile, unsigned file is refused at the signature — the parser is
        // never reached, so the error is BadSignature rather than Truncated/BadHeader.
        let mut hostile = Vec::new();
        hostile.extend_from_slice(MANIFEST_MAGIC);
        hostile.extend_from_slice(&u32::MAX.to_le_bytes()); // an enormous declared string length
        hostile.extend_from_slice(&[0u8; 64]);
        assert!(matches!(
            Manifest::verify_and_parse(&hostile, &trusted, &hash),
            Err(ManifestError::BadSignature)
        ));
    }
}
