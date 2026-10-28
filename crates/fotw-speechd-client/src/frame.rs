//! Length-prefixed frames, shared by both ends of the pipe.
//!
//! ```text
//! u32 big-endian L | u8 kind | L-1 bytes of payload      (1 ≤ L ≤ MAX_FRAME)
//! ```
//!
//! The length is validated before anything is allocated for the payload, so a
//! peer cannot make the reader allocate an oversized frame. Everything here is
//! pure except the two `read_frame` adapters.

use std::io::{self, Read};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest frame, counting the kind byte but not the length prefix.
pub const MAX_FRAME: usize = 1 << 20;
/// Largest control payload.
pub const MAX_CONTROL: usize = 64 << 10;
/// Kind byte of a JSON control frame.
pub const KIND_CONTROL: u8 = 0x01;
/// Kind byte of a PCM audio frame.
pub const KIND_AUDIO: u8 = 0x02;
/// Most samples one audio frame can carry.
pub const MAX_SAMPLES_PER_FRAME: usize = (MAX_FRAME - 1 - 4) / 2;

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A control message's JSON bytes, not yet parsed.
    Control(Vec<u8>),
    /// PCM for one stream.
    Audio {
        /// Stream id.
        stream: u32,
        /// Mono signed 16-bit samples.
        pcm: Vec<i16>,
    },
}

/// Why a frame was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// A zero length: not even a kind byte.
    #[error("empty frame")]
    Empty,
    /// The length prefix exceeds [`MAX_FRAME`].
    #[error("frame of {0} bytes exceeds the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    /// A control payload over [`MAX_CONTROL`].
    #[error("control message of {0} bytes exceeds the {MAX_CONTROL}-byte limit")]
    ControlTooLarge(usize),
    /// A kind byte that is neither control nor audio.
    #[error("unknown frame kind {0:#04x}")]
    UnknownKind(u8),
    /// An audio frame with no stream id, no samples or half a sample.
    #[error("malformed audio frame: {0}")]
    BadAudio(&'static str),
    /// A control payload that is not a valid message.
    #[error("malformed control message: {0}")]
    BadControl(String),
    /// The stream ended inside a frame.
    #[error("stream ended inside a frame")]
    Truncated,
    /// The pipe failed.
    #[error("i/o: {0}")]
    Io(String),
}

/// Encode a control message.
///
/// # Errors
/// [`FrameError::ControlTooLarge`] past [`MAX_CONTROL`], or
/// [`FrameError::BadControl`] if it does not serialize.
pub fn encode_control<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(message).map_err(|e| FrameError::BadControl(e.to_string()))?;
    if json.len() > MAX_CONTROL {
        return Err(FrameError::ControlTooLarge(json.len()));
    }
    Ok(with_header(KIND_CONTROL, &json))
}

/// Encode one audio frame.
///
/// # Errors
/// [`FrameError::BadAudio`] for no samples or more than
/// [`MAX_SAMPLES_PER_FRAME`].
pub fn encode_audio(stream: u32, pcm: &[i16]) -> Result<Vec<u8>, FrameError> {
    if pcm.is_empty() {
        return Err(FrameError::BadAudio("no samples"));
    }
    if pcm.len() > MAX_SAMPLES_PER_FRAME {
        return Err(FrameError::BadAudio("too many samples for one frame"));
    }
    let mut payload = Vec::with_capacity(4 + pcm.len() * 2);
    payload.extend_from_slice(&stream.to_be_bytes());
    for sample in pcm {
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(with_header(KIND_AUDIO, &payload))
}

fn with_header(kind: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len() + 1).expect("payload bounded by MAX_FRAME");
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

/// Validate a length prefix before reading what it describes.
///
/// # Errors
/// [`FrameError::Empty`] or [`FrameError::TooLarge`].
pub fn check_length(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(prefix) as usize;
    match len {
        0 => Err(FrameError::Empty),
        n if n > MAX_FRAME => Err(FrameError::TooLarge(n)),
        n => Ok(n),
    }
}

/// Decode a frame's body: its kind byte and payload.
///
/// # Errors
/// Any [`FrameError`] describing the malformation.
pub fn decode_body(body: &[u8]) -> Result<Frame, FrameError> {
    let (&kind, payload) = body.split_first().ok_or(FrameError::Empty)?;
    match kind {
        KIND_CONTROL => {
            if payload.len() > MAX_CONTROL {
                return Err(FrameError::ControlTooLarge(payload.len()));
            }
            Ok(Frame::Control(payload.to_vec()))
        }
        KIND_AUDIO => {
            if payload.len() < 4 {
                return Err(FrameError::BadAudio("no stream id"));
            }
            let (id, samples) = payload.split_at(4);
            if samples.is_empty() {
                return Err(FrameError::BadAudio("no samples"));
            }
            if samples.len() % 2 != 0 {
                return Err(FrameError::BadAudio("odd byte count"));
            }
            Ok(Frame::Audio {
                stream: u32::from_be_bytes(id.try_into().expect("four bytes")),
                pcm: samples
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect(),
            })
        }
        other => Err(FrameError::UnknownKind(other)),
    }
}

/// Read one frame. `Ok(None)` is a clean end of stream between frames.
///
/// # Errors
/// A malformed frame, a stream that ends mid-frame, or an I/O failure.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut prefix = [0u8; 4];
    if !fill(reader, &mut prefix, true)? {
        return Ok(None);
    }
    let len = check_length(prefix)?;
    let mut body = vec![0u8; len];
    fill(reader, &mut body, false)?;
    decode_body(&body).map(Some)
}

/// Fill `buf`. Returns `false` only when `eof_ok` and nothing at all was read.
fn fill<R: Read>(reader: &mut R, buf: &mut [u8], eof_ok: bool) -> Result<bool, FrameError> {
    let mut read = 0;
    while read < buf.len() {
        match reader.read(&mut buf[read..]) {
            Ok(0) if read == 0 && eof_ok => return Ok(false),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e.to_string())),
        }
    }
    Ok(true)
}

/// Async [`read_frame`].
///
/// # Errors
/// As [`read_frame`].
pub async fn read_frame_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Frame>, FrameError> {
    let mut prefix = [0u8; 4];
    let mut read = 0;
    while read < 4 {
        match reader.read(&mut prefix[read..]).await {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => read += n,
            Err(e) => return Err(FrameError::Io(e.to_string())),
        }
    }
    let len = check_length(prefix)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e.to_string())
        }
    })?;
    decode_body(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(bytes: &[u8]) -> Result<Option<Frame>, FrameError> {
        read_frame(&mut &bytes[..])
    }

    #[test]
    fn audio_round_trips_little_endian_samples() {
        let bytes = encode_audio(7, &[i16::MIN, -1, 0, 1, i16::MAX]).unwrap();
        assert_eq!(&bytes[..5], &[0, 0, 0, 15, KIND_AUDIO]);
        assert_eq!(&bytes[9..11], &[0x00, 0x80]);
        assert_eq!(
            read_all(&bytes).unwrap(),
            Some(Frame::Audio {
                stream: 7,
                pcm: vec![i16::MIN, -1, 0, 1, i16::MAX]
            })
        );
    }

    #[test]
    fn the_largest_audio_frame_fits_and_one_more_sample_does_not() {
        let max = vec![1i16; MAX_SAMPLES_PER_FRAME];
        let bytes = encode_audio(1, &max).unwrap();
        assert!(bytes.len() - 4 <= MAX_FRAME);
        assert!(encode_audio(1, &[1i16; MAX_SAMPLES_PER_FRAME + 1]).is_err());
        assert!(encode_audio(1, &[]).is_err());
    }

    #[test]
    fn an_oversized_length_is_refused_before_its_payload_is_read() {
        let mut prefix = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes().to_vec();
        // No payload follows: refusing must not wait for, or allocate, one.
        prefix.push(KIND_CONTROL);
        assert_eq!(read_all(&prefix), Err(FrameError::TooLarge(MAX_FRAME + 1)));
        assert_eq!(
            check_length([0xff; 4]),
            Err(FrameError::TooLarge(u32::MAX as usize))
        );
    }

    #[test]
    fn malformed_frames_are_refused() {
        let cases: [(&[u8], FrameError); 6] = [
            (&[0, 0, 0, 0], FrameError::Empty),
            (&[0, 0, 0, 2, 0x09, 0], FrameError::UnknownKind(0x09)),
            (
                &[0, 0, 0, 3, KIND_AUDIO, 0, 0],
                FrameError::BadAudio("no stream id"),
            ),
            (
                &[0, 0, 0, 5, KIND_AUDIO, 0, 0, 0, 1],
                FrameError::BadAudio("no samples"),
            ),
            (
                &[0, 0, 0, 6, KIND_AUDIO, 0, 0, 0, 1, 7],
                FrameError::BadAudio("odd byte count"),
            ),
            (&[0, 0, 0, 9, KIND_CONTROL, b'{'], FrameError::Truncated),
        ];
        for (bytes, expected) in cases {
            assert_eq!(read_all(bytes), Err(expected), "{bytes:?}");
        }
        let mut control = (u32::try_from(MAX_CONTROL + 2).unwrap())
            .to_be_bytes()
            .to_vec();
        control.push(KIND_CONTROL);
        control.extend(std::iter::repeat_n(b' ', MAX_CONTROL + 1));
        assert_eq!(
            read_all(&control),
            Err(FrameError::ControlTooLarge(MAX_CONTROL + 1))
        );
    }

    #[test]
    fn a_clean_end_between_frames_is_not_an_error_but_a_torn_prefix_is() {
        assert_eq!(read_all(&[]), Ok(None));
        assert_eq!(read_all(&[0, 0]), Err(FrameError::Truncated));
    }

    #[tokio::test]
    async fn the_async_reader_agrees_with_the_blocking_one() {
        let mut bytes = encode_control(&serde_json::json!({"type": "health", "id": 1})).unwrap();
        bytes.extend(encode_audio(2, &[5, 6]).unwrap());
        let mut reader = &bytes[..];
        assert!(matches!(
            read_frame_async(&mut reader).await,
            Ok(Some(Frame::Control(_)))
        ));
        assert_eq!(
            read_frame_async(&mut reader).await,
            Ok(Some(Frame::Audio {
                stream: 2,
                pcm: vec![5, 6]
            }))
        );
        assert_eq!(read_frame_async(&mut reader).await, Ok(None));
    }
}
