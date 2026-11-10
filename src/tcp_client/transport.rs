//! Framing and connection establishment only. Request IDs/pending live in Worker.
use super::{
    FrameDecoder, FrameError, MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES, ParseError,
    StopReason,
};
use std::{
    fmt,
    future::Future,
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Error, HandshakeError, Message, WebSocket,
    client::IntoClientRequest,
    http::{HeaderMap, HeaderName, HeaderValue, Uri},
    protocol::WebSocketConfig,
};

pub const SUBPROTOCOL: &str = "lab-server.application.v1";
const UPGRADE_LIMIT: usize = 16 * 1024;
const IO_CALLS: usize = 64;
const WAIT: Duration = Duration::from_millis(2);
const WS_WRITE_LIMIT: usize = MAX_REQUEST_FRAME_BYTES + 1024;

/// Never formats the URI or headers: they can contain credentials.
/// Plain WS is restricted to resolved loopback addresses and a loopback TCP peer.
#[derive(Clone)]
pub struct Endpoint {
    address: Address,
    ws: WebSocketOptions,
}
#[derive(Clone)]
enum Address {
    Tcp(SocketAddr),
    Ws(Uri),
}
impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Endpoint([redacted])")
    }
}
impl From<SocketAddr> for Endpoint {
    fn from(address: SocketAddr) -> Self {
        Self {
            address: Address::Tcp(address),
            ws: WebSocketOptions::default(),
        }
    }
}
impl std::str::FromStr for Endpoint {
    type Err = io::Error;
    fn from_str(text: &str) -> io::Result<Self> {
        if text.len() > 4096 {
            return Err(invalid());
        }
        if let Some(text) = text.strip_prefix("tcp://") {
            return text
                .parse::<SocketAddr>()
                .map(Self::from)
                .map_err(|_| invalid());
        }
        if let Ok(address) = text.parse::<SocketAddr>() {
            return Ok(address.into());
        }
        let uri: Uri = text.parse().map_err(|_| invalid())?;
        if !matches!(uri.scheme_str(), Some("ws" | "wss"))
            || uri.host().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || text.contains('#')
        {
            return Err(invalid());
        }
        Ok(Self {
            address: Address::Ws(uri),
            ws: WebSocketOptions::default(),
        })
    }
}
impl Endpoint {
    pub fn with_websocket_options(mut self, options: WebSocketOptions) -> Self {
        self.ws = options;
        self
    }
    /// Opt-in environment configuration; called by Steel's connect adapter.
    /// Only WS endpoints read these variables. No values enter error messages.
    pub fn with_environment(mut self) -> io::Result<Self> {
        if matches!(self.address, Address::Ws(_)) {
            self.ws = WebSocketOptions::from_environment()?;
        }
        Ok(self)
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Invalid endpoint or WebSocket configuration",
    )
}

/// Handshake headers are secret by default, including in Debug output.
#[derive(Clone, Default)]
pub struct WebSocketOptions {
    headers: HeaderMap,
    roots: Vec<rustls::pki_types::CertificateDer<'static>>,
}
impl fmt::Debug for WebSocketOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebSocketOptions([redacted])")
    }
}
impl WebSocketOptions {
    /// Sets Origin or an application/authentication header. Protocol-controlled
    /// headers cannot be overridden. Total custom header storage is <= 8 KiB.
    pub fn header(mut self, name: &str, value: &str) -> io::Result<Self> {
        if name.len() > 256 || value.len() > 8192 {
            return Err(invalid());
        }
        let name: HeaderName = name.parse().map_err(|_| invalid())?;
        if matches!(
            name.as_str(),
            "host" | "connection" | "upgrade" | "content-length" | "transfer-encoding"
        ) || name.as_str().starts_with("sec-websocket-")
        {
            return Err(invalid());
        }
        let mut value: HeaderValue = value.parse().map_err(|_| invalid())?;
        value.set_sensitive(true);
        self.headers.insert(name, value);
        if self.headers.len() > 32
            || self
                .headers
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 4)
                .sum::<usize>()
                > 8192
        {
            return Err(invalid());
        }
        Ok(self)
    }
    /// Adds a trust anchor, retaining certificate chain and hostname validation.
    /// No custom verifier or insecure connector is exposed.
    pub fn trust_certificate(mut self, der: Vec<u8>) -> io::Result<Self> {
        if der.len() > 16 * 1024 || self.roots.len() >= 16 {
            return Err(invalid());
        }
        let cert = rustls::pki_types::CertificateDer::from(der);
        rustls::RootCertStore::empty()
            .add(cert.clone())
            .map_err(|_| invalid())?;
        self.roots.push(cert);
        Ok(self)
    }
    pub fn from_environment() -> io::Result<Self> {
        let mut options = Self::default();
        match std::env::var("LAB_PLOT_WS_HEADERS") {
            Ok(text) => {
                if text.len() > 16 * 1024 {
                    return Err(invalid());
                }
                let headers: std::collections::BTreeMap<String, String> =
                    serde_json::from_str(&text).map_err(|_| invalid())?;
                for (name, value) in headers {
                    options = options.header(&name, &value)?;
                }
            }
            Err(std::env::VarError::NotPresent) => (),
            Err(_) => return Err(invalid()),
        }
        match std::env::var("LAB_PLOT_WS_ORIGIN") {
            Ok(origin) => options = options.header("origin", &origin)?,
            Err(std::env::VarError::NotPresent) => (),
            Err(_) => return Err(invalid()),
        }
        Ok(options)
    }
}

/// A write can be accepted into the WS library without reaching the wire.
/// `attempted` is conservative; complete means framing ownership transferred.
pub(super) struct Written {
    pub offset: usize,
    pub complete: bool,
    pub attempted: bool,
}
#[derive(Debug)]
pub(super) struct WriteFailure {
    pub reason: StopReason,
    pub attempted: bool,
}
pub(super) trait SendFrame {
    fn send_frame(&mut self, frame: &str, offset: usize) -> Result<Written, WriteFailure>;
}
// Also used by deterministic partial-write/failure tests of the common engine.
impl<W: Write> SendFrame for W {
    fn send_frame(&mut self, frame: &str, offset: usize) -> Result<Written, WriteFailure> {
        match self.write(&frame.as_bytes()[offset..]) {
            Ok(0) => Err(WriteFailure {
                reason: StopReason::Write(io::ErrorKind::WriteZero),
                attempted: true,
            }),
            Ok(n) => Ok(Written {
                offset: offset + n,
                complete: offset + n == frame.len(),
                attempted: true,
            }),
            Err(e) if blocked_io(&e) => Ok(Written {
                offset,
                complete: false,
                attempted: false,
            }),
            Err(e) => Err(WriteFailure {
                reason: StopReason::Write(e.kind()),
                attempted: true,
            }),
        }
    }
}

pub(super) enum Transport {
    Tcp {
        socket: TcpStream,
        decoder: FrameDecoder,
    },
    Ws(Box<WebSocket<BoundedIo<Stream>>>),
}
pub(super) struct Received {
    pub frames: Vec<String>,
    pub failure: Option<StopReason>,
}
impl Transport {
    pub fn connect(
        endpoint: Endpoint,
        timeout: Duration,
        stopped: &impl Fn() -> bool,
    ) -> Result<Self, StopReason> {
        let deadline = Instant::now() + timeout;
        let addresses = match &endpoint.address {
            Address::Tcp(address) => vec![*address],
            Address::Ws(uri) => {
                let host = uri
                    .host()
                    .ok_or(StopReason::WebSocketHandshake)?
                    .trim_matches(['[', ']']);
                let port = uri
                    .port_u16()
                    .unwrap_or(if uri.scheme_str() == Some("wss") {
                        443
                    } else {
                        80
                    });
                resolve(host, port, deadline, stopped)?
            }
        };
        Self::connect_resolved(endpoint, addresses, deadline, stopped)
    }
    // Separate resolution from connection so tests can supply controlled DNS
    // answers without changing system DNS or contacting external servers.
    fn connect_resolved(
        endpoint: Endpoint,
        mut addresses: Vec<SocketAddr>,
        deadline: Instant,
        stopped: &impl Fn() -> bool,
    ) -> Result<Self, StopReason> {
        if let Address::Ws(uri) = &endpoint.address {
            addresses = websocket_addresses(uri, addresses)?;
        }
        let socket = connect_socket(addresses, deadline, stopped)?;
        socket
            .set_nonblocking(true)
            .map_err(|e| StopReason::Connect(e.kind()))?;
        let Address::Ws(uri) = endpoint.address else {
            return Ok(Self::Tcp {
                socket,
                decoder: FrameDecoder::default(),
            });
        };
        check(deadline, stopped)?;
        // Inspect the actual peer BEFORE constructing the HTTP request, attaching
        // custom headers or starting any handshake. Never trust "localhost" alone.
        let peer = socket
            .peer_addr()
            .map_err(|e| StopReason::Connect(e.kind()))?;
        check_websocket_peer(&uri, peer)?;
        let stream = if uri.scheme_str() == Some("wss") {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            for cert in endpoint.ws.roots {
                roots.add(cert).map_err(|_| StopReason::Tls)?;
            }
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| StopReason::Tls)?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let host = uri
                .host()
                .ok_or(StopReason::Tls)?
                .trim_matches(['[', ']'])
                .to_owned();
            let name =
                rustls::pki_types::ServerName::try_from(host).map_err(|_| StopReason::Tls)?;
            let mut connection = rustls::ClientConnection::new(Arc::new(config), name)
                .map_err(|_| StopReason::Tls)?;
            connection.set_buffer_limit(Some(WS_WRITE_LIMIT));
            let mut tls = rustls::StreamOwned::new(connection, BoundedIo::new(socket));
            while tls.conn.is_handshaking() {
                check(deadline, stopped)?;
                tls.sock.turn();
                match tls.conn.complete_io(&mut tls.sock) {
                    Ok(_) => (),
                    Err(e) if blocked_io(&e) => thread::park_timeout(WAIT),
                    Err(_) => return Err(StopReason::Tls),
                }
            }
            Stream::Tls(Box::new(tls))
        } else {
            Stream::Plain(BoundedIo::new(socket))
        };
        let mut request = uri
            .into_client_request()
            .map_err(|_| StopReason::WebSocketHandshake)?;
        request.headers_mut().extend(endpoint.ws.headers);
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        let mut io = BoundedIo::new(stream);
        io.upgrade_left = Some(UPGRADE_LIMIT);
        let config = WebSocketConfig::default()
            .read_buffer_size(4096)
            .write_buffer_size(0)
            .max_write_buffer_size(WS_WRITE_LIMIT)
            .max_message_size(Some(MAX_RESPONSE_FRAME_BYTES))
            .max_frame_size(Some(MAX_RESPONSE_FRAME_BYTES));
        let mut attempt = tungstenite::client::client_with_config(request, io, Some(config));
        loop {
            check(deadline, stopped)?;
            match attempt {
                Ok((mut socket, response)) => {
                    let mut protocols = response.headers().get_all("sec-websocket-protocol").iter();
                    if protocols.next().and_then(|v| v.to_str().ok()) != Some(SUBPROTOCOL)
                        || protocols.next().is_some()
                    {
                        return Err(StopReason::WebSocketSubprotocol);
                    }
                    socket.get_mut().upgrade_left = None;
                    return Ok(Self::Ws(Box::new(socket)));
                }
                Err(HandshakeError::Failure(_)) => return Err(StopReason::WebSocketHandshake),
                Err(HandshakeError::Interrupted(mut handshake)) => {
                    thread::park_timeout(WAIT);
                    let io = handshake.get_mut().get_mut();
                    io.turn();
                    io.inner.turn();
                    attempt = handshake.handshake();
                }
            }
        }
    }
    pub fn turn(&mut self) {
        if let Self::Ws(socket) = self {
            socket.get_mut().turn();
            socket.get_mut().inner.turn();
        }
    }
    pub fn receive(&mut self) -> Received {
        match self {
            Self::Tcp { socket, decoder } => {
                let mut buffer = [0; 8192];
                match socket.read(&mut buffer) {
                    Ok(0) => Received {
                        frames: vec![],
                        failure: Some(
                            decoder
                                .finish()
                                .err()
                                .map(StopReason::Frame)
                                .unwrap_or(StopReason::Eof),
                        ),
                    },
                    Ok(n) => match decoder.push(&buffer[..n]) {
                        Ok(frames) => Received {
                            frames,
                            failure: None,
                        },
                        Err(e) => Received {
                            frames: e.frames,
                            failure: Some(StopReason::Frame(e.error)),
                        },
                    },
                    Err(e) if blocked_io(&e) => Received {
                        frames: vec![],
                        failure: None,
                    },
                    Err(e) => Received {
                        frames: vec![],
                        failure: Some(StopReason::Read(e.kind())),
                    },
                }
            }
            Self::Ws(socket) => {
                // Flush automatic Pong and any single retained application frame.
                if let Err(e) = socket.flush()
                    && !blocked_ws(&e)
                {
                    return Received {
                        frames: vec![],
                        failure: Some(ws_error(e)),
                    };
                }
                let result = match socket.read() {
                    Ok(Message::Text(text)) => {
                        return Received {
                            frames: vec![text.to_string()],
                            failure: None,
                        };
                    }
                    Ok(Message::Ping(_) | Message::Pong(_)) => None,
                    Ok(Message::Close(_)) => {
                        let _ = socket.flush();
                        Some(StopReason::WebSocketClosed)
                    }
                    Ok(_) => Some(StopReason::WebSocketUnsupported),
                    Err(e) if blocked_ws(&e) => None,
                    Err(e) => Some(ws_error(e)),
                };
                Received {
                    frames: vec![],
                    failure: result,
                }
            }
        }
    }
    pub fn close(&mut self) {
        if matches!(self, Self::Ws(_)) {
            self::Transport::turn(self);
            if let Self::Ws(socket) = self {
                let _ = socket.close(None);
            }
        }
    }
}

fn insecure_websocket() -> StopReason {
    // Use the existing error API and a constant diagnostic: never echo a URI,
    // HTTP headers, DNS answers or server-supplied text containing credentials.
    StopReason::Protocol(ParseError(
        "Unencrypted ws:// is allowed only for loopback addresses; use wss:// for remote servers"
            .into(),
    ))
}
fn websocket_addresses(
    uri: &Uri,
    mut addresses: Vec<SocketAddr>,
) -> Result<Vec<SocketAddr>, StopReason> {
    if uri.scheme_str() == Some("ws") {
        addresses.retain(|address| address.ip().is_loopback());
        if addresses.is_empty() {
            return Err(insecure_websocket());
        }
    }
    Ok(addresses)
}
fn check_websocket_peer(uri: &Uri, peer: SocketAddr) -> Result<(), StopReason> {
    if uri.scheme_str() == Some("ws") && !peer.ip().is_loopback() {
        return Err(insecure_websocket());
    }
    Ok(())
}
impl SendFrame for Transport {
    fn send_frame(&mut self, frame: &str, offset: usize) -> Result<Written, WriteFailure> {
        match self {
            Self::Tcp { socket, .. } => socket.send_frame(frame, offset),
            Self::Ws(socket) => send_websocket(socket, frame, offset),
        }
    }
}
fn send_websocket<S: Read + Write>(
    socket: &mut WebSocket<S>,
    frame: &str,
    offset: usize,
) -> Result<Written, WriteFailure> {
    match socket.flush() {
        Ok(()) => (),
        Err(e) if blocked_ws(&e) => {
            return Ok(Written {
                offset,
                complete: false,
                attempted: false,
            });
        }
        Err(e) => {
            return Err(WriteFailure {
                reason: ws_write_error(e),
                attempted: false,
            });
        }
    }
    // encode_request remains NDJSON-compatible for TCP callers.
    // Strip only its terminator; a WS Text contains exactly one JSON.
    let text = frame.strip_suffix('\n').unwrap_or(frame);
    match socket.write(Message::Text(text.to_owned().into())) {
        Ok(()) => (),
        Err(e) if blocked_ws(&e) => (), // library owns it: NEVER write it again
        Err(Error::WriteBufferFull(_)) => {
            return Ok(Written {
                offset,
                complete: false,
                attempted: false,
            });
        }
        Err(e) => {
            return Err(WriteFailure {
                reason: ws_write_error(e),
                attempted: true,
            });
        }
    }
    Ok(Written {
        offset: frame.len(),
        complete: true,
        attempted: true,
    })
}

fn blocked_io(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn blocked_ws(e: &Error) -> bool {
    matches!(e, Error::Io(e) if blocked_io(e))
}
fn ws_write_error(e: Error) -> StopReason {
    match e {
        Error::Io(e) => StopReason::Write(e.kind()),
        e => ws_error(e),
    }
}
fn ws_error(e: Error) -> StopReason {
    match e {
        Error::Capacity(_) => StopReason::Frame(FrameError::FrameTooLong),
        Error::Io(e) => StopReason::Read(e.kind()),
        Error::ConnectionClosed | Error::AlreadyClosed => StopReason::WebSocketClosed,
        _ => StopReason::WebSocketProtocol,
    }
}
fn check(deadline: Instant, stopped: &impl Fn() -> bool) -> Result<(), StopReason> {
    if stopped() {
        Err(StopReason::Shutdown)
    } else if Instant::now() >= deadline {
        Err(StopReason::Connect(io::ErrorKind::TimedOut))
    } else {
        Ok(())
    }
}
// A current-thread runtime drives cancellable DNS / TCP only, never lookup_host
// or spawn_blocking. Dropping it drops all DNS tasks; no resolver threads survive.
fn wait_future<T>(
    future: impl Future<Output = Result<T, StopReason>>,
    deadline: Instant,
    stopped: &impl Fn() -> bool,
) -> Result<T, StopReason> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| StopReason::Connect(e.kind()))?;
    runtime.block_on(async {
        tokio::pin!(future);
        loop {
            check(deadline, stopped)?;
            tokio::select! {
                result = &mut future => return result,
                _ = tokio::time::sleep(WAIT) => (),
            }
        }
    })
}
fn resolve(
    host: &str,
    port: u16,
    deadline: Instant,
    stopped: &impl Fn() -> bool,
) -> Result<Vec<SocketAddr>, StopReason> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    wait_future(
        async {
            let resolver = hickory_resolver::TokioResolver::builder_tokio()
                .map_err(|_| StopReason::Dns)?
                .build();
            let addresses = resolver
                .lookup_ip(host)
                .await
                .map_err(|_| StopReason::Dns)?;
            Ok(addresses
                .iter()
                .take(16)
                .map(|ip| SocketAddr::new(ip, port))
                .collect())
        },
        deadline,
        stopped,
    )
}
fn connect_socket(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
    stopped: &impl Fn() -> bool,
) -> Result<TcpStream, StopReason> {
    wait_future(
        async {
            let mut reason = StopReason::Connect(io::ErrorKind::AddrNotAvailable);
            for address in addresses {
                match tokio::net::TcpStream::connect(address).await {
                    Ok(socket) => {
                        return socket.into_std().map_err(|e| StopReason::Connect(e.kind()));
                    }
                    Err(e) => reason = StopReason::Connect(e.kind()),
                }
            }
            Err(reason)
        },
        deadline,
        stopped,
    )
}

// Bound calls even under endless empty fragments / TLS records. Both TLS's raw
// socket and tungstenite's plaintext stream get a budget reset each worker turn.
pub(super) struct BoundedIo<S> {
    inner: S,
    calls: usize,
    upgrade_left: Option<usize>,
}
impl<S> BoundedIo<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            calls: IO_CALLS,
            upgrade_left: None,
        }
    }
    fn turn(&mut self) {
        self.calls = IO_CALLS;
    }
    fn allow(&mut self) -> io::Result<()> {
        if self.calls == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.calls -= 1;
        Ok(())
    }
}
impl<S: Read> Read for BoundedIo<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.allow()?;
        let limit = buf
            .len()
            .min(4096)
            .min(self.upgrade_left.unwrap_or(usize::MAX));
        if limit == 0 && !buf.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let count = self.inner.read(&mut buf[..limit])?;
        if let Some(left) = &mut self.upgrade_left {
            *left -= count;
        }
        Ok(count)
    }
}
impl<S: Write> Write for BoundedIo<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.allow()?;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.allow()?;
        self.inner.flush()
    }
}
pub(super) enum Stream {
    Plain(BoundedIo<TcpStream>),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, BoundedIo<TcpStream>>>),
}
impl Stream {
    fn turn(&mut self) {
        match self {
            Self::Plain(s) => s.turn(),
            Self::Tls(s) => s.sock.turn(),
        }
    }
}
impl Read for Stream {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(b),
            Self::Tls(s) => s.read(b),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(b),
            Self::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::UdpSocket,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
    };
    use tungstenite::protocol::Role;

    #[test]
    fn ws_security_filters_all_dns_answers_and_checks_actual_peer() {
        let local = [
            "127.0.0.1:80",
            "127.42.5.6:80",
            "127.255.255.255:80",
            "[::1]:80",
        ]
        .map(|s| s.parse::<SocketAddr>().unwrap());
        let remote = [
            "0.0.0.0:80",
            "192.0.2.1:80",
            "10.0.0.1:80",
            "[2001:db8::1]:80",
            "[::ffff:127.0.0.1]:80",
        ]
        .map(|s| s.parse::<SocketAddr>().unwrap());
        // Neither the spelling localhost nor a numeric URI bypasses DNS/peer checks.
        for name in ["localhost", "remote.invalid", "127.0.0.1"] {
            let uri: Uri = format!("ws://{name}/?private=uri-secret").parse().unwrap();
            let mixed = remote.iter().chain(&local).copied().collect();
            assert_eq!(websocket_addresses(&uri, mixed).unwrap(), local);
            assert_eq!(
                websocket_addresses(&uri, remote.to_vec()),
                Err(insecure_websocket())
            );
            assert_eq!(websocket_addresses(&uri, vec![]), Err(insecure_websocket()));
            for peer in local {
                assert_eq!(check_websocket_peer(&uri, peer), Ok(()));
            }
            for peer in remote {
                assert_eq!(check_websocket_peer(&uri, peer), Err(insecure_websocket()));
            }
        }
        let wss: Uri = "wss://remote.invalid/".parse().unwrap();
        assert_eq!(websocket_addresses(&wss, remote.to_vec()).unwrap(), remote);
        for peer in remote {
            assert_eq!(check_websocket_peer(&wss, peer), Ok(()));
        }
    }

    #[test]
    fn ws_security_nonlocal_localhost_dns_is_rejected_before_connect() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        for header in [None, Some("Authorization"), Some("X-Token")] {
            let mut options = WebSocketOptions::default();
            if let Some(header) = header {
                options = options.header(header, "header-secret").unwrap();
            }
            let endpoint = "ws://localhost/application/v1?private=uri-secret"
                .parse::<Endpoint>()
                .unwrap()
                .with_websocket_options(options);
            let result = Transport::connect_resolved(
                endpoint,
                vec![listener.local_addr().unwrap()],
                Instant::now() + Duration::from_secs(1),
                &|| false,
            );
            assert!(matches!(result, Err(reason) if reason == insecure_websocket()));
            // Not even a TCP connection is admitted, hence no HTTP bytes can escape.
            assert!(matches!(listener.accept(), Err(e) if e.kind()==io::ErrorKind::WouldBlock));
        }
    }

    #[test]
    fn ws_security_remote_wss_name_keeps_certificate_validation() {
        // Controlled DNS: remote.invalid resolves to our loopback TLS fixture.
        // No system DNS changes or connections to arbitrary external servers.
        for (trusted, correct_name) in [(true, true), (false, true), (true, false)] {
            let identity = rcgen::generate_simple_self_signed(vec![
                if correct_name {
                    "remote.invalid"
                } else {
                    "wrong.invalid"
                }
                .into(),
            ])
            .unwrap();
            let cert = identity.cert.der().clone();
            let key =
                rustls::pki_types::PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der());
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let task = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline);
                            thread::sleep(WAIT);
                        }
                        Err(e) => panic!("fixture accept failed: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let tls = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                    stream,
                );
                #[allow(clippy::result_large_err)]
                let callback =
                    |request: &tungstenite::handshake::server::Request,
                     mut response: tungstenite::handshake::server::Response| {
                        assert_eq!(
                            request.headers()["authorization"],
                            "Bearer controlled-secret"
                        );
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            HeaderValue::from_static(SUBPROTOCOL),
                        );
                        Ok(response)
                    };
                match tungstenite::accept_hdr(tls, callback) {
                    Ok(mut socket) => {
                        assert!(trusted && correct_name);
                        assert_eq!(
                            socket.read().unwrap().into_text().unwrap(),
                            r#"{"id":1,"op":"hello"}"#
                        );
                        socket
                            .send(Message::Text(
                                r#"{"id":1,"status":"ok","protocol":"lab-server","version":1}"#
                                    .into(),
                            ))
                            .unwrap();
                        let _ = socket.read();
                    }
                    Err(_) => assert!(!trusted || !correct_name),
                }
                assert!(matches!(listener.accept(), Err(e) if e.kind()==io::ErrorKind::WouldBlock));
            });
            let mut options = WebSocketOptions::default()
                .header("Authorization", "Bearer controlled-secret")
                .unwrap();
            if trusted {
                options = options.trust_certificate(cert.to_vec()).unwrap();
            }
            let endpoint = "wss://remote.invalid/application/v1"
                .parse::<Endpoint>()
                .unwrap()
                .with_websocket_options(options);
            let deadline = Instant::now() + Duration::from_secs(3);
            let result = Transport::connect_resolved(endpoint, vec![address], deadline, &|| false);
            if trusted && correct_name {
                let mut transport =
                    result.unwrap_or_else(|reason| panic!("WSS failed: {reason:?}"));
                loop {
                    assert!(Instant::now() < deadline);
                    transport.turn();
                    if transport
                        .send_frame("{\"id\":1,\"op\":\"hello\"}\n", 0)
                        .unwrap()
                        .complete
                    {
                        break;
                    }
                    thread::sleep(WAIT);
                }
                loop {
                    assert!(Instant::now() < deadline);
                    transport.turn();
                    let received = transport.receive();
                    assert!(received.failure.is_none());
                    if !received.frames.is_empty() {
                        assert!(matches!(
                            super::super::parse_message(&received.frames[0]).unwrap(),
                            super::super::Incoming::Response(super::super::Response {
                                data: super::super::ResponseData::Hello { version: 1, .. },
                                ..
                            })
                        ));
                        break;
                    }
                    thread::sleep(WAIT);
                }
                transport.close();
            } else {
                assert!(matches!(result, Err(StopReason::Tls)));
            }
            task.join().unwrap();
        }
    }

    #[test]
    fn dns_blackhole_is_cancellable_and_has_total_deadline() {
        for cancel in [true, false] {
            let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
            dns.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let address = dns.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let (tx, rx) = mpsc::channel();
            let task = thread::spawn(move || {
                let result = wait_future(
                    async {
                        use hickory_resolver::{
                            Resolver,
                            config::{NameServerConfig, ResolverConfig, ResolverOpts},
                            name_server::TokioConnectionProvider,
                            proto::xfer::Protocol,
                        };
                        let mut config = ResolverConfig::new();
                        config.add_name_server(NameServerConfig::new(address, Protocol::Udp));
                        let mut builder = Resolver::builder_with_config(
                            config,
                            TokioConnectionProvider::default(),
                        );
                        *builder.options_mut() = ResolverOpts::default();
                        let resolver = builder.build();
                        resolver
                            .lookup_ip("blackhole.example.com.")
                            .await
                            .map_err(|_| StopReason::Dns)?;
                        Ok(())
                    },
                    Instant::now() + Duration::from_millis(250),
                    &|| worker_stop.load(Ordering::Acquire),
                );
                tx.send(result).unwrap();
            });
            let mut packet = [0; 4096];
            assert!(dns.recv_from(&mut packet).unwrap().0 > 0);
            let start = Instant::now();
            if cancel {
                stop.store(true, Ordering::Release);
            }
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                Err(if cancel {
                    StopReason::Shutdown
                } else {
                    StopReason::Connect(io::ErrorKind::TimedOut)
                })
            );
            assert!(start.elapsed() < Duration::from_secs(1));
            task.join().unwrap();
        }
    }

    struct Backpressure {
        bytes: Vec<u8>,
        blocked: bool,
        fail: bool,
    }
    impl Read for Backpressure {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
    impl Write for Backpressure {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if self.blocked {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let count = b.len().min(3);
            self.bytes.extend_from_slice(&b[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn websocket_buffer_acceptance_never_reenqueues_or_accumulates_frames() {
        let io = Backpressure {
            bytes: vec![],
            blocked: true,
            fail: false,
        };
        let mut ws = WebSocket::from_raw_socket(
            io,
            Role::Client,
            Some(
                WebSocketConfig::default()
                    .write_buffer_size(0)
                    .max_write_buffer_size(WS_WRITE_LIMIT),
            ),
        );
        let frame = "{\"id\":2,\"op\":\"set\",\"key\":\"p\",\"value\":1}\n";
        let written = send_websocket(&mut ws, frame, 0).unwrap();
        assert!(written.complete && written.attempted);
        // A later command may NOT enter tungstenite while the prior frame waits.
        for _ in 0..1000 {
            let written = send_websocket(&mut ws, "{\"id\":3}\n", 0).unwrap();
            assert!(!written.complete && !written.attempted);
        }
        ws.get_mut().blocked = false;
        ws.flush().unwrap();
        let bytes = ws.get_ref().bytes.clone();
        let mut decoder =
            WebSocket::from_raw_socket(std::io::Cursor::new(bytes), Role::Server, None);
        assert_eq!(
            decoder.read().unwrap().into_text().unwrap(),
            frame.trim_end()
        );
        assert!(decoder.read().is_err()); // exactly one message, no retries
        ws.get_mut().fail = true;
        assert!(send_websocket(&mut ws, frame, 0).is_err());
    }

    #[test]
    fn empty_fragment_flood_yields_to_worker_stop_and_timeouts() {
        let mut bytes = vec![0x01, 0]; // nonfinal Text
        for _ in 0..200_000 {
            bytes.extend_from_slice(&[0, 0]);
        }
        let mut ws = WebSocket::from_raw_socket(
            BoundedIo::new(std::io::Cursor::new(bytes)),
            Role::Client,
            Some(WebSocketConfig::default().read_buffer_size(4096)),
        );
        assert!(matches!(ws.read(), Err(Error::Io(e)) if e.kind()==io::ErrorKind::WouldBlock));
        assert_eq!(ws.get_ref().calls, 0);
    }
}
