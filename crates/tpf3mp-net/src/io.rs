use quinn::{ReadExactError, RecvStream, SendStream, WriteError};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tpf3mp_proto::{
    FRAME_HEADER_LEN, FrameError, PREAMBLE_LEN, PreambleError, decode_frame, decode_preamble,
    encode_frame, encode_preamble, frame_len,
};

#[derive(Debug, Error)]
pub enum NetError {
    #[error("reading from the stream failed: {0}")]
    Read(#[from] ReadExactError),
    #[error("writing to the stream failed: {0}")]
    Write(#[from] WriteError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Preamble(#[from] PreambleError),
}

impl NetError {
    /// Whether the error means the peer went away (connection lost or stream
    /// ended), as opposed to the peer sending something invalid.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Read(_) | Self::Write(_))
    }
}

pub async fn write_preamble(send: &mut SendStream, version: u32) -> Result<(), NetError> {
    send.write_all(&encode_preamble(version)).await?;
    Ok(())
}

pub async fn read_preamble(recv: &mut RecvStream) -> Result<u32, NetError> {
    let mut bytes = [0; PREAMBLE_LEN];
    recv.read_exact(&mut bytes).await?;
    Ok(decode_preamble(bytes)?)
}

pub async fn write_message<T: Serialize>(
    send: &mut SendStream,
    message: &T,
    max: usize,
) -> Result<(), NetError> {
    let frame = encode_frame(message, max)?;
    send.write_all(&frame).await?;
    Ok(())
}

/// Writes a frame produced earlier by `encode_frame`, so a message sent to
/// many peers is encoded once.
pub async fn write_frame(send: &mut SendStream, frame: &[u8]) -> Result<(), NetError> {
    send.write_all(frame).await?;
    Ok(())
}

/// Reads one frame. The length is checked against `max` before anything is
/// allocated, so a hostile peer cannot make the reader reserve more than
/// `max` bytes.
pub async fn read_message<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<T, NetError> {
    let mut header = [0; FRAME_HEADER_LEN];
    recv.read_exact(&mut header).await?;
    let len = frame_len(header, max)?;
    let mut payload = vec![0; len];
    recv.read_exact(&mut payload).await?;
    Ok(decode_frame(&payload)?)
}
