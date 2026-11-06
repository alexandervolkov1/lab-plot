//! GUI-owned client and bounded control bridge. Measurements never enter Steel.
use crate::{
    plots::{PlotCommand, PlotState},
    tcp_client::*,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
};

pub const CONTROL_CAPACITY: usize = 256;
pub const EVENTS_PER_FRAME: usize = 256;
pub const JOURNAL_CAPACITY: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Ready,
    Stopping,
    Stopped,
}
impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Ready => "Ready",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
        }
    }
}
enum Control {
    Plot(PlotCommand),
    Connect(SocketAddr),
    Disconnect,
}
struct Bridge {
    commands: VecDeque<Control>,
    phase: Phase,
    sender: Option<ClientSender>,
    closing: bool,
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct ControlSender {
    shared: Arc<Mutex<Bridge>>,
}
impl ControlSender {
    fn capacity(bridge: &Bridge) -> Result<(), String> {
        if bridge.closing {
            return Err("Application is closing".into());
        }
        if bridge.commands.len() == CONTROL_CAPACITY {
            return Err("GUI command queue is full; command not accepted".into());
        }
        Ok(())
    }
    pub fn plot(&self, command: PlotCommand) -> Result<bool, String> {
        let mut bridge = lock(&self.shared);
        Self::capacity(&bridge)?;
        bridge.commands.push_back(Control::Plot(command));
        Ok(true)
    }
    pub fn connect(&self, address: SocketAddr) -> Result<bool, String> {
        let mut bridge = lock(&self.shared);
        Self::capacity(&bridge)?;
        if bridge.phase != Phase::Stopped {
            return Err("Disconnect and wait for Stopped before connecting".into());
        }
        bridge.commands.push_back(Control::Connect(address));
        bridge.phase = Phase::Connecting;
        Ok(true)
    }
    pub fn disconnect(&self) -> Result<bool, String> {
        let mut bridge = lock(&self.shared);
        Self::capacity(&bridge)?;
        if matches!(bridge.phase, Phase::Stopped | Phase::Stopping) {
            return Ok(true);
        }
        bridge.commands.push_back(Control::Disconnect);
        bridge.phase = Phase::Stopping;
        bridge.sender = None;
        Ok(true)
    }
    pub fn status(&self) -> Phase {
        lock(&self.shared).phase
    }
    /// Direct admission to the TCP client's bounded outgoing queue. Does not
    /// await GUI acknowledgement or a server reply. Only short Rust locks are used.
    pub fn request(&self, request: Request) -> Result<u64, String> {
        let bridge = lock(&self.shared);
        if bridge.closing || bridge.phase != Phase::Ready {
            return Err("Connection is not Ready; request not sent".into());
        }
        bridge
            .sender
            .as_ref()
            .ok_or("Connection unavailable")?
            .request(request)
            .map_err(|e| format!("Request not accepted: {e:?}"))
    }
}

#[derive(Debug)]
pub enum JournalResult {
    Message(String),
    Response(Response),
    RequestFailed { id: u64, failure: RequestFailure },
    ServerError(ServerError),
}
#[derive(Debug)]
pub struct JournalEntry {
    pub connection: u64,
    pub result: JournalResult,
}
impl JournalEntry {
    pub fn request_id(&self) -> Option<u64> {
        match &self.result {
            JournalResult::Response(r) => Some(r.id),
            JournalResult::RequestFailed { id, .. } => Some(*id),
            _ => None,
        }
    }
    pub fn text(&self) -> String {
        let body = match &self.result {
            JournalResult::Message(s) => s.clone(),
            JournalResult::Response(r) => format!("request {}: {:?}", r.id, r.data),
            JournalResult::RequestFailed { id, failure } => {
                format!("request {id}: RequestFailed {failure:?}")
            }
            JournalResult::ServerError(e) => format!("ServerError: {} {:?}", e.error, e.key),
        };
        format!("Connection {} · {body}", self.connection)
    }
}

pub struct Integration {
    shared: Arc<Mutex<Bridge>>,
    client: Option<Client>,
    options: ClientOptions,
    terminal_received: bool,
    pub connection: u64,
    pub reason: Option<String>,
    pub stream_error: Option<String>,
    pub subscriptions: BTreeSet<String>,
    pub signal_errors: BTreeMap<String, String>,
    pub journal: VecDeque<JournalEntry>,
    pub evicted: u64,
}
impl Default for Integration {
    fn default() -> Self {
        Self::new(ClientOptions::default())
    }
}
impl Integration {
    pub fn new(options: ClientOptions) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Bridge {
                commands: VecDeque::new(),
                phase: Phase::Stopped,
                sender: None,
                closing: false,
            })),
            client: None,
            options,
            terminal_received: false,
            connection: 0,
            reason: None,
            stream_error: None,
            subscriptions: BTreeSet::new(),
            signal_errors: BTreeMap::new(),
            journal: VecDeque::new(),
            evicted: 0,
        }
    }
    pub fn sender(&self) -> ControlSender {
        ControlSender {
            shared: self.shared.clone(),
        }
    }
    pub fn phase(&self) -> Phase {
        self.sender().status()
    }
    fn record(&mut self, result: JournalResult) {
        if self.journal.len() == JOURNAL_CAPACITY {
            self.journal.pop_front();
            self.evicted = self.evicted.saturating_add(1);
        }
        self.journal.push_back(JournalEntry {
            connection: self.connection,
            result,
        });
    }
    /// Prevent new commands, then let GUI updates drain final results and reap
    /// the worker. Called only after the existing unsaved-script close decision.
    pub fn begin_close(&mut self) {
        let mut bridge = lock(&self.shared);
        bridge.closing = true;
        bridge.commands.clear();
        bridge.sender = None;
        if let Some(client) = &self.client {
            client.request_shutdown();
            bridge.phase = Phase::Stopping;
        } else {
            bridge.phase = Phase::Stopped;
        }
    }
    pub fn is_closed(&self) -> bool {
        self.client.is_none() && lock(&self.shared).closing
    }

    /// Controls are drained before observations under the same short bridge lock
    /// used for request admission. Thus a bind preceding subscribe cannot race
    /// behind its measurements. No socket I/O or wait for a live worker occurs
    /// under this lock; a finished worker can be reaped below.
    pub fn update(&mut self, plots: &mut PlotState) -> usize {
        let shared = self.shared.clone();
        let mut bridge = lock(&shared);
        while let Some(command) = bridge.commands.pop_front() {
            match command {
                Control::Plot(command) => {
                    if let Err(error) = plots.apply(command) {
                        self.record(JournalResult::Message(error));
                    }
                }
                Control::Connect(address) => {
                    self.connection += 1;
                    self.reason = None;
                    self.stream_error = None;
                    self.subscriptions.clear();
                    self.signal_errors.clear();
                    self.terminal_received = false;
                    plots.break_bound();
                    match Client::start(address, self.options.clone()) {
                        Ok(client) => {
                            bridge.sender = Some(client.sender());
                            self.client = Some(client);
                        }
                        Err(error) => {
                            let error = format!("Connection could not start: {error}");
                            self.reason = Some(error.clone());
                            bridge.phase = Phase::Stopped;
                            self.record(JournalResult::Message(error));
                        }
                    }
                }
                Control::Disconnect => {
                    if let Some(client) = &self.client {
                        client.request_shutdown();
                    }
                    plots.break_bound();
                }
            }
        }
        // Inspect out-of-band state BEFORE queued events: a damaged stream must
        // not continue extending curves while the GUI drains an old backlog.
        let state = self.client.as_ref().map(Client::state);
        if let Some(ClientState::Stopped(reason)) = &state {
            let text = format!("{reason:?}");
            self.reason = Some(text.clone());
            if *reason != StopReason::Shutdown {
                self.stream_error = Some(text);
            }
            bridge.sender = None;
            bridge.phase = Phase::Stopping; // final results/worker still being drained
            plots.break_bound();
        } else if bridge.phase == Phase::Stopping {
            plots.break_bound();
        }
        let mut count = 0;
        while count < EVENTS_PER_FRAME {
            let Some(event) = self.client.as_ref().and_then(Client::try_recv) else {
                break;
            };
            count += 1;
            match event {
                ClientEvent::Ready => {
                    if matches!(state, Some(ClientState::Ready)) && bridge.phase != Phase::Stopping
                    {
                        bridge.phase = Phase::Ready;
                    }
                    self.record(JournalResult::Message("Ready".into()));
                }
                ClientEvent::Measurement(measurement) => {
                    if bridge.phase != Phase::Ready
                        || self.stream_error.is_some()
                        || !self.subscriptions.contains(&measurement.key)
                    {
                        continue;
                    }
                    match &measurement.reading {
                        Reading::Error(error) => {
                            self.signal_errors
                                .insert(measurement.key.clone(), error.clone());
                            self.record(JournalResult::Message(format!(
                                "{} at {}: {error}",
                                measurement.key, measurement.completed_at_unix_ms
                            )));
                        }
                        Reading::Value { .. } => {
                            self.signal_errors.remove(&measurement.key);
                        }
                    }
                    for error in plots.measurement(measurement) {
                        self.record(JournalResult::Message(error));
                    }
                }
                ClientEvent::Response(response) => {
                    if let ResponseData::Subscriptions(keys) = &response.data {
                        let next: BTreeSet<_> = keys.iter().cloned().collect();
                        for removed in self.subscriptions.difference(&next) {
                            plots.break_key(removed);
                            self.signal_errors.remove(removed);
                        }
                        self.subscriptions = next;
                    }
                    self.record(JournalResult::Response(response));
                }
                ClientEvent::RequestFailed { id, failure } => {
                    self.record(JournalResult::RequestFailed { id, failure })
                }
                ClientEvent::ServerError(error) => {
                    if matches!(
                        error.error.as_str(),
                        "observation_data_lost" | "observation_stream_closed"
                    ) {
                        self.stream_error = Some(error.error.clone());
                        plots.break_bound();
                    }
                    self.record(JournalResult::ServerError(error));
                }
                ClientEvent::Stopped(reason) => {
                    self.reason = Some(format!("{reason:?}"));
                    self.record(JournalResult::Message(format!("Stopped: {reason:?}")));
                    self.terminal_received = true;
                    self.subscriptions.clear();
                    plots.break_bound();
                }
            }
        }
        if self.terminal_received && self.client.as_ref().is_some_and(Client::is_finished) {
            // is_finished guarantees Drop's join cannot wait on network work.
            self.client.take();
            bridge.sender = None;
            bridge.phase = Phase::Stopped;
        }
        count
    }
}
