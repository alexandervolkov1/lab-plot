//! One socket, one worker, bounded queues. No retries or automatic reconnects.
use super::*;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub outgoing_capacity: usize,
    pub event_capacity: usize,
    pub pending_capacity: usize,
    pub connect_timeout: Duration,
    pub hello_timeout: Duration,
    /// Deadline from admission to the pending set. Expiry closes the connection;
    /// it never implies that a sent physical command did not execute.
    pub request_timeout: Duration,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            outgoing_capacity: 64,
            event_capacity: 1024,
            pending_capacity: 64,
            connect_timeout: Duration::from_secs(3),
            hello_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(15),
        }
    }
}
impl ClientOptions {
    fn validate(&self) -> io::Result<()> {
        if !(1..=64).contains(&self.outgoing_capacity)
            || !(1..=1024).contains(&self.event_capacity)
            || !(1..=64).contains(&self.pending_capacity)
            || [
                self.connect_timeout,
                self.hello_timeout,
                self.request_timeout,
            ]
            .iter()
            .any(|d| d.is_zero() || *d > Duration::from_secs(60))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid client limits or timeout (1 ns..=60 s)",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Shutdown,
    Connect(io::ErrorKind),
    Read(io::ErrorKind),
    Write(io::ErrorKind),
    Eof,
    Frame(FrameError),
    Protocol(ParseError),
    IncompatibleProtocol,
    UnexpectedResponseId(u64),
    HelloTimeout,
    RequestTimeout(u64),
    /// At least one event could not fit. Stream completeness is lost.
    EventQueueOverflow,
    ObservationDataLost,
    ObservationStreamClosed,
    WorkerStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    Connecting,
    Ready,
    Stopping,
    Stopped(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFailure {
    /// No write of this request was attempted; execution is impossible.
    NotSent,
    /// A set may have reached the server. Do not retry automatically.
    OutcomeUnknown,
    /// A non-set request was sent but no trustworthy response was received.
    ResponseUnavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Ready,
    Response(Response),
    Measurement(Measurement),
    ServerError(ServerError),
    RequestFailed { id: u64, failure: RequestFailure },
    Stopped(StopReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    NotReady,
    Stopped,
    QueueFull,
    IdExhausted,
    Encode(EncodeError),
}

struct Command {
    id: u64,
    request: Option<Request>,
    frame: String,
}
struct Pending {
    command: Command,
    attempted: bool,
    started: Instant,
}
struct Shared {
    state: ClientState,
    next_id: Option<u64>,
    outgoing: VecDeque<Command>,
    events: VecDeque<ClientEvent>,
    // Written only during termination (plus at most one overflowing event).
    // Bound: outgoing_capacity + pending_capacity + 2, never a second live queue.
    terminal: VecDeque<ClientEvent>,
    options: ClientOptions,
}
fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Cloneable, thread-safe producer. ID allocation and queue insertion are atomic
/// under one short lock, so wire order follows ID order even across producers.
#[derive(Clone)]
pub struct ClientSender {
    shared: Arc<Mutex<Shared>>,
    worker: thread::Thread,
}
impl ClientSender {
    /// Returns an ID only after bounded queue admission. Every error means this
    /// call was not enqueued or sent. Does not wait for socket I/O or queue space.
    pub fn request(&self, request: Request) -> Result<u64, RequestError> {
        let mut shared = lock(&self.shared);
        match shared.state {
            ClientState::Ready => (),
            ClientState::Connecting => return Err(RequestError::NotReady),
            _ => return Err(RequestError::Stopped),
        }
        if shared.outgoing.len() == shared.options.outgoing_capacity {
            return Err(RequestError::QueueFull);
        }
        let id = shared.next_id.ok_or(RequestError::IdExhausted)?;
        let frame = encode_request(id, &request).map_err(RequestError::Encode)?;
        shared.outgoing.push_back(Command {
            id,
            request: Some(request),
            frame,
        });
        shared.next_id = id.checked_add(1);
        drop(shared);
        self.worker.unpark();
        Ok(id)
    }
}

/// Owns the worker lifetime. Drain events even after `state()` reports Stopped:
/// final request results and the Stopped event remain available exactly once.
/// EOF/Shutdown never certify complete delivery of observations.
pub struct Client {
    sender: ClientSender,
    handle: Option<JoinHandle<()>>,
}
impl Client {
    /// Starts connection establishment in the background. Socket errors arrive
    /// through state/events; only invalid options or thread creation fail here.
    /// A numeric address avoids blocking DNS resolution during shutdown.
    pub fn start(address: SocketAddr, options: ClientOptions) -> io::Result<Self> {
        options.validate()?;
        let shared = Arc::new(Mutex::new(Shared {
            state: ClientState::Connecting,
            next_id: Some(2),
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
            terminal: VecDeque::new(),
            options,
        }));
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("lab-tcp-client".into())
            .spawn(move || {
                let mut worker = Worker {
                    shared: worker_shared,
                    pending: BTreeMap::new(),
                    writing: None,
                };
                let reason = catch_unwind(AssertUnwindSafe(|| worker.run(address)))
                    .unwrap_or(StopReason::WorkerStopped);
                worker.finish(reason);
            })?;
        Ok(Self {
            sender: ClientSender {
                shared,
                worker: handle.thread().clone(),
            },
            handle: Some(handle),
        })
    }
    pub fn sender(&self) -> ClientSender {
        self.sender.clone()
    }
    /// Poll worker completion before dropping/joining from an interactive thread.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
    /// Available independently of the event queue, including on overflow.
    pub fn state(&self) -> ClientState {
        lock(&self.sender.shared).state.clone()
    }
    pub fn try_recv(&self) -> Option<ClientEvent> {
        let mut shared = lock(&self.sender.shared);
        shared
            .events
            .pop_front()
            .or_else(|| shared.terminal.pop_front())
    }
    /// Signals shutdown without waiting. Useful on a GUI thread.
    pub fn request_shutdown(&self) {
        let mut shared = lock(&self.sender.shared);
        if !matches!(shared.state, ClientState::Stopped(_)) {
            shared.state = ClientState::Stopping;
        }
        drop(shared);
        self.sender.worker.unpark();
    }
    /// Stops and joins. During connection establishment this can wait up to
    /// connect_timeout; otherwise the worker polls at most every 2 ms.
    /// Does not flush unsent commands. Safe to call more than once.
    pub fn shutdown(&mut self) {
        self.request_shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    pending: BTreeMap<u64, Pending>,
    writing: Option<(u64, usize)>,
}
impl Worker {
    fn emit(&self, event: ClientEvent) -> Result<(), StopReason> {
        let mut shared = lock(&self.shared);
        if shared.events.len() < shared.options.event_capacity {
            shared.events.push_back(event);
            Ok(())
        } else {
            // Preserve the definitive response even when normal delivery fails.
            // The following finish() accounts for every other admitted request.
            shared.terminal.push_back(event);
            shared.state = ClientState::Stopping;
            Err(StopReason::EventQueueOverflow)
        }
    }
    fn run(&mut self, address: SocketAddr) -> StopReason {
        let options = lock(&self.shared).options.clone();
        let mut socket = match TcpStream::connect_timeout(&address, options.connect_timeout) {
            Ok(socket) => socket,
            Err(e) => return StopReason::Connect(e.kind()),
        };
        if let Err(e) = socket.set_nonblocking(true) {
            return StopReason::Connect(e.kind());
        }
        self.pending.insert(
            1,
            Pending {
                command: Command {
                    id: 1,
                    request: None,
                    frame: "{\"id\":1,\"op\":\"hello\"}\n".into(),
                },
                attempted: false,
                started: Instant::now(),
            },
        );
        self.writing = Some((1, 0));
        let mut decoder = FrameDecoder::default();
        let mut buffer = [0; 8192];
        loop {
            {
                let mut shared = lock(&self.shared);
                if shared.state == ClientState::Stopping {
                    return StopReason::Shutdown;
                }
                if shared.state == ClientState::Ready
                    && self.writing.is_none()
                    && self.pending.len() < options.pending_capacity
                    && let Some(command) = shared.outgoing.pop_front()
                {
                    self.writing = Some((command.id, 0));
                    self.pending.insert(
                        command.id,
                        Pending {
                            command,
                            attempted: false,
                            started: Instant::now(),
                        },
                    );
                }
            }
            let mut progress = match self.write_pending(&mut socket) {
                Ok(progress) => progress,
                Err(reason) => return reason,
            };
            match socket.read(&mut buffer) {
                Ok(0) => {
                    return decoder
                        .finish()
                        .err()
                        .map(StopReason::Frame)
                        .unwrap_or(StopReason::Eof);
                }
                Ok(n) => {
                    progress = true;
                    let (frames, failure) = match decoder.push(&buffer[..n]) {
                        Ok(frames) => (frames, None),
                        Err(failure) => (failure.frames, Some(failure.error)),
                    };
                    for frame in frames {
                        if let Err(reason) = self.incoming(&frame) {
                            return reason;
                        }
                    }
                    if let Some(error) = failure {
                        return StopReason::Frame(error);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return StopReason::Read(e.kind()),
            }
            for (&id, pending) in &self.pending {
                let timeout = if id == 1 {
                    options.hello_timeout
                } else {
                    options.request_timeout
                };
                if pending.started.elapsed() >= timeout {
                    return if id == 1 {
                        StopReason::HelloTimeout
                    } else {
                        StopReason::RequestTimeout(id)
                    };
                }
            }
            if !progress {
                thread::park_timeout(Duration::from_millis(2));
            }
        }
    }
    // One bounded nonblocking write per iteration keeps incoming traffic fair.
    fn write_pending(&mut self, socket: &mut impl Write) -> Result<bool, StopReason> {
        let Some((id, offset)) = self.writing else {
            return Ok(false);
        };
        let pending = self
            .pending
            .get_mut(&id)
            .expect("writing request is pending");
        let previously_attempted = pending.attempted;
        // Mark before calling into I/O: an unexpected unwind must not assert
        // that a physical command definitely did not execute.
        pending.attempted = true;
        match socket.write(&pending.command.frame.as_bytes()[offset..]) {
            Ok(0) => Err(StopReason::Write(io::ErrorKind::WriteZero)),
            Ok(n) => {
                let offset = offset + n;
                self.writing = if offset == pending.command.frame.len() {
                    None
                } else {
                    Some((id, offset))
                };
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                pending.attempted = previously_attempted;
                Ok(false)
            }
            Err(e) => Err(StopReason::Write(e.kind())),
        }
    }
    fn incoming(&mut self, frame: &str) -> Result<(), StopReason> {
        let message = parse_message(frame).map_err(StopReason::Protocol)?;
        match message {
            Incoming::Response(response) => {
                let pending = self
                    .pending
                    .get(&response.id)
                    .ok_or(StopReason::UnexpectedResponseId(response.id))?;
                if !pending.attempted || self.writing.is_some_and(|(id, _)| id == response.id) {
                    return Err(StopReason::UnexpectedResponseId(response.id));
                }
                if response.id == 1 {
                    if response.data
                        != (ResponseData::Hello {
                            protocol: "lab-server".into(),
                            version: 1,
                        })
                    {
                        return Err(StopReason::IncompatibleProtocol);
                    }
                    self.pending.remove(&1);
                    let mut shared = lock(&self.shared);
                    if shared.state == ClientState::Stopping {
                        return Err(StopReason::Shutdown);
                    }
                    shared.state = ClientState::Ready;
                    drop(shared);
                    self.emit(ClientEvent::Ready)
                } else {
                    if !matches_request(
                        pending
                            .command
                            .request
                            .as_ref()
                            .expect("application request"),
                        &response.data,
                    ) {
                        return Err(StopReason::Protocol(ParseError(
                            "response does not match request".into(),
                        )));
                    }
                    self.pending.remove(&response.id);
                    self.emit(ClientEvent::Response(response))
                }
            }
            Incoming::Measurement(measurement) => {
                if self.pending.contains_key(&1) {
                    return Err(StopReason::IncompatibleProtocol);
                }
                self.emit(ClientEvent::Measurement(measurement))
            }
            Incoming::ServerError(error) => {
                let reason = match error.error.as_str() {
                    "observation_data_lost" => Some(StopReason::ObservationDataLost),
                    "observation_stream_closed" => Some(StopReason::ObservationStreamClosed),
                    _ => None,
                };
                let delivered = self.emit(ClientEvent::ServerError(error));
                if let Some(reason) = reason {
                    Err(reason)
                } else {
                    delivered
                }
            }
        }
    }
    fn finish(&mut self, reason: StopReason) {
        let mut shared = lock(&self.shared);
        for (_, pending) in std::mem::take(&mut self.pending) {
            if pending.command.id == 1 {
                continue;
            }
            let failure = if !pending.attempted {
                RequestFailure::NotSent
            } else if matches!(pending.command.request, Some(Request::Set { .. })) {
                RequestFailure::OutcomeUnknown
            } else {
                RequestFailure::ResponseUnavailable
            };
            shared.terminal.push_back(ClientEvent::RequestFailed {
                id: pending.command.id,
                failure,
            });
        }
        while let Some(command) = shared.outgoing.pop_front() {
            shared.terminal.push_back(ClientEvent::RequestFailed {
                id: command.id,
                failure: RequestFailure::NotSent,
            });
        }
        shared.state = ClientState::Stopped(reason.clone());
        shared.terminal.push_back(ClientEvent::Stopped(reason));
    }
}

fn matches_request(request: &Request, response: &ResponseData) -> bool {
    if let ResponseData::Error(error) = response {
        return match (&error.key, request) {
            (Some(actual), Request::Get { key } | Request::Set { key, .. }) => actual == key,
            _ => true,
        };
    }
    match (request, response) {
        (Request::Ping | Request::Set { .. }, ResponseData::Ack)
        | (Request::Describe, ResponseData::Parameters(_))
        | (
            Request::Subscribe { .. } | Request::Unsubscribe { .. },
            ResponseData::Subscriptions(_),
        ) => true,
        (Request::Get { key }, ResponseData::NoData { key: actual }) => key == actual,
        (Request::Get { key }, ResponseData::Measurement(measurement)) => key == &measurement.key,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> Worker {
        let request = Request::Set {
            key: "p".into(),
            value: Number::Integer(1),
        };
        Worker {
            shared: Arc::new(Mutex::new(Shared {
                state: ClientState::Ready,
                next_id: Some(3),
                outgoing: VecDeque::new(),
                events: VecDeque::new(),
                terminal: VecDeque::new(),
                options: ClientOptions::default(),
            })),
            pending: BTreeMap::from([(
                2,
                Pending {
                    command: Command {
                        id: 2,
                        frame: encode_request(2, &request).unwrap(),
                        request: Some(request),
                    },
                    attempted: false,
                    started: Instant::now(),
                },
            )]),
            writing: Some((2, 0)),
        }
    }
    struct Writer {
        result: io::Result<usize>,
        bytes: Vec<u8>,
        panic: bool,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(!self.panic, "simulated worker unwind");
            match &self.result {
                Ok(n) => {
                    let n = (*n).min(bytes.len());
                    self.bytes.extend_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Err(e) => Err(io::Error::from(e.kind())),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn partial_writes_and_backpressure_preserve_frame_and_uncertainty() {
        let mut worker = worker();
        let expected = worker.pending[&2].command.frame.clone();
        let mut writer = Writer {
            result: Err(io::ErrorKind::WouldBlock.into()),
            bytes: vec![],
            panic: false,
        };
        assert_eq!(worker.write_pending(&mut writer), Ok(false));
        assert!(!worker.pending[&2].attempted);
        writer.result = Ok(3);
        assert_eq!(worker.write_pending(&mut writer), Ok(true));
        assert_eq!(worker.writing, Some((2, 3)));
        writer.result = Err(io::ErrorKind::Interrupted.into());
        assert_eq!(worker.write_pending(&mut writer), Ok(false));
        assert!(worker.pending[&2].attempted);
        writer.result = Ok(3);
        while worker.writing.is_some() {
            worker.write_pending(&mut writer).unwrap();
        }
        assert_eq!(writer.bytes, expected.as_bytes());
        worker.finish(StopReason::Eof);
        assert_eq!(
            lock(&worker.shared).terminal[0],
            ClientEvent::RequestFailed {
                id: 2,
                failure: RequestFailure::OutcomeUnknown
            }
        );
    }
    #[test]
    fn write_errors_and_worker_unwind_keep_set_outcome_unknown() {
        for (result, panic, expected) in [
            (Ok(0), false, StopReason::Write(io::ErrorKind::WriteZero)),
            (
                Err(io::ErrorKind::BrokenPipe.into()),
                false,
                StopReason::Write(io::ErrorKind::BrokenPipe),
            ),
            (Ok(1), true, StopReason::WorkerStopped),
        ] {
            let mut worker = worker();
            let mut writer = Writer {
                result,
                bytes: vec![],
                panic,
            };
            let reason = catch_unwind(AssertUnwindSafe(|| worker.write_pending(&mut writer)))
                .unwrap_or(Err(StopReason::WorkerStopped))
                .unwrap_err();
            assert_eq!(reason, expected);
            worker.finish(reason);
            assert_eq!(
                lock(&worker.shared).terminal[0],
                ClientEvent::RequestFailed {
                    id: 2,
                    failure: RequestFailure::OutcomeUnknown
                }
            );
        }
    }
    #[test]
    fn id_exhaustion_and_encode_rejection_never_enqueue_or_wrap() {
        let worker = worker();
        lock(&worker.shared).next_id = Some(u64::MAX);
        let sender = ClientSender {
            shared: worker.shared.clone(),
            worker: thread::current(),
        };
        assert_eq!(
            sender.request(Request::Set {
                key: "p".into(),
                value: Number::Float(f64::NAN)
            }),
            Err(RequestError::Encode(EncodeError::NonFiniteFloat))
        );
        assert_eq!(sender.request(Request::Ping), Ok(u64::MAX));
        assert_eq!(
            sender.request(Request::Ping),
            Err(RequestError::IdExhausted)
        );
        assert_eq!(lock(&worker.shared).outgoing.len(), 1);
    }
}
