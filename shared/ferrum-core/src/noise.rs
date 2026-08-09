use snow::{Builder, Keypair, TransportState};
use std::error::Error;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// An established, encrypted Noise_XX transport session. Both peers know each
/// other's static public key by the time this exists, unlike the anonymous
/// Noise_NN pattern this replaces.
///
/// Cheaply `Clone`able (the transport is `Arc<Mutex<_>>`-backed) so a single
/// session can drive two independent tasks each holding one half of a split
/// stream: a writer task locking only for `write_message`, a reader task
/// locking only for `read_message`. The lock is never held across network
/// I/O, only around the synchronous encrypt/decrypt call, so the two never
/// contend at anything above interactive data rates.
#[derive(Clone)]
pub struct NoiseSession {
    transport: Arc<Mutex<TransportState>>,
}

impl NoiseSession {
    pub async fn handshake_responder(
        stream: &mut TcpStream,
        local_keypair: &Keypair,
    ) -> Result<(Self, Vec<u8>), Box<dyn Error + Send + Sync>> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        let mut state = builder
            .local_private_key(&local_keypair.private)
            .build_responder()?;

        let mut buf = [0u8; 1024];

        // -> e
        let len = stream.read_u16().await? as usize;
        stream.read_exact(&mut buf[..len]).await?;
        state.read_message(&buf[..len], &mut [0u8; 1024])?;

        // <- e, ee, s, es
        let mut out_buf = [0u8; 1024];
        let len = state.write_message(&[], &mut out_buf)?;
        stream.write_u16(len as u16).await?;
        stream.write_all(&out_buf[..len]).await?;

        // -> s, se
        let len = stream.read_u16().await? as usize;
        stream.read_exact(&mut buf[..len]).await?;
        state.read_message(&buf[..len], &mut [0u8; 1024])?;

        let remote_static = state
            .get_remote_static()
            .ok_or("handshake completed without a remote static key")?
            .to_vec();

        let transport = state.into_transport_mode()?;
        Ok((Self { transport: Arc::new(Mutex::new(transport)) }, remote_static))
    }

    pub async fn handshake_initiator(
        stream: &mut TcpStream,
        local_keypair: &Keypair,
    ) -> Result<(Self, Vec<u8>), Box<dyn Error + Send + Sync>> {
        let builder = Builder::new(NOISE_PATTERN.parse()?);
        let mut state = builder
            .local_private_key(&local_keypair.private)
            .build_initiator()?;

        // -> e
        let mut out_buf = [0u8; 1024];
        let len = state.write_message(&[], &mut out_buf)?;
        stream.write_u16(len as u16).await?;
        stream.write_all(&out_buf[..len]).await?;

        // <- e, ee, s, es
        let mut buf = [0u8; 1024];
        let len = stream.read_u16().await? as usize;
        stream.read_exact(&mut buf[..len]).await?;
        state.read_message(&buf[..len], &mut [0u8; 1024])?;

        let remote_static = state
            .get_remote_static()
            .ok_or("handshake completed without a remote static key")?
            .to_vec();

        // -> s, se
        let len = state.write_message(&[], &mut out_buf)?;
        stream.write_u16(len as u16).await?;
        stream.write_all(&out_buf[..len]).await?;

        let transport = state.into_transport_mode()?;
        Ok((Self { transport: Arc::new(Mutex::new(transport)) }, remote_static))
    }

    pub async fn send_message<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        msg: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut cipher_buf = vec![0u8; msg.len() + 32];
        let len = {
            let mut transport = self.transport.lock().await;
            transport.write_message(msg, &mut cipher_buf)?
        };
        writer.write_u32(len as u32).await?;
        writer.write_all(&cipher_buf[..len]).await?;
        Ok(())
    }

    pub async fn read_message<R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
    ) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        let len = reader.read_u32().await? as usize;
        let mut cipher_buf = vec![0u8; len];
        reader.read_exact(&mut cipher_buf).await?;

        let mut plain_buf = vec![0u8; len];
        let plain_len = {
            let mut transport = self.transport.lock().await;
            transport.read_message(&cipher_buf, &mut plain_buf)?
        };
        plain_buf.truncate(plain_len);
        Ok(plain_buf)
    }
}
