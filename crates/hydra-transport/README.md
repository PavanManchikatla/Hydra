# hydra-transport

Control/data-plane transport (BLUEPRINT §1.3, spec §4). A `Transport` trait fronts concrete
impls — `tcp_mtls` (TCP + mutual TLS; the default, built first) and, later, QUIC. All impls
speak the `hydra-proto` wire framing (`HYFR` header + BLAKE3); every frame's header is validated
against the hard caps **before** the payload is read or the flatbuffer is parsed.

- `framed` — async frame I/O over any stream; pre-allocation gate rejects bad magic / wrong
  major / oversized `payload_len`; BLAKE3 tag verified before the payload is returned.
- `tls`    — cluster CA (`ClusterCa`) minted at pairing + per-device identities; rustls
  server/client configs requiring CA-signed certs (mutual TLS). Crypto backend: `ring`.
- `tcp_mtls` — `TcpMtlsListener` / `TcpMtls` over tokio.
