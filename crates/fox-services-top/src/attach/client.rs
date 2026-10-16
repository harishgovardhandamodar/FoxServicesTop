//! The viewer side of the attach socket: attaches to a `--daemon`
//! collector, receives its live projections over the socket and implements
//! [`SessionPort`] so the terminal UI runs unchanged against a remote
//! session.
//!
//! Two threads per attachment: a reader turning server frames into the
//! session's projection slot and result queue, and a writer sending the
//! viewer's commands, viewport changes and detach. A shutdown request from
//! the UI is local (it detaches this viewer); it is never forwarded to the
//! collector, which only stops when it is itself told to.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError, bounded};
use fox_services_top_core::commands::{CommandEnvelope, CommandRejection, CommandResult};
use fox_services_top_core::errors::ErrorCode;
use fox_services_top_core::identity::SessionId;
use fox_services_top_core::ports::SessionPort;
use fox_services_top_core::view::ViewModel;

use super::protocol::{
    ClientMessage, FrameReader, PROTOCOL_VERSION, ServerMessage, is_retryable, write_frame,
    write_message,
};

/// Commands queued for the writer thread before submissions are rejected.
const OUTBOUND_QUEUE: usize = 1024;
/// Command results buffered for the UI before the oldest are dropped.
const RESULT_QUEUE: usize = 4096;
/// Bound on the Hello/HelloAck exchange.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// What happened when trying to attach.
pub(crate) enum Attach {
    /// Attached; the viewer runs against the collector's session.
    Attached(RemoteSession),
    /// No collector is listening (no socket, or nothing bound to it).
    NotRunning,
    /// A collector is listening but speaks another protocol version.
    Incompatible { daemon: u32, ours: u32 },
}

/// Connects to the collector at `socket_path` and performs the handshake.
/// `Ok(Attach::NotRunning)` is the ordinary "no daemon" case; other errors
/// mean something is listening but not speaking the protocol.
pub(crate) fn connect(socket_path: &Path) -> io::Result<Attach> {
    let stream = match UnixStream::connect(socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(Attach::NotRunning);
        }
        Err(error) => return Err(error),
    };
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let read_stream = stream.try_clone()?;
    let mut reader = FrameReader::new(read_stream);

    write_message(
        &mut &stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let reply = loop {
        match reader.next_message::<ServerMessage>() {
            Ok(Some(message)) => break message,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the collector did not answer the attach handshake",
                    ));
                }
            }
            Err(error) if is_retryable(&error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    };
    match reply {
        ServerMessage::HelloAck {
            version,
            session_id,
            ..
        } if version == PROTOCOL_VERSION => {
            // Socket times are shared by every duplicate; the session's
            // reader blocks until the socket is shut down on drop.
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            Ok(Attach::Attached(RemoteSession::spawn(stream, session_id)?))
        }
        ServerMessage::HelloAck { version, .. } => Ok(Attach::Incompatible {
            daemon: version,
            ours: PROTOCOL_VERSION,
        }),
        ServerMessage::Incompatible { version } => Ok(Attach::Incompatible {
            daemon: version,
            ours: PROTOCOL_VERSION,
        }),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("collector answered the handshake with {other:?}"),
        )),
    }
}

/// Shared attachment state, split from `RemoteSession` so both threads hold
/// an `Arc` of it.
#[derive(Default)]
struct Attachment {
    projection: Mutex<Option<Arc<ViewModel>>>,
    results: Mutex<VecDeque<CommandResult>>,
    /// The collector asked the viewer to exit (shutdown or a broken link).
    daemon_shutdown: AtomicBool,
}

/// A [`SessionPort`] backed by a live connection to a `--daemon` collector.
pub(crate) struct RemoteSession {
    session_id: SessionId,
    outbound: Sender<Vec<u8>>,
    shared: Arc<Attachment>,
    /// The original stream, kept only to shut the connection down on drop.
    stream: UnixStream,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl RemoteSession {
    fn spawn(stream: UnixStream, session_id: SessionId) -> io::Result<Self> {
        let shared = Arc::new(Attachment::default());
        let (outbound, inbound) = bounded::<Vec<u8>>(OUTBOUND_QUEUE);

        let reader_stream = stream.try_clone()?;
        let writer_stream = stream.try_clone()?;
        let reader_shared = Arc::clone(&shared);
        let reader = spawn_named("fox-services-top-attach-client-reader", move || {
            reader_loop(reader_stream, &reader_shared);
        })?;
        let writer = spawn_named("fox-services-top-attach-client-writer", move || {
            writer_loop(writer_stream, &inbound);
        })?;
        Ok(Self {
            session_id,
            outbound,
            shared,
            stream,
            reader: Some(reader),
            writer: Some(writer),
        })
    }

    /// Frames and queues one client message; `false` when the connection is
    /// gone or the queue is full (the caller then rejects the submission).
    fn enqueue(&self, message: &ClientMessage) -> Result<(), QueueFull> {
        let payload = serde_json::to_vec(message).map_err(|_| QueueFull::Gone)?;
        let mut frame = Vec::new();
        write_frame(&mut frame, &payload).map_err(|_| QueueFull::Gone)?;
        match self.outbound.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(QueueFull::Full),
            Err(TrySendError::Disconnected(_)) => Err(QueueFull::Gone),
        }
    }
}

/// Why a client message could not be queued.
enum QueueFull {
    Full,
    Gone,
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        // Best effort: tell the collector this viewer is leaving, then shut
        // the socket down so the reader thread's blocking read returns.
        let _ = self.enqueue(&ClientMessage::Detach);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        // The threads own clones of the socket and end once it is shut
        // down; detach rather than join so a wedged peer cannot block exit.
        drop(self.reader.take());
        drop(self.writer.take());
    }
}

impl SessionPort for RemoteSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        let request_id = command.request_id;
        match self.enqueue(&ClientMessage::Submit { command }) {
            Ok(()) => Ok(()),
            Err(QueueFull::Full) => Err(CommandRejection {
                request_id,
                code: ErrorCode::Busy,
                message: "commands to the collector are queued faster than it reads them"
                    .to_owned(),
            }),
            Err(QueueFull::Gone) => Err(CommandRejection {
                request_id,
                code: ErrorCode::Unavailable,
                message: "the collector connection is closed".to_owned(),
            }),
        }
    }

    fn take_projection(&self) -> Option<Arc<ViewModel>> {
        self.shared
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn drain_results(&self, max: usize) -> Vec<CommandResult> {
        let mut results = self
            .shared
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let take = max.min(results.len());
        results.drain(..take).collect()
    }

    fn request_shutdown(&self, _reason: &str) {
        // Local: never forward a shutdown to the collector, which would end
        // the collection the viewer is only watching. The detach on drop
        // ends this connection; the collector keeps running.
        let _ = self.enqueue(&ClientMessage::Detach);
    }

    fn shutdown_requested(&self) -> bool {
        self.shared.daemon_shutdown.load(Ordering::Acquire)
    }

    fn set_viewport(&self, columns: u16, rows: u16) {
        let _ = self.enqueue(&ClientMessage::SetViewport { columns, rows });
    }
}

fn spawn_named(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
}

fn reader_loop(stream: UnixStream, shared: &Arc<Attachment>) {
    let mut reader = FrameReader::new(stream);
    loop {
        match reader.next_message::<ServerMessage>() {
            Ok(Some(ServerMessage::Projection { view })) => {
                let mut slot = shared
                    .projection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *slot = Some(view);
            }
            Ok(Some(ServerMessage::Result { result })) => {
                let mut results = shared
                    .results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if results.len() >= RESULT_QUEUE {
                    results.pop_front();
                }
                results.push_back(result);
            }
            Ok(Some(ServerMessage::ShutdownRequested)) => {
                shared.daemon_shutdown.store(true, Ordering::Release);
                return;
            }
            // A repeated hello/ack or an incompatible notice after the
            // handshake is unexpected; not worth dropping the link over.
            Ok(Some(ServerMessage::HelloAck { .. } | ServerMessage::Incompatible { .. })) => {}
            Ok(None) => {}
            Err(_) => {
                // The collector is gone (or the connection broke): the
                // viewer must end rather than show a frozen screen.
                shared.daemon_shutdown.store(true, Ordering::Release);
                return;
            }
        }
    }
}

fn writer_loop(stream: UnixStream, inbound: &crossbeam_channel::Receiver<Vec<u8>>) {
    let mut stream = stream;
    for frame in inbound {
        if stream
            .write_all(&frame)
            .and_then(|()| stream.flush())
            .is_err()
        {
            return;
        }
    }
}
