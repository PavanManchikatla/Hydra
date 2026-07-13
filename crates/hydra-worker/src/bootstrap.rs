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
use crate::wire::{SessionKeys, CLUSTER_ID_LEN, HASH_LEN, MODEL_INSTANCE_ID_LEN, SESSION_ID_LEN};
use crate::worker::WorkerConfig;

/// Everything a worker process needs to come up and serve.
pub struct Bootstrap {
    pub listen_addr: String,
    /// The DNS/CN identity this worker presents (what the coordinator uses as `server_name`).
    pub device_name: String,
    pub ca_cert_der: Vec<u8>,
    pub cert_chain_der: Vec<Vec<u8>>,
    pub key_pkcs8_der: Vec<u8>,
    pub cfg: WorkerConfig,
}

impl Bootstrap {
    pub fn identity(&self) -> DeviceIdentity {
        let chain: Vec<CertificateDer<'static>> =
            self.cert_chain_der.iter().map(|d| CertificateDer::from(d.clone())).collect();
        DeviceIdentity::from_der(self.device_name.clone(), chain, self.key_pkcs8_der.clone())
    }

    pub fn ca_cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.ca_cert_der.clone())
    }

    pub fn write_to(&self, path: &str) -> io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        f.write_all(&self.encode())?;
        Ok(())
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
        w.bytes(&self.cfg.keys.cluster_id);
        w.bytes(&self.cfg.keys.manifest_hash);
        w.bytes(&self.cfg.keys.model_instance_id);
        w.bytes(&self.cfg.keys.session_id);
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
        w.0
    }

    fn decode(buf: &[u8]) -> Result<Bootstrap, String> {
        let mut r = Reader { b: buf, i: 0 };
        let listen_addr = r.str()?;
        let device_name = r.str()?;
        let ca_cert_der = r.bytes()?;
        let n = r.u32()? as usize;
        let mut cert_chain_der = Vec::with_capacity(n);
        for _ in 0..n {
            cert_chain_der.push(r.bytes()?);
        }
        let key_pkcs8_der = r.bytes()?;
        let keys = SessionKeys {
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
        Ok(Bootstrap {
            listen_addr,
            device_name,
            ca_cert_der,
            cert_chain_der,
            key_pkcs8_der,
            cfg: WorkerConfig {
                keys,
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
        let end = self.i + n;
        let s = self.b.get(self.i..end).ok_or("truncated bytes")?;
        self.i = end;
        Ok(s.to_vec())
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

    #[test]
    fn bootstrap_round_trips() {
        let boot = Bootstrap {
            listen_addr: "127.0.0.1:0".into(),
            device_name: "worker-1".into(),
            ca_cert_der: vec![1, 2, 3],
            cert_chain_der: vec![vec![4, 5], vec![6]],
            key_pkcs8_der: vec![9, 9, 9],
            cfg: WorkerConfig {
                keys: SessionKeys::dev(7),
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
                recovery_start: false,
            },
        };
        let bytes = boot.encode();
        let back = Bootstrap::decode(&bytes).unwrap();
        assert_eq!(back.device_name, "worker-1");
        assert_eq!(back.cert_chain_der, vec![vec![4, 5], vec![6]]);
        assert_eq!(back.cfg.keys, SessionKeys::dev(7));
        assert_eq!(back.cfg.layer_last, 12);
        assert_eq!(back.cfg.model_path.as_deref(), Some("/models/x.gguf"));
        assert!(back.cfg.receives_tokens && !back.cfg.is_final);
        assert_eq!(back.cfg.sampler_config.as_ref().map(|s| s.seed), Some(99));
        assert_eq!(back.cfg.sampler_config.as_ref().map(|s| s.penalty_last_n), Some(16));
    }
}
