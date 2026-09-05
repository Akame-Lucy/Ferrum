use snow::{Builder, Keypair, TransportState};
use std::error::Error;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Noise caps a single encrypted message at 65535 bytes, 16 of which are
/// the AEAD tag. Anything larger has to be split.
const NOISE_MAX_MESSAGE: usize = 65535;
const NOISE_TAG_LEN: usize = 16;
const CHUNK_PLAINTEXT: usize = NOISE_MAX_MESSAGE - NOISE_TAG_LEN;

/// Handshake messages for this pattern are well under 200 bytes. A peer
/// announcing anything near the u16 maximum is not speaking the protocol.
const HANDSHAKE_BUF: usize = 1024;

/// Upper bound on one logical message's plaintext. File contents move in
/// chunks well below this (see `protocol::TRANSFER_CHUNK`), so the only
/// messages that get anywhere near it are directory listings and search
/// results of very large trees. Small enough that a peer cannot make us
/// allocate arbitrary memory by lying in a length prefix.
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

type BoxError = Box<dyn Error + Send + Sync>;

/// An established, encrypted Noise_XX transport session. Both peers know each
/// other's static public key by the time this exists, unlike the anonymous
/// Noise_NN pattern this replaces.
///
/// Cheaply `Clone`able (the transport is `Arc<Mutex<_>>`-backed) so a single
/// session can drive two independent tasks each holding one half of a split
/// stream: a writer task calling only `send_message`, a reader task calling
/// only `read_message`. Each direction has its own cipher state and nonce
/// counter, so the two never interfere. What is *not* supported is two
/// concurrent writers (or readers) on one session: chunks of two messages
/// would interleave on the wire.
///
/// # Wire format
///
/// Handshake frames are a big-endian u16 length followed by the Noise
/// handshake message.
///
/// After the handshake, one logical message is a big-endian u32 plaintext
/// length, followed by as many `u16 length + Noise ciphertext` frames as it
/// takes to deliver that many plaintext bytes (each frame at most 65535
/// bytes). Splitting is what lets a payload exceed Noise's 64 KiB message
/// limit, which a file read or write easily does. Lengths are in the clear;
/// the plaintext never is.
#[derive(Clone)]
pub struct NoiseSession {
    transport: Arc<Mutex<TransportState>>,
}

async fn read_handshake_frame<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut [u8; HANDSHAKE_BUF],
) -> Result<usize, BoxError> {
    let len = stream.read_u16().await? as usize;
    if len > HANDSHAKE_BUF {
        return Err(format!("handshake frame of {} bytes exceeds the {} byte limit", len, HANDSHAKE_BUF).into());
    }
    stream.read_exact(&mut buf[..len]).await?;
    Ok(len)
}

async fn write_handshake_frame<W: AsyncWrite + Unpin>(stream: &mut W, frame: &[u8]) -> Result<(), BoxError> {
    stream.write_u16(frame.len() as u16).await?;
    stream.write_all(frame).await?;
    Ok(())
}

impl NoiseSession {
    pub async fn handshake_responder<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        local_keypair: &Keypair,
    ) -> Result<(Self, Vec<u8>), BoxError> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        // snow 0.10 validates the key length here rather than at build time,
        // so this is fallible where it used to be infallible.
        let mut state = builder
            .local_private_key(&local_keypair.private)?
            .build_responder()?;

        let mut buf = [0u8; HANDSHAKE_BUF];
        let mut scratch = [0u8; HANDSHAKE_BUF];

        // -> e
        let len = read_handshake_frame(stream, &mut buf).await?;
        state.read_message(&buf[..len], &mut scratch)?;

        // <- e, ee, s, es
        let len = state.write_message(&[], &mut scratch)?;
        write_handshake_frame(stream, &scratch[..len]).await?;

        // -> s, se
        let len = read_handshake_frame(stream, &mut buf).await?;
        state.read_message(&buf[..len], &mut scratch)?;

        let remote_static = state
            .get_remote_static()
            .ok_or("handshake completed without a remote static key")?
            .to_vec();

        let transport = state.into_transport_mode()?;
        Ok((Self { transport: Arc::new(Mutex::new(transport)) }, remote_static))
    }

    pub async fn handshake_initiator<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        local_keypair: &Keypair,
    ) -> Result<(Self, Vec<u8>), BoxError> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        // snow 0.10 validates the key length here rather than at build time,
        // so this is fallible where it used to be infallible.
        let mut state = builder
            .local_private_key(&local_keypair.private)?
            .build_initiator()?;

        let mut buf = [0u8; HANDSHAKE_BUF];
        let mut scratch = [0u8; HANDSHAKE_BUF];

        // -> e
        let len = state.write_message(&[], &mut scratch)?;
        write_handshake_frame(stream, &scratch[..len]).await?;

        // <- e, ee, s, es
        let len = read_handshake_frame(stream, &mut buf).await?;
        state.read_message(&buf[..len], &mut scratch)?;

        let remote_static = state
            .get_remote_static()
            .ok_or("handshake completed without a remote static key")?
            .to_vec();

        // -> s, se
        let len = state.write_message(&[], &mut scratch)?;
        write_handshake_frame(stream, &scratch[..len]).await?;

        let transport = state.into_transport_mode()?;
        Ok((Self { transport: Arc::new(Mutex::new(transport)) }, remote_static))
    }

    /// Encrypts and sends one logical message, splitting it into as many
    /// Noise messages as needed. Encryption of every chunk happens under one
    /// lock so the chunk sequence is contiguous on the wire; the lock is
    /// released before any network I/O.
    pub async fn send_message<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        msg: &[u8],
    ) -> Result<(), BoxError> {
        if msg.len() > MAX_MESSAGE_LEN {
            return Err(format!(
                "message of {} bytes exceeds the {} byte limit",
                msg.len(),
                MAX_MESSAGE_LEN
            )
            .into());
        }

        let chunk_count = msg.len().div_ceil(CHUNK_PLAINTEXT);
        let mut wire = Vec::with_capacity(4 + msg.len() + chunk_count * (2 + NOISE_TAG_LEN));
        wire.extend_from_slice(&(msg.len() as u32).to_be_bytes());

        {
            let mut transport = self.transport.lock().await;
            let mut cipher_buf = [0u8; NOISE_MAX_MESSAGE];
            for chunk in msg.chunks(CHUNK_PLAINTEXT) {
                let len = transport.write_message(chunk, &mut cipher_buf)?;
                wire.extend_from_slice(&(len as u16).to_be_bytes());
                wire.extend_from_slice(&cipher_buf[..len]);
            }
        }

        writer.write_all(&wire).await?;
        Ok(())
    }

    /// Receives and decrypts one logical message sent by `send_message`.
    pub async fn read_message<R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
    ) -> Result<Vec<u8>, BoxError> {
        let total = reader.read_u32().await? as usize;
        if total > MAX_MESSAGE_LEN {
            return Err(format!(
                "peer announced a {} byte message, above the {} byte limit",
                total, MAX_MESSAGE_LEN
            )
            .into());
        }

        let mut plain = Vec::with_capacity(total);
        let mut cipher_buf = vec![0u8; NOISE_MAX_MESSAGE];
        let mut plain_buf = vec![0u8; NOISE_MAX_MESSAGE];

        while plain.len() < total {
            let len = reader.read_u16().await? as usize;
            if len < NOISE_TAG_LEN {
                return Err(format!("ciphertext chunk of {} bytes is shorter than the AEAD tag", len).into());
            }
            reader.read_exact(&mut cipher_buf[..len]).await?;

            let plain_len = {
                let mut transport = self.transport.lock().await;
                transport.read_message(&cipher_buf[..len], &mut plain_buf)?
            };

            if plain.len() + plain_len > total {
                return Err("peer sent more plaintext than it announced".into());
            }
            plain.extend_from_slice(&plain_buf[..plain_len]);
        }

        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn keypair() -> Keypair {
        Builder::new(NOISE_PATTERN.parse().unwrap()).generate_keypair().unwrap()
    }

    /// Two sessions connected through an in-memory pipe, plus the streams
    /// each side keeps talking on.
    async fn connected() -> (NoiseSession, tokio::io::DuplexStream, NoiseSession, tokio::io::DuplexStream) {
        let (mut a, mut b) = duplex(1 << 16);
        let ka = keypair();
        let kb = keypair();
        let pub_a = ka.public.clone();
        let pub_b = kb.public.clone();

        let responder = tokio::spawn(async move {
            let (s, remote) = NoiseSession::handshake_responder(&mut b, &kb).await.unwrap();
            (s, remote, b)
        });
        let (sa, remote_from_a, a) = {
            let (s, remote) = NoiseSession::handshake_initiator(&mut a, &ka).await.unwrap();
            (s, remote, a)
        };
        let (sb, remote_from_b, b) = responder.await.unwrap();

        assert_eq!(remote_from_a, pub_b, "initiator must learn the responder's static key");
        assert_eq!(remote_from_b, pub_a, "responder must learn the initiator's static key");
        (sa, a, sb, b)
    }

    #[tokio::test]
    async fn small_message_round_trips() {
        let (sa, mut a, sb, mut b) = connected().await;
        sa.send_message(&mut a, b"hello").await.unwrap();
        assert_eq!(sb.read_message(&mut b).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn empty_message_round_trips_and_keeps_nonces_in_sync() {
        let (sa, mut a, sb, mut b) = connected().await;
        sa.send_message(&mut a, b"").await.unwrap();
        sa.send_message(&mut a, b"after").await.unwrap();
        assert_eq!(sb.read_message(&mut b).await.unwrap(), b"");
        assert_eq!(sb.read_message(&mut b).await.unwrap(), b"after");
    }

    /// A payload several times Noise's 64 KiB limit must arrive intact.
    #[tokio::test]
    async fn message_larger_than_one_noise_frame_round_trips() {
        let (sa, mut a, sb, mut b) = connected().await;
        let payload: Vec<u8> = (0..(3 * NOISE_MAX_MESSAGE + 12345)).map(|i| (i % 251) as u8).collect();

        let sender = {
            let payload = payload.clone();
            tokio::spawn(async move {
                sa.send_message(&mut a, &payload).await.unwrap();
            })
        };
        let got = sb.read_message(&mut b).await.unwrap();
        sender.await.unwrap();
        assert_eq!(got.len(), payload.len());
        assert!(got == payload);
    }

    #[tokio::test]
    async fn both_directions_work_on_split_halves() {
        let (sa, a, sb, b) = connected().await;
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);

        let (sa2, sb2) = (sa.clone(), sb.clone());
        let a_to_b = tokio::spawn(async move { sa2.send_message(&mut aw, b"ping").await.unwrap() });
        let b_to_a = tokio::spawn(async move { sb2.send_message(&mut bw, b"pong").await.unwrap() });

        assert_eq!(sb.read_message(&mut br).await.unwrap(), b"ping");
        assert_eq!(sa.read_message(&mut ar).await.unwrap(), b"pong");
        a_to_b.await.unwrap();
        b_to_a.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_handshake_frame_is_rejected_not_panicked() {
        let (mut a, mut b) = duplex(1 << 16);
        let kb = keypair();
        let responder = tokio::spawn(async move { NoiseSession::handshake_responder(&mut b, &kb).await.map(|_| ()) });
        // Announce a frame far larger than any real handshake message.
        a.write_u16(60000).await.unwrap();
        a.write_all(&[0u8; 64]).await.unwrap();
        let err = responder.await.unwrap().expect_err("must reject");
        assert!(err.to_string().contains("exceeds"), "got: {err}");
    }

    #[tokio::test]
    async fn oversized_message_announcement_is_rejected() {
        let (_sa, _a, sb, _b) = connected().await;
        let (mut raw, mut peer) = duplex(64);
        raw.write_u32((MAX_MESSAGE_LEN + 1) as u32).await.unwrap();
        let err = sb.read_message(&mut peer).await.expect_err("must reject");
        assert!(err.to_string().contains("limit"), "got: {err}");
    }

    #[tokio::test]
    async fn tampered_ciphertext_fails_to_decrypt() {
        let (sa, mut a, sb, mut b) = connected().await;
        sa.send_message(&mut a, b"integrity").await.unwrap();

        // Pull the raw frame off the pipe, flip a byte, and feed it back.
        let mut header = [0u8; 4];
        b.read_exact(&mut header).await.unwrap();
        let len = b.read_u16().await.unwrap() as usize;
        let mut ct = vec![0u8; len];
        b.read_exact(&mut ct).await.unwrap();
        ct[0] ^= 0x01;

        let (mut raw, mut peer) = duplex(1 << 12);
        raw.write_all(&header).await.unwrap();
        raw.write_u16(len as u16).await.unwrap();
        raw.write_all(&ct).await.unwrap();
        assert!(sb.read_message(&mut peer).await.is_err());
    }
}
