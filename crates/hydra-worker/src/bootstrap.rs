//! Worker **provisioning bootstrap**: the mTLS material + role config a standalone `hydra-worker`
//! process needs to serve, written by the `--local-pair` runner (or, in the field, by M4 pairing)
//! and read on startup. It carries the device's signed cert chain + key and the **CA certificate**
//! (a trust anchor, never the CA's private key), plus the [`WorkerConfig`] role.
//!
//! Format: a tiny length-prefixed binary blob (u32 LE lengths) — no serde dependency, no text
//! parsing of secrets. This is a dev/provisioning artifact; secure handling of the key file is a
//! deployment concern (M4).

use std::io::{self, Read, Write};

use hydra_transport::{CertificateDer, DeviceIdentity};

use crate::sampler::SamplingConfig;
use crate::wire::{SessionFence, CLUSTER_ID_LEN, HASH_LEN, MODEL_INSTANCE_ID_LEN, SESSION_ID_LEN};
use crate::worker::WorkerConfig;

/// The downstream + durability wiring a **forwarding** stage (S1/S2 in a chained pipeline) needs to
/// run [`crate::worker::serve_multi_conn_forwarding_durable`]: where to send each boundary directly
/// (the down-link) and where to copy it for durability (P1·1b seam B). Absent for a final stage (S_P),
/// which samples and never forwards.
#[derive(Clone)]
pub struct ForwardingBootstrap {
    /// Downstream peer address + presented cert name (the direct S1→S2 / S2→S_P boundary link).
    pub down_addr: String,
    pub down_name: String,
    /// Durability target address + presented cert name (`BOUNDARY_COPY` out / `DURABILITY_ACK` back).
    pub dur_addr: String,
    pub dur_name: String,
    /// D1 (release gated on `DURABILITY_ACK`) vs D0 (downstream ack alone).
    pub require_durable: bool,
    /// R3′ retention bound (spec §5) — where the forward path backpressures on durability.
    pub capacity: u32,
}

/// Everything a worker process needs to come up and serve.
pub struct Bootstrap {
    pub listen_addr: String,
    /// The DNS/CN identity this worker presents (what the coordinator uses as `server_name`).
    pub device_name: String,
    pub ca_cert_der: Vec<u8>,
    pub cert_chain_der: Vec<Vec<u8>>,
    pub key_pkcs8_der: Vec<u8>,
    pub cfg: WorkerConfig,
    /// Present ⇒ this is a forwarding stage (durable worker→worker FWD); absent ⇒ a final stage.
    /// Appended to the wire format (append-only), so an older bootstrap without it decodes to `None`.
    pub forwarding: Option<ForwardingBootstrap>,
    /// **Audit C2 — the peers this worker will accept, and the role each may speak as.**
    ///
    /// A worker cannot invent this: which stage its upstream is, and whether the thing dialling it
    /// is the coordinator or a peer stage, are facts the *provisioner* knows. Carrying them here
    /// means the listener's role table is configuration rather than a default — and a bootstrap
    /// that names none is **refused at startup**, not started with an empty (deny-all) table that
    /// would fail confusingly on the first connection.
    pub expected_peers: Vec<(String, u8)>,
}

/// Wire tags for [`hydra_transport::roles::PeerRole`]. Stable by value — a bootstrap written by one
/// build is read by another, so these are append-only like every other wire enum in the project.
pub const ROLE_COORDINATOR: u8 = 0;
pub const ROLE_DURABILITY_TARGET: u8 = 1;
/// Stage ranks are `2 + rank`, so a stage's tag carries its rank without a second field.
pub const ROLE_STAGE_BASE: u8 = 2;

impl Bootstrap {
    /// The listener's role table (audit C2), built from `expected_peers`.
    ///
    /// **Refuses an empty table** rather than returning one. An empty table denies every peer, which
    /// is fail-closed and therefore *safe* — but it is also almost certainly a provisioning mistake,
    /// and a worker that starts and then rejects every connection is harder to diagnose than one
    /// that does not start. Fail closed **and** fail loudly.
    pub fn role_table(&self) -> Result<hydra_transport::roles::RoleTable, String> {
        use hydra_transport::roles::{PeerRole, RoleTable};
        if self.expected_peers.is_empty() {
            return Err("bootstrap names no expected peers: this worker would refuse every \
                        connection (audit C2). Provision the name->role table."
                .to_string());
        }
        let mut t = RoleTable::new();
        for (name, tag) in &self.expected_peers {
            let role = match *tag {
                ROLE_COORDINATOR => PeerRole::Coordinator,
                ROLE_DURABILITY_TARGET => PeerRole::DurabilityTarget,
                t if t >= ROLE_STAGE_BASE => PeerRole::Stage { rank: (t - ROLE_STAGE_BASE) as u16 },
                other => return Err(format!("unknown role tag {other} for peer {name:?}")),
            };
            t = t.with(name, role);
        }
        Ok(t)
    }

    pub fn identity(&self) -> DeviceIdentity {
        let chain: Vec<CertificateDer<'static>> =
            self.cert_chain_der.iter().map(|d| CertificateDer::from(d.clone())).collect();
        DeviceIdentity::from_der(self.device_name.clone(), chain, self.key_pkcs8_der.clone())
    }

    pub fn ca_cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.ca_cert_der.clone())
    }

    /// **Audit H17 — a bootstrap blob is a PRIVATE KEY FILE, and was being written like a log.**
    ///
    /// This encodes `key_pkcs8_der` — the device's private key — plus the cluster CA certificate.
    /// It was written with `File::create`, which **follows symlinks** and applies the default
    /// umask, to **predictable names in a shared `/tmp`** (`hydra-wan-sp.boot`). Any local user
    /// could read it; and via C2's role binding, a stolen worker key is a worker. Cleanup was
    /// `Drop`-only, so a `kill -9` of the runner left the key on disk.
    ///
    /// Now: `create_new` (**refuses to clobber**, and with `O_EXCL` refuses to follow a planted
    /// symlink), mode **0600**, inside a directory created **0700**. `create_new` also means a
    /// stale file from a previous run is an error rather than a silent overwrite — if a key is
    /// already there, someone should look at it before it is replaced.
    pub fn write_to(&self, path: &str) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(&self.encode())?;
        f.sync_all()?;
        Ok(())
    }

    /// [`Self::write_to`] for a path that may already hold a blob from a previous run: the old file
    /// is **removed first** so the 0600 `create_new` still applies, rather than reusing whatever
    /// permissions the existing file happens to carry (audit H17).
    pub fn write_to_replacing(&self, path: &str) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.write_to(path)
    }

    pub fn read_from(path: &str) -> io::Result<Bootstrap> {
        let mut buf = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        Self::decode(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.str(&self.listen_addr);
        w.str(&self.device_name);
        w.bytes(&self.ca_cert_der);
        w.u32(self.cert_chain_der.len() as u32);
        for c in &self.cert_chain_der {
            w.bytes(c);
        }
        w.bytes(&self.key_pkcs8_der);
        // config
        w.bytes(&self.cfg.fence.cluster_id);
        w.bytes(&self.cfg.fence.manifest_hash);
        w.bytes(&self.cfg.fence.model_instance_id);
        w.bytes(&self.cfg.fence.session_id);
        w.u32(self.cfg.rank as u32);
        w.i32(self.cfg.layer_first);
        w.i32(self.cfg.layer_last);
        w.u32(self.cfg.is_final as u32);
        w.u32(self.cfg.receives_tokens as u32);
        w.u32(self.cfg.epoch);
        w.u32(self.cfg.recovery_id);
        w.str(self.cfg.model_path.as_deref().unwrap_or(""));
        w.i32(self.cfg.n_gpu_layers);
        w.i32(self.cfg.n_ctx);
        w.u32(self.cfg.recovery_start as u32);
        match &self.cfg.sampler_config {
            Some(s) => {
                w.u32(1);
                w.f32(s.temperature);
                w.f32(s.top_p);
                w.f32(s.repeat_penalty);
                w.u32(s.penalty_last_n as u32);
                w.u64(s.seed);
            }
            None => w.u32(0),
        }
        // Forwarding block (append-only): presence flag then the fields.
        match &self.forwarding {
            Some(f) => {
                w.u32(1);
                w.str(&f.down_addr);
                w.str(&f.down_name);
                w.str(&f.dur_addr);
                w.str(&f.dur_name);
                w.u32(f.require_durable as u32);
                w.u32(f.capacity);
            }
            None => w.u32(0),
        }
        // P2·10b shard-manifest path (append-only, after the forwarding block): empty ⇒ this
        // worker loads a full model, exactly as before. An older bootstrap simply ends here.
        // **Audit C1:** the manifest path travels WITH its trusted signing key. A bootstrap that
        // carried only the path would let a provisioner hand a worker a manifest without saying
        // whose signature counts — which is how the self-attesting `verify()` stayed unnoticed.
        match &self.cfg.shard_manifest {
            Some(sm) => {
                w.str(&sm.path);
                w.0.extend_from_slice(&sm.trusted_signer);
            }
            None => w.str(""),
        }
        // Audit C2 (append-only, after the shard block): the name->role table. An older bootstrap
        // simply ends before this and is refused at startup by `role_table()`.
        w.u32(self.expected_peers.len() as u32);
        for (name, role) in &self.expected_peers {
            w.str(name);
            w.u32(*role as u32);
        }
        w.0
    }

    /// Fuzz entry point for the rule-17 class sweep. The blob is untrusted input — it arrives on
    /// the worker's filesystem from whatever provisioned it, and since audit C1 it carries the
    /// **manifest trust anchor**, so a parser defect here sits *upstream* of the trust decision.
    pub fn decode_for_fuzz(buf: &[u8]) -> Result<Bootstrap, String> {
        Self::decode(buf)
    }

    fn decode(buf: &[u8]) -> Result<Bootstrap, String> {
        let mut r = Reader { b: buf, i: 0 };
        let listen_addr = r.str()?;
        let device_name = r.str()?;
        let ca_cert_der = r.bytes()?;
        let n = r.u32()?;
        // **Audit L2 — the FOURTH instance of the §7.28-D2 class** (GGUF, manifest, and now here).
        // A declared count may never reserve more memory than the remaining input could justify;
        // a DER cert entry costs at least its own 4-byte length prefix.
        let mut cert_chain_der = Vec::with_capacity(r.reserve_for(n as u64, 4));
        for _ in 0..n {
            cert_chain_der.push(r.bytes()?);
        }
        let key_pkcs8_der = r.bytes()?;
        let fence = SessionFence {
            cluster_id: r.arr::<CLUSTER_ID_LEN>()?,
            manifest_hash: r.arr::<HASH_LEN>()?,
            model_instance_id: r.arr::<MODEL_INSTANCE_ID_LEN>()?,
            session_id: r.arr::<SESSION_ID_LEN>()?,
        };
        let rank = r.u32()? as u16;
        let layer_first = r.i32()?;
        let layer_last = r.i32()?;
        let is_final = r.u32()? != 0;
        let receives_tokens = r.u32()? != 0;
        let epoch = r.u32()?;
        let recovery_id = r.u32()?;
        let model_path = r.str()?;
        let n_gpu_layers = r.i32()?;
        let n_ctx = r.i32()?;
        let recovery_start = r.u32()? != 0;
        let sampler_config = if r.u32()? != 0 {
            Some(SamplingConfig {
                temperature: r.f32()?,
                top_p: r.f32()?,
                repeat_penalty: r.f32()?,
                penalty_last_n: r.u32()? as usize,
                seed: r.u64()?,
            })
        } else {
            None
        };
        // Forwarding block (append-only): older bootstraps end here → decode to `None`.
        let forwarding = if r.remaining() && r.u32()? != 0 {
            Some(ForwardingBootstrap {
                down_addr: r.str()?,
                down_name: r.str()?,
                dur_addr: r.str()?,
                dur_name: r.str()?,
                require_durable: r.u32()? != 0,
                capacity: r.u32()?,
            })
        } else {
            None
        };
        // P2·10b shard-manifest path (append-only): older bootstraps end here → `None`.
        let shard_manifest = if r.remaining() {
            let path = r.str()?;
            if path.is_empty() {
                None
            } else {
                // A shard path with no trusted key is REFUSED, not defaulted. There is no key this
                // code could substitute that would be safe (C1).
                let key: [u8; 32] = r.raw32().map_err(|_| {
                    "bootstrap names a shard manifest but carries no trusted signing key (audit C1): \
                     a shard path without a trust anchor is refused, never defaulted"
                        .to_string()
                })?;
                Some(crate::worker::ShardManifestConfig { path, trusted_signer: key })
            }
        } else {
            None
        };
        // Audit C2: the expected-peer table (append-only). Absent ⇒ empty ⇒ `role_table()` refuses.
        let expected_peers = if r.remaining() {
            let n = r.u32()?;
            let mut v = Vec::with_capacity(r.reserve_for(n as u64, 5));
            for _ in 0..n {
                let name = r.str()?;
                let role = r.u32()?;
                v.push((name, u8::try_from(role).map_err(|_| "role tag out of range".to_string())?));
            }
            v
        } else {
            Vec::new()
        };
        Ok(Bootstrap {
            listen_addr,
            device_name,
            ca_cert_der,
            cert_chain_der,
            key_pkcs8_der,
            forwarding,
            expected_peers,
            cfg: WorkerConfig {
                fence,
                rank,
                layer_first,
                layer_last,
                is_final,
                receives_tokens,
                epoch,
                recovery_id,
                model_path: (!model_path.is_empty()).then_some(model_path),
                n_gpu_layers,
                n_ctx,
                sampler_config,
                recovery_start,
                shard_manifest,
            },
        })
    }
}

#[derive(Default)]
struct Writer(Vec<u8>);
impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}
impl Reader<'_> {
    /// Any bytes left to read? (Used for append-only optional trailing blocks.)
    fn remaining(&self) -> bool {
        self.i < self.b.len()
    }
    fn remaining_bytes(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }
    /// Clamp an attacker-declared count to what the remaining input could justify (audit L2 /
    /// standing rule 17 — the same invariant as `gguf::Cursor::reserve_for` and
    /// `manifest::Reader::reserve_for`, deliberately spelled the same way in all three so the
    /// class is recognisable at a glance).
    fn reserve_for(&self, declared: u64, min_bytes_each: usize) -> usize {
        (declared as usize).min(self.remaining_bytes() / min_bytes_each.max(1))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let end = self.i + 4;
        let s = self.b.get(self.i..end).ok_or("truncated u32")?;
        self.i = end;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64, String> {
        let end = self.i + 8;
        let s = self.b.get(self.i..end).ok_or("truncated u64")?;
        self.i = end;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.u32()? as usize;
        // Bounds-check BEFORE `to_vec()` allocates, and use a checked add so the offset arithmetic
        // cannot wrap a hostile length back into range (audit L2 / rule 17).
        let end = self.i.checked_add(n).ok_or("bytes length overflows")?;
        let s = self.b.get(self.i..end).ok_or("truncated bytes")?;
        self.i = end;
        Ok(s.to_vec())
    }
    /// A fixed 32 raw bytes with no length prefix (the trusted signing key trailer).
    fn raw32(&mut self) -> Result<[u8; 32], String> {
        let end = self.i + 32;
        let s = self.b.get(self.i..end).ok_or("truncated 32-byte key")?;
        self.i = end;
        Ok(s.try_into().unwrap())
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.bytes()?.try_into().map_err(|_| format!("expected {N}-byte array"))
    }
    fn str(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?).map_err(|_| "invalid utf8".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, valid bootstrap for the role-table tests.
    fn sample_bootstrap() -> Bootstrap {
        Bootstrap {
            listen_addr: "127.0.0.1:0".into(),
            device_name: "s1".into(),
            ca_cert_der: vec![1],
            cert_chain_der: vec![vec![2]],
            key_pkcs8_der: vec![3],
            expected_peers: vec![
                ("coordinator".into(), ROLE_COORDINATOR),
                ("s2".into(), ROLE_STAGE_BASE + 1),
            ],
            forwarding: None,
            cfg: WorkerConfig {
                fence: SessionFence::dev(9),
                rank: 0,
                layer_first: 0,
                layer_last: 12,
                is_final: false,
                receives_tokens: true,
                epoch: 0,
                recovery_id: 0,
                model_path: None,
                n_gpu_layers: 0,
                n_ctx: 64,
                sampler_config: None,
                recovery_start: false,
                shard_manifest: None,
            },
        }
    }

    #[test]
    fn bootstrap_round_trips() {
        let boot = Bootstrap {
            listen_addr: "127.0.0.1:0".into(),
            device_name: "worker-1".into(),
            ca_cert_der: vec![1, 2, 3],
            cert_chain_der: vec![vec![4, 5], vec![6]],
            expected_peers: vec![("coordinator".into(), ROLE_COORDINATOR), ("s1".into(), ROLE_STAGE_BASE)],
            key_pkcs8_der: vec![9, 9, 9],
            cfg: WorkerConfig {
                fence: SessionFence::dev(7),
                rank: 0,
                layer_first: 0,
                layer_last: 12,
                is_final: false,
                receives_tokens: true,
                epoch: 3,
                recovery_id: 0,
                model_path: Some("/models/x.gguf".into()),
                n_gpu_layers: 0,
                n_ctx: 64,
                sampler_config: Some(SamplingConfig { temperature: 0.7, top_p: 0.9, repeat_penalty: 1.1, penalty_last_n: 16, seed: 99 }),
                recovery_start: false, shard_manifest: None,
            },
            forwarding: None,
        };
        let bytes = boot.encode();
        let back = Bootstrap::decode(&bytes).unwrap();
        assert_eq!(back.device_name, "worker-1");
        assert_eq!(back.cert_chain_der, vec![vec![4, 5], vec![6]]);
        assert_eq!(back.cfg.fence, SessionFence::dev(7));
        assert_eq!(back.cfg.layer_last, 12);
        assert_eq!(back.cfg.model_path.as_deref(), Some("/models/x.gguf"));
        assert!(back.cfg.receives_tokens && !back.cfg.is_final);
        assert_eq!(back.cfg.sampler_config.as_ref().map(|s| s.seed), Some(99));
        assert_eq!(back.cfg.sampler_config.as_ref().map(|s| s.penalty_last_n), Some(16));
        assert!(back.forwarding.is_none());
    }

    /// **Audit C2.** The role table is provisioning, and a bootstrap that names no peers is
    /// **refused at startup** rather than starting a worker that will deny every connection.
    /// Fail closed *and* fail loudly: an empty table is safe but undiagnosable.
    #[test]
    fn a_bootstrap_naming_no_peers_is_refused_rather_than_starting_a_deny_all_worker() {
        let mut boot = sample_bootstrap();
        boot.expected_peers.clear();
        let err = boot.role_table().expect_err("an empty peer table must be refused");
        assert!(err.contains("names no expected peers"), "the error should say why: {err}");

        // Control: the provisioned table builds, and the roles round-trip by value.
        let boot = sample_bootstrap();
        let t = boot.role_table().expect("a provisioned table builds");
        assert_eq!(t.len(), 2);
    }

    /// The role tags are a wire enum, so their VALUES are the contract — a bootstrap written by one
    /// build is read by another. Pinned here rather than trusted to stay put.
    #[test]
    fn role_wire_tags_are_stable_by_value() {
        assert_eq!((ROLE_COORDINATOR, ROLE_DURABILITY_TARGET, ROLE_STAGE_BASE), (0, 1, 2));
        let boot = Bootstrap {
            expected_peers: vec![("sp".into(), ROLE_STAGE_BASE + 5)],
            ..sample_bootstrap()
        };
        let t = boot.role_table().unwrap();
        // A stage's tag carries its rank, so rank 5 must survive the round trip.
        assert!(format!("{t:?}").contains("rank: 5"), "stage rank must survive the tag encoding: {t:?}");
    }

    #[test]
    fn bootstrap_round_trips_with_forwarding() {
        let boot = Bootstrap {
            listen_addr: "127.0.0.1:0".into(),
            device_name: "s2".into(),
            ca_cert_der: vec![1],
            cert_chain_der: vec![vec![2]],
            key_pkcs8_der: vec![3],
            expected_peers: vec![("coordinator".into(), ROLE_COORDINATOR), ("s1".into(), ROLE_STAGE_BASE)],
            cfg: WorkerConfig {
                fence: SessionFence::dev(8),
                rank: 1,
                layer_first: 14,
                layer_last: 21,
                is_final: false,
                receives_tokens: false,
                epoch: 0,
                recovery_id: 0,
                model_path: Some("/m.gguf".into()),
                n_gpu_layers: 0,
                n_ctx: 64,
                sampler_config: None,
                recovery_start: false, shard_manifest: None,
            },
            forwarding: Some(ForwardingBootstrap {
                down_addr: "10.0.0.5:41999".into(),
                down_name: "sp".into(),
                dur_addr: "100.64.0.1:42000".into(),
                dur_name: "coordinator".into(),
                require_durable: true,
                capacity: 64,
            }),
        };
        let back = Bootstrap::decode(&boot.encode()).unwrap();
        let f = back.forwarding.expect("forwarding block round-trips");
        assert_eq!(f.down_addr, "10.0.0.5:41999");
        assert_eq!(f.down_name, "sp");
        assert_eq!(f.dur_name, "coordinator");
        assert!(f.require_durable);
        assert_eq!(f.capacity, 64);
    }
}
