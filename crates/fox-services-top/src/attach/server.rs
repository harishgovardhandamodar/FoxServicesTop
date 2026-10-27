//! The collector side of the attach socket (`--daemon`).
//!
//! One viewer at a time: the server accepts a connection, runs the
//! [`protocol::ClientMessage::Hello`] handshake, then serves that viewer in
//! two threads — one reading its commands (applied to the collector's one
//! session) and one writing the session's projections and command results
//! back — until the viewer detaches, the connection breaks or the collector
//! shuts down. A viewer can never stop the collector: a detach or a broken
//! connection only ends that connection.
//!
//! Generic over the session type so the composition root passes its
//! [`SessionHandle`] and the tests pass a fake; everything it needs is the
//! [`SessionPort`] contract.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use fox_services_top_core::commands::{CommandEnvelope, CommandResult};
use fox_services_top_core::errors::ErrorDetail;
use fox_services_top_core::ports::{LogLevel, Logger, SessionPort};

use super::protocol::{ClientMessage, FrameReader, PROTOCOL_VERSION, ServerMessage, write_message};

/// How often the accept loop looks at the stop flag while no viewer waits.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// How long one serve iteration waits for a message from the reader thread
/// before polling the session for a new projection.
const SERVE_POLL: Duration = Duration::from_millis(25);
/// Read timeout of the per-viewer reader thread; it only exists so the
/// thread notices the collector's stop flag.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// Write timeout of the per-viewer writer; a viewer that stops reading must
/// not wedge the collector thread forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Command results relayed to a viewer in one serve iteration.
const MAX_RESULTS: usize = 64;
/// Queued messages from the reader thread (immediate rejections) to the
/// writer; never approached in practice.
const OUTBOUND_QUEUE: usize = 64;

/// Binds `socket_path` (removing a stale socket left by a previous run) and
/// serves viewers on a background thread until `stop` is set. Binding
/// happens here, on the caller's thread, so a bind failure stops the daemon
/// at startup instead of silently.
pub(crate) fn serve_in_background<S>(
    session: S,
    socket_path: &Path,
    logger: Arc<dyn Logger>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>>
where
    S: SessionPort + Clone + Send + 'static,
{
    let listener = bind(socket_path)?;
    let path = socket_path.to_path_buf();
    std::thread::Builder::new()
        .name("fox-services-top-attach".to_owned())
        .spawn(move || accept_loop(&listener, &path, session, logger, stop))
}

/// Binds the listener with a private socket file. A leftover socket can only
/// be stale: the daemon holds the instance lock, so no other collector is
/// serving this state directory.
fn bind(socket_path: &Path) -> io::Result<UnixListener> {
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn accept_loop<S>(
    listener: &UnixListener,
    socket_path: &Path,
    session: S,
    logger: Arc<dyn Logger>,
    stop: Arc<AtomicBool>,
) where
    S: SessionPort + Clone + Send + 'static,
{
    if let Err(error) = listener.set_nonblocking(true) {
        log(
            &logger,
            LogLevel::Error,
            &format!("attach socket cannot be polled: {error}"),
        );
        return;
    }
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _peer)) => serve_client(&stream, &session, &logger, &stop),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(error) => {
                log(
                    &logger,
                    LogLevel::Warn,
                    &format!("attach accept failed: {error}"),
                );
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }
    // Best effort; a leftover socket is removed on the next start anyway.
    let _ = std::fs::remove_file(socket_path);
}

/// Serves one viewer until it detaches or the collector stops.
fn serve_client<S>(
    stream: &UnixStream,
    session: &S,
    logger: &Arc<dyn Logger>,
    stop: &Arc<AtomicBool>,
) where
    S: SessionPort + Clone + Send + 'static,
{
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let reader_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(error) => {
            log(
                logger,
                LogLevel::Warn,
                &format!("attach connection cannot be read: {error}"),
            );
            return;
        }
    };
    let _ = reader_stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut reader = FrameReader::new(reader_stream);

    if !handshake(stream, &mut reader, session, logger) {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    log(logger, LogLevel::Info, "a viewer attached");

    let (outbound_tx, outbound_rx) = bounded::<ServerMessage>(OUTBOUND_QUEUE);
    let reader_done = Arc::new(AtomicBool::new(false));
    let reader_thread = spawn_reader(
        reader,
        session.clone(),
        Arc::clone(logger),
        Arc::clone(stop),
        &outbound_tx,
        Arc::clone(&reader_done),
    );

    let mut writer = stream;
    let mut sent_all = true;
    while !stop.load(Ordering::Acquire) && !reader_done.load(Ordering::Acquire) && sent_all {
        if let Some(view) = session.take_projection() {
            sent_all = write_message(&mut writer, &ServerMessage::Projection { view }).is_ok();
        }
        for result in session.drain_results(MAX_RESULTS) {
            if write_message(&mut writer, &ServerMessage::Result { result }).is_err() {
                sent_all = false;
                break;
            }
        }
        if !sent_all {
            break;
        }
        if session.shutdown_requested() {
            let _ = write_message(&mut writer, &ServerMessage::ShutdownRequested);
            break;
        }
        match outbound_rx.recv_timeout(SERVE_POLL) {
            Ok(message) => sent_all = write_message(&mut writer, &message).is_ok(),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    // Wakes the reader thread's blocking read; it exits on its own. The
    // handle is detached rather than joined: the thread only owns a socket
    // clone and ends once the socket is shut down, so joining is
    // unnecessary and would be unbounded.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(reader_thread);
    log(logger, LogLevel::Info, "the viewer detached");
}

/// The Hello/HelloAck exchange; `false` when the viewer must be dropped.
fn handshake<S>(
    stream: &UnixStream,
    reader: &mut FrameReader<UnixStream>,
    session: &S,
    logger: &Arc<dyn Logger>,
) -> bool
where
    S: SessionPort,
{
    let version = loop {
        match reader.next_message::<ClientMessage>() {
            Ok(Some(ClientMessage::Hello { version })) => break version,
            Ok(Some(_)) => {
                log(
                    logger,
                    LogLevel::Warn,
                    "a viewer sent a message before its hello",
                );
                return false;
            }
            Ok(None) => continue,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return false,
            Err(error) => {
                log(
                    logger,
                    LogLevel::Warn,
                    &format!("attach handshake failed: {error}"),
                );
                return false;
            }
        }
    };
    let mut writer = stream;
    if version != PROTOCOL_VERSION {
        let _ = write_message(
            &mut writer,
            &ServerMessage::Incompatible {
                version: PROTOCOL_VERSION,
            },
        );
        return false;
    }
    let ack = ServerMessage::HelloAck {
        version: PROTOCOL_VERSION,
        session_id: session.session_id().clone(),
        daemon_pid: std::process::id(),
    };
    if write_message(&mut writer, &ack).is_err() {
        return false;
    }
    log(
        logger,
        LogLevel::Info,
        "attach handshake accepted (protocol v1)",
    );
    true
}

fn spawn_reader<S>(
    reader: FrameReader<UnixStream>,
    session: S,
    logger: Arc<dyn Logger>,
    stop: Arc<AtomicBool>,
    outbound: &Sender<ServerMessage>,
    done: Arc<AtomicBool>,
) -> Option<JoinHandle<()>>
where
    S: SessionPort + Send + 'static,
{
    let outbound = outbound.clone();
    let thread_logger = Arc::clone(&logger);
    std::thread::Builder::new()
        .name("fox-services-top-attach-reader".to_owned())
        .spawn(move || {
            reader_loop(reader, &session, &thread_logger, &stop, &outbound);
            done.store(true, Ordering::Release);
        })
        .map_err(|error| {
            log(
                &logger,
                LogLevel::Warn,
                &format!("attach reader thread failed to start: {error}"),
            );
        })
        .ok()
}

fn reader_loop<S>(
    mut reader: FrameReader<UnixStream>,
    session: &S,
    logger: &Arc<dyn Logger>,
    stop: &Arc<AtomicBool>,
    outbound: &Sender<ServerMessage>,
) where
    S: SessionPort,
{
    while !stop.load(Ordering::Acquire) {
        match reader.next_message::<ClientMessage>() {
            Ok(Some(ClientMessage::Submit { command })) => {
                apply_command(session, command, outbound);
            }
            Ok(Some(ClientMessage::SetViewport { columns, rows })) => {
                session.set_viewport(columns, rows);
            }
            Ok(Some(ClientMessage::Detach)) => break,
            // A repeated hello is a protocol oddity, not a reason to drop.
            Ok(Some(ClientMessage::Hello { .. })) => {}
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                log(
                    logger,
                    LogLevel::Debug,
                    &format!("attach read ended: {error}"),
                );
                break;
            }
        }
    }
}

/// Applies one command to the collector's session; an immediate rejection is
/// relayed to the viewer as a command result, exactly as a local session
/// would report a rejected submission.
fn apply_command<S>(session: &S, command: CommandEnvelope, outbound: &Sender<ServerMessage>)
where
    S: SessionPort,
{
    let Err(rejection) = session.submit(command) else {
        return;
    };
    let detail = ErrorDetail::new(rejection.code, &rejection.message);
    let result = CommandResult::from_error(rejection.request_id, &detail);
    let _ = outbound.send(ServerMessage::Result { result });
}

fn log(logger: &Arc<dyn Logger>, level: LogLevel, message: &str) {
    logger.log(level, "attach", message);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use fox_services_top_core::commands::{CommandBody, CommandEnvelope, CommandRejection};
    use fox_services_top_core::config::Theme;
    use fox_services_top_core::identity::SessionId;
    use fox_services_top_core::metric::CanonicalUnit;
    use fox_services_top_core::testing::MemoryLogger;
    use fox_services_top_core::tiers::BarSize;
    use fox_services_top_core::view::{
        CellState, HeaderView, MetricCell, ModuleViews, OverloadState, SessionState, ViewModel,
    };

    use super::*;
    use crate::attach::client::{Attach, connect};

    /// A session with no Manager behind it: enough to drive the server.
    #[derive(Clone)]
    struct FakeSession {
        shared: Arc<FakeShared>,
    }

    struct FakeShared {
        id: SessionId,
        projection: Mutex<Option<Arc<ViewModel>>>,
        results: Mutex<Vec<CommandResult>>,
        shutdown: AtomicBool,
        submissions: Mutex<Vec<CommandEnvelope>>,
        viewport: Mutex<(u16, u16)>,
    }

    impl FakeSession {
        fn new(id: &str) -> Self {
            Self {
                shared: Arc::new(FakeShared {
                    id: SessionId::new(id).unwrap(),
                    projection: Mutex::new(None),
                    results: Mutex::new(Vec::new()),
                    shutdown: AtomicBool::new(false),
                    submissions: Mutex::new(Vec::new()),
                    viewport: Mutex::new((0, 0)),
                }),
            }
        }

        fn publish(&self, revision: u64) {
            *self.shared.projection.lock().unwrap() = Some(Arc::new(view(revision)));
        }

        fn submitted(&self) -> usize {
            self.shared.submissions.lock().unwrap().len()
        }

        fn request_stop(&self) {
            self.shared.shutdown.store(true, Ordering::Release);
        }
    }

    impl SessionPort for FakeSession {
        fn session_id(&self) -> &SessionId {
            &self.shared.id
        }

        fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
            self.shared.submissions.lock().unwrap().push(command);
            Ok(())
        }

        fn take_projection(&self) -> Option<Arc<ViewModel>> {
            self.shared.projection.lock().unwrap().take()
        }

        fn drain_results(&self, max: usize) -> Vec<CommandResult> {
            let mut results = self.shared.results.lock().unwrap();
            let take = max.min(results.len());
            results.drain(..take).collect()
        }

        fn request_shutdown(&self, _reason: &str) {
            self.request_stop();
        }

        fn shutdown_requested(&self) -> bool {
            self.shared.shutdown.load(Ordering::Acquire)
        }

        fn set_viewport(&self, columns: u16, rows: u16) {
            *self.shared.viewport.lock().unwrap() = (columns, rows);
        }
    }

    fn view(revision: u64) -> ViewModel {
        ViewModel {
            frame_revision: revision,
            snapshot_revision: revision,
            generated_boot_ns: 0,
            header: HeaderView {
                host_name: "test".into(),
                kernel: None,
                uptime_s: None,
                version: "0".into(),
                namespace_label: None,
                overload: OverloadState::Normal,
                configured_interval_ms: 1000,
                effective_interval_ms: 1000,
                hosts_healthy: 0,
                hosts_total: 0,
                read_only: false,
                theme: Theme::Dark,
                total_power: MetricCell::missing(CanonicalUnit::Watts, CellState::Unavailable),
            },
            session: SessionState {
                session_id: SessionId::new("s").unwrap(),
                view: fox_services_top_core::commands::ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: String::new(),
                sort: fox_services_top_core::commands::SortKey::GpuMemory,
                ascending: false,
                overview_bar: BarSize::OneMinute,
                process_bar: BarSize::Auto,
            },
            modules: ModuleViews::default(),
            banners: Vec::new(),
            enabled_modules: Vec::new(),
            permit: None,
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            // Short directory: macOS unix sockets are limited to 104 bytes
            // (`sun_path`), so the label and a short unique suffix must
            // leave room for the socket file name.
            let dir = std::env::temp_dir().join(format!(
                "fst-a-{}-{label}-{}",
                std::process::id(),
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Polls `check` until it is true or `timeout` passes.
    fn wait_for(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        check()
    }

    fn start(session: &FakeSession, socket: &Path) -> ServerGuard {
        let stop = Arc::new(AtomicBool::new(false));
        let join = serve_in_background(
            session.clone(),
            socket,
            Arc::new(MemoryLogger::default()),
            Arc::clone(&stop),
        )
        .unwrap();
        ServerGuard {
            stop,
            join: Some(join),
        }
    }

    /// Stops the server thread when the test ends, so it does not outlive the
    /// test's socket.
    struct ServerGuard {
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    }

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    #[test]
    fn a_projection_and_a_command_cross_the_socket() {
        let scratch = Scratch::new("roundtrip");
        let socket = scratch.0.join("attach.sock");
        let session = FakeSession::new("sess-1");
        let _server = start(&session, &socket);

        let client = match connect(&socket).unwrap() {
            Attach::Attached(client) => client,
            _ => panic!("expected an attachment"),
        };
        assert_eq!(client.session_id().as_str(), "sess-1");

        // A projection published on the collector reaches the viewer.
        session.publish(7);
        let mut received = None;
        assert!(
            wait_for(
                || {
                    received = client.take_projection();
                    received.is_some()
                },
                Duration::from_secs(3)
            ),
            "no projection arrived"
        );
        assert_eq!(received.unwrap().frame_revision, 7);

        // A command submitted by the viewer reaches the collector.
        let command = CommandEnvelope::new(
            SessionId::new("sess-1").unwrap(),
            CommandBody::Shutdown {
                reason: "test".to_owned(),
            },
        );
        client.submit(command).unwrap();
        assert!(
            wait_for(|| session.submitted() == 1, Duration::from_secs(3)),
            "the command did not reach the collector"
        );

        // The viewer's viewport is forwarded too.
        client.set_viewport(120, 40);
        {
            let session = session.clone();
            assert!(wait_for(
                || *session.shared.viewport.lock().unwrap() == (120, 40),
                Duration::from_secs(3)
            ));
        }
    }

    #[test]
    fn a_viewer_quit_detaches_without_stopping_the_collector() {
        let scratch = Scratch::new("detach");
        let socket = scratch.0.join("attach.sock");
        let session = FakeSession::new("sess-2");
        let _server = start(&session, &socket);

        let client = match connect(&socket).unwrap() {
            Attach::Attached(client) => client,
            _ => panic!("expected an attachment"),
        };
        // What the terminal UI does on `q`: a local shutdown request.
        client.request_shutdown("user");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !session.shutdown_requested(),
            "a viewer quit must not stop the collector"
        );
    }

    #[test]
    fn the_collectors_shutdown_reaches_the_viewer() {
        let scratch = Scratch::new("shutdown");
        let socket = scratch.0.join("attach.sock");
        let session = FakeSession::new("sess-3");
        let _server = start(&session, &socket);

        let client = match connect(&socket).unwrap() {
            Attach::Attached(client) => client,
            _ => panic!("expected an attachment"),
        };
        assert!(!client.shutdown_requested());
        session.request_stop();
        assert!(
            wait_for(|| client.shutdown_requested(), Duration::from_secs(3)),
            "the viewer never learned the collector was stopping"
        );
    }

    #[test]
    fn one_viewer_at_a_time_and_a_second_reuses_the_socket() {
        let scratch = Scratch::new("reuse");
        let socket = scratch.0.join("attach.sock");
        let session = FakeSession::new("sess-4");
        let _server = start(&session, &socket);

        let first = match connect(&socket).unwrap() {
            Attach::Attached(client) => client,
            _ => panic!("expected an attachment"),
        };
        drop(first);
        // The server returns to accept and serves a new viewer.
        assert!(
            wait_for(
                || matches!(connect(&socket).unwrap(), Attach::Attached(_)),
                Duration::from_secs(3)
            ),
            "a second viewer never attached"
        );
    }

    #[test]
    fn a_version_mismatch_is_refused_before_serving() {
        let scratch = Scratch::new("version");
        let socket = scratch.0.join("attach.sock");
        let session = FakeSession::new("sess-5");
        let _server = start(&session, &socket);

        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = FrameReader::new(stream.try_clone().unwrap());
        write_message(
            &mut &stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION + 99,
            },
        )
        .unwrap();
        let reply: ServerMessage = reader.next_message().unwrap().unwrap();
        assert!(matches!(
            reply,
            ServerMessage::Incompatible { version } if version == PROTOCOL_VERSION
        ));
    }
}
