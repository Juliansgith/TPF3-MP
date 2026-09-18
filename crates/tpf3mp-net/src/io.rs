use quinn::{ReadExactError, RecvStream, SendStream, WriteError};
use thiserror::Error;
use tpf3mp_proto::{
    FRAME_HEADER_LEN, FrameError, Message, PREAMBLE_LEN, PreambleError, decode_message,
    decode_preamble, encode_frame, encode_preamble, frame_len,
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

pub async fn write_preamble(send: &mut SendStream, version: u32) -> Result<(), NetError> {
    send.write_all(&encode_preamble(version)).await?;
    Ok(())
}

pub async fn read_preamble(recv: &mut RecvStream) -> Result<u32, NetError> {
    let mut bytes = [0; PREAMBLE_LEN];
    recv.read_exact(&mut bytes).await?;
    Ok(decode_preamble(bytes)?)
}

pub async fn write_message(
    send: &mut SendStream,
    message: &Message,
    max: usize,
) -> Result<(), NetError> {
    let frame = encode_frame(message, max)?;
    send.write_all(&frame).await?;
    Ok(())
}

/// Reads one frame. The length is checked against `max` before anything is
/// allocated, so a hostile peer cannot make the reader reserve more than
/// `max` bytes.
pub async fn read_message(recv: &mut RecvStream, max: usize) -> Result<Message, NetError> {
    let mut header = [0; FRAME_HEADER_LEN];
    recv.read_exact(&mut header).await?;
    let len = frame_len(header, max)?;
    let mut payload = vec![0; len];
    recv.read_exact(&mut payload).await?;
    Ok(decode_message(&payload)?)
}
