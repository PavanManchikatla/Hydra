//! Length-prefixed frame I/O over any async byte stream (TLS or plain TCP).
//!
//! Read path (DoD-critical): the 12-byte header is read and validated — magic, wire major, and
//! `payload_len ≤ MAX_FRAME_BYTES` — **before** the payload buffer is allocated or the flatbuffer
//! is parsed. A bad tag is caught by BLAKE3 before the body is handed upward.

use hydra_proto::framing::{encode_frame, verify_frame, FrameHeader, HEADER_LEN, TAG_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Frame, TransportError};

/// How long a peer may take to deliver a payload it has already declared (audit M1).
///
/// Generous by design: a 64 MiB prefill chunk over a slow WAN link is a legitimate frame and must
/// not be cut off. What this refuses is the frame that never arrives at all.
pub const PAYLOAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A framed connection over stream `S`.
pub struct Conn<S> {
    stream: S,
    peer: Option<String>,
    /// Audit M1: how long this connection will wait for a declared payload. Defaults to
    /// [`PAYLOAD_READ_TIMEOUT`]; settable so a test can assert the bound without waiting for it.
    payload_read_timeout: std::time::Duration,
}

impl<S> Conn<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self { stream, peer: None, payload_read_timeout: PAYLOAD_READ_TIMEOUT }
    }

    pub fn with_peer(stream: S, peer: impl Into<String>) -> Self {
        Self { stream, peer: Some(peer.into()), payload_read_timeout: PAYLOAD_READ_TIMEOUT }
    }

    /// Authenticated peer identity (certificate subject), if the transport recorded one.
    pub fn peer_identity(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    /// Encode and write one frame (header + payload + BLAKE3 tag), then flush.
    pub async fn send(&mut self, flags: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(flags, payload)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read one frame. Validates the header (magic/version/cap) before allocating the payload,
    /// then verifies the BLAKE3 tag before returning.
    pub async fn recv(&mut self) -> Result<Frame, TransportError> {
        let mut hdr = [0u8; HEADER_LEN];
        self.stream.read_exact(&mut hdr).await?;
        // Pre-allocation gate: rejects bad magic / wrong major / oversized payload_len.
        let header = FrameHeader::parse(&hdr)?;
        // **Audit M1 — the hold is now bounded in TIME as well as in size.**
        //
        // The cap is real and is enforced before this allocation, which is why M1 is not the D2
        // class. What was unbounded is the *duration*: **12 bytes of header reserve up to 64 MiB**
        // and then `read_exact` waits for a payload that may never arrive — so sixteen idle
        // connections from one certificate holder commit a gigabyte, indefinitely. Under the
        // honest-worker assumption that is an accident (a wedged peer, a half-open NAT mapping)
        // rather than an attack, and the assumption excuses malice, never accidents.
        //
        // The timeout is what turns "held until the peer relents" into "held for at most this
        // long". The per-peer/global connection semaphore — the other half of the audit's fix —
        // belongs at the accept loop, not here, and is named in §8 rather than implied.
        let mut rest = vec![0u8; header.payload_len as usize + TAG_LEN];
        match tokio::time::timeout(self.payload_read_timeout, self.stream.read_exact(&mut rest)).await {
            Ok(r) => {
                r?;
            }
            Err(_) => return Err(TransportError::PayloadReadTimeout { declared: header.payload_len }),
        }
        // Verify the tag over header||payload before handing the payload up.
        let mut full = Vec::with_capacity(HEADER_LEN + rest.len());
        full.extend_from_slice(&hdr);
        full.extend_from_slice(&rest);
        let (h2, payload) = verify_frame(&full)?;
        debug_assert_eq!(h2, header);
        Ok(Frame { header, payload: payload.to_vec() })
    }

    /// Override the declared-payload read bound for this connection (audit M1).
    ///
    /// Production uses [`PAYLOAD_READ_TIMEOUT`]. A test that asserts the bound exists should not
    /// have to *wait* the production bound to do it — sixty seconds in a push lane is a cost paid
    /// on every run to observe something a millisecond can show.
    pub fn set_payload_read_timeout(&mut self, d: std::time::Duration) {
        self.payload_read_timeout = d;
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_proto::framing::{FRAME_MAGIC, HEADER_LEN};
    use hydra_proto::limits::MAX_FRAME_BYTES;
    use hydra_proto::framing::FrameError;

    // A plain in-memory duplex stands in for a TLS stream to test framing in isolation.
    #[tokio::test]
    async fn roundtrip_over_duplex() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut ca = Conn::new(a);
        let mut cb = Conn::new(b);
        let payload = b"authenticated control frame".to_vec();
        let send = tokio::spawn(async move { ca.send(0x00, &payload).await.unwrap() });
        let frame = cb.recv().await.unwrap();
        send.await.unwrap();
        assert_eq!(frame.payload, b"authenticated control frame");
    }

    #[tokio::test]
    async fn oversized_header_rejected_before_payload() {
        // Hand-write a header claiming a payload larger than the cap; recv must reject at the
        // header stage without trying to read/allocate the (nonexistent) payload.
        let (mut a, b) = tokio::io::duplex(1024);
        let mut cb = Conn::new(b);
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        hdr.extend_from_slice(&1u16.to_le_bytes()); // wire_version
        hdr.extend_from_slice(&0u16.to_le_bytes()); // flags
        hdr.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        assert_eq!(hdr.len(), HEADER_LEN);
        tokio::io::AsyncWriteExt::write_all(&mut a, &hdr).await.unwrap();
        let err = cb.recv().await.unwrap_err();
        assert!(matches!(err, TransportError::Frame(FrameError::LimitExceeded { .. })));
    }

    #[tokio::test]
    async fn bad_magic_rejected() {
        let (mut a, b) = tokio::io::duplex(1024);
        let mut cb = Conn::new(b);
        let mut hdr = [0u8; HEADER_LEN];
        hdr[0] = 0xDE; // wrong magic
        tokio::io::AsyncWriteExt::write_all(&mut a, &hdr).await.unwrap();
        assert!(matches!(cb.recv().await, Err(TransportError::Frame(FrameError::BadMagic(_)))));
    }

    #[tokio::test]
    async fn corrupt_tag_rejected() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut ca = Conn::new(a);
        let mut cb = Conn::new(b);
        // Build a valid frame, corrupt one payload byte on the wire.
        let good = hydra_proto::framing::encode_frame(0, b"tamper").unwrap();
        let mut bad = good.clone();
        bad[HEADER_LEN] ^= 0x01;
        let send = tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(ca.inner_mut(), &bad).await.unwrap();
        });
        assert!(matches!(
            cb.recv().await,
            Err(TransportError::Frame(FrameError::BadChecksum))
        ));
        send.await.unwrap();
    }

    impl<S> Conn<S> {
        // test-only accessor
        fn inner_mut(&mut self) -> &mut S {
            &mut self.stream
        }
    }
}
