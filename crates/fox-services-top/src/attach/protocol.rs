//! Wire protocol between the headless collector (`--daemon`) and an
//! attaching viewer frontend.
//!
//! Framing is a 4-byte big-endian length followed by that many bytes of
//! `serde_json`. Every message is one frame, in each direction. The viewer
//! always speaks first ([`ClientMessage::Hello`]); the collector answers
//! with [`ServerMessage::HelloAck`] (carrying the session the viewer must
//! address its commands to) or [`ServerMessage::Incompatible`].
//!
//! Everything the viewer sends is advisory: a command is applied to the
//! collector's one session, a viewport change only affects projection
//! windowing and a detach only ends this viewer's connection — none of them
//! can stop the collector. Only the collector decides when to stop, and it
//! tells the viewer with [`ServerMessage::ShutdownRequested`].

use std::io::{self, Read, Write};
use std::sync::Arc;

use fox_services_top_core::commands::{CommandEnvelope, CommandResult};
use fox_services_top_core::identity::SessionId;
use fox_services_top_core::view::ViewModel;
use serde::{Deserialize, Serialize};

/// Bumped whenever the framing or either message enum changes; a viewer and
/// a collector of different versions refuse to attach instead of
/// misreading each other.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest framed message accepted, in bytes. One projection is bounded by
/// `runtime.max_projection_bytes` (at most 1 MiB) and one command by the
/// ingress caps, so this only defends a reader against a bogus length
/// prefix.
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// Viewer to collector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message; the collector answers [`ServerMessage::HelloAck`] or
    /// [`ServerMessage::Incompatible`].
    Hello { version: u32 },
    /// A controller command to apply to the collector's session.
    Submit { command: CommandEnvelope },
    /// Pure viewport change (projection windowing only).
    SetViewport { columns: u16, rows: u16 },
    /// The viewer is leaving; the collector ends this connection and waits
    /// for the next viewer. Never stops the collector.
    Detach,
}

/// Collector to viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Attach accepted; `session_id` is the collector's session, which every
    /// following [`ClientMessage::Submit`] must carry.
    HelloAck {
        version: u32,
        session_id: SessionId,
        daemon_pid: u32,
    },
    /// The collector speaks another protocol version and will not serve
    /// this viewer.
    Incompatible { version: u32 },
    /// The latest projection; replaces any the viewer has not taken yet.
    Projection { view: Arc<ViewModel> },
    /// A command result delivered independently of projections.
    Result { result: CommandResult },
    /// The collector is shutting down; the viewer must exit.
    ShutdownRequested,
}

/// Writes one frame (4-byte big-endian length, then the payload).
pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame larger than u32"))?;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the protocol maximum",
        ));
    }
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Serializes and writes one message frame.
pub fn write_message(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(io::Error::other)?;
    write_frame(writer, &payload)
}

/// Reads whole frames from a stream without losing partial reads to a read
/// timeout: bytes are accumulated until a complete frame is present, so a
/// timeout between reads is [`FrameReader::next_frame`] returning `Ok(None)`
/// rather than an error.
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
        }
    }

    /// Returns the next whole frame, or `Ok(None)` when the underlying read
    /// timed out/would block (retry later). A clean EOF between frames and a
    /// truncation mid-frame both come back as [`io::ErrorKind::UnexpectedEof`].
    pub fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self.take_complete()? {
                return Ok(Some(frame));
            }
            let mut scratch = [0u8; 16 * 1024];
            match self.inner.read(&mut scratch) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the peer closed the connection",
                    ));
                }
                Ok(read) => self.buffer.extend_from_slice(&scratch[..read]),
                Err(error) if is_retryable(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }

    /// Decodes the next message, or `Ok(None)` on a retryable timeout.
    pub fn next_message<M: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<M>> {
        match self.next_frame()? {
            None => Ok(None),
            Some(frame) => serde_json::from_slice(&frame)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        }
    }

    /// Splits off a complete frame from the buffer, if one is present.
    fn take_complete(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(
            self.buffer[..4]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length"))?,
        );
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds the protocol maximum",
            ));
        }
        let total = 4 + len as usize;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let frame = self.buffer[..total].to_vec();
        self.buffer.drain(..total);
        Ok(Some(frame[4..].to_vec()))
    }
}

/// Whether the error means "nothing was read, try again" (a socket read or
/// write timeout).
pub fn is_retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn frames_round_trip_through_a_reader() {
        let messages = vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            ClientMessage::SetViewport {
                columns: 120,
                rows: 40,
            },
            ClientMessage::Detach,
        ];
        let mut wire = Vec::new();
        for message in &messages {
            write_message(&mut wire, message).unwrap();
        }
        let mut reader = FrameReader::new(Cursor::new(wire));
        for expected in &messages {
            let decoded: ClientMessage = reader.next_message().unwrap().unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    /// A reader that would block once, then yields its bytes: models a socket
    /// read timeout in the middle of a frame.
    struct BlocksThenYields {
        data: Vec<u8>,
        position: usize,
        blocked: bool,
    }

    impl Read for BlocksThenYields {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "wait"));
            }
            let remaining = &self.data[self.position..];
            let take = remaining.len().min(buffer.len());
            buffer[..take].copy_from_slice(&remaining[..take]);
            self.position += take;
            Ok(take)
        }
    }

    #[test]
    fn a_timeout_between_reads_is_none_not_an_error() {
        let payload = serde_json::to_vec(&ClientMessage::Detach).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &payload).unwrap();
        let mut reader = FrameReader::new(BlocksThenYields {
            data: wire,
            position: 0,
            blocked: false,
        });
        // The first read would block: nothing to hand back yet.
        assert!(reader.next_frame().unwrap().is_none());
        // The retry completes the frame.
        assert_eq!(
            reader.next_frame().unwrap().unwrap(),
            serde_json::to_vec(&ClientMessage::Detach).unwrap()
        );
    }

    #[test]
    fn a_truncated_frame_is_an_unexpected_eof() {
        let payload = serde_json::to_vec(&ClientMessage::Detach).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &payload).unwrap();
        let mut short = FrameReader::new(Cursor::new(wire[..wire.len() - 1].to_vec()));
        assert_eq!(
            short.next_frame().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn an_oversized_length_prefix_is_refused() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        let mut reader = FrameReader::new(Cursor::new(wire));
        assert_eq!(
            reader.next_frame().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
