use lab_plot::tcp_client::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    handshake::server::{Request as Upgrade, Response as UpgradeResponse},
    protocol::frame::{
        Frame,
        coding::{Data, OpCode},
    },
};
#[allow(dead_code)]
mod support;
use support::{WAIT, mock};
const PROTOCOL: &str = "lab-server.application.v1";

#[allow(clippy::result_large_err)]
fn upgrade(
    request: &Upgrade,
    mut response: UpgradeResponse,
) -> Result<UpgradeResponse, tungstenite::handshake::server::ErrorResponse> {
    assert_eq!(request.uri().path(), "/application/v1");
    assert_eq!(request.headers()["sec-websocket-protocol"], PROTOCOL);
    response
        .headers_mut()
        .insert("sec-websocket-protocol", PROTOCOL.parse().unwrap());
    Ok(response)
}
fn server(
    action: impl FnOnce(WebSocket<TcpStream>) + Send + 'static,
) -> (Endpoint, thread::JoinHandle<()>) {
    let (address, task) =
        mock(move |socket| action(tungstenite::accept_hdr(socket.into_inner(), upgrade).unwrap()));
    (
        format!("ws://{address}/application/v1").parse().unwrap(),
        task,
    )
}
fn send<S: Read + Write>(ws: &mut WebSocket<S>, value: Value) {
    ws.send(Message::Text(value.to_string().into())).unwrap();
}
fn read<S: Read + Write>(ws: &mut WebSocket<S>) -> Value {
    let Message::Text(text) = ws.read().unwrap() else {
        panic!("expected Text")
    };
    assert!(!text.ends_with('\n'), "WS must not use NDJSON");
    serde_json::from_str(&text).unwrap()
}
fn hello<S: Read + Write>(ws: &mut WebSocket<S>) {
    assert_eq!(read(ws), json!({"id":1,"op":"hello"}));
    send(
        ws,
        json!({"id":1,"status":"ok","protocol":"lab-server","version":1}),
    );
}
fn event(client: &Client) -> ClientEvent {
    let start = Instant::now();
    loop {
        if let Some(event) = client.try_recv() {
            return event;
        }
        assert!(
            start.elapsed() < WAIT,
            "event timeout: {:?}",
            client.state()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn stopped(client: &Client) -> (Vec<ClientEvent>, StopReason) {
    let mut events = vec![];
    loop {
        match event(client) {
            ClientEvent::Stopped(reason) => {
                assert!(client.try_recv().is_none());
                return (events, reason);
            }
            e => events.push(e),
        }
    }
}
fn ready(endpoint: Endpoint, options: ClientOptions) -> Client {
    let client = Client::start_endpoint(endpoint, options).unwrap();
    assert_eq!(event(&client), ClientEvent::Ready);
    client
}

#[test]
fn ws_security_ipv4_ipv6_and_localhost_reach_ready() {
    for (bind, host) in [
        ("127.0.0.1:0", "127.0.0.1"),
        ("127.42.0.7:0", "127.42.0.7"),
        ("[::1]:0", "[::1]"),
        ("127.0.0.1:0", "localhost"),
    ] {
        let listener = std::net::TcpListener::bind(bind).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = thread::spawn(move || {
            let start = Instant::now();
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < WAIT);
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(WAIT)).unwrap();
            socket.set_write_timeout(Some(WAIT)).unwrap();
            let mut ws = tungstenite::accept_hdr(socket, upgrade).unwrap();
            hello(&mut ws);
            assert!(matches!(ws.read(), Ok(Message::Close(_))));
        });
        let client = ready(
            format!("ws://{host}:{port}/application/v1")
                .parse()
                .unwrap(),
            ClientOptions::default(),
        );
        client.request_shutdown();
        assert_eq!(stopped(&client).1, StopReason::Shutdown);
        task.join().unwrap();
    }
}

fn assert_loopback_refusal(reason: &StopReason) {
    let text = format!("{reason:?}");
    assert!(text.contains("loopback") && text.contains("wss://"));
    for secret in ["uri-secret", "header-secret", "Authorization", "X-Token"] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn ws_security_nonloopback_refusal_sends_no_headers_and_stops_gui_client() {
    use lab_plot::{
        integration::{Integration, Phase},
        plots::{PlotCommand, PlotState},
    };
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    for header in [None, Some("Authorization"), Some("X-Token")] {
        let mut options = WebSocketOptions::default();
        if let Some(header) = header {
            options = options.header(header, "header-secret").unwrap();
        }
        let endpoint = format!(
            "ws://{}/application/v1?private=uri-secret",
            listener.local_addr().unwrap()
        )
        .parse::<Endpoint>()
        .unwrap()
        .with_websocket_options(options);
        let client = Client::start_endpoint(endpoint.clone(), ClientOptions::default()).unwrap();
        let (events, reason) = stopped(&client);
        assert!(events.is_empty());
        assert_loopback_refusal(&reason);
        assert_eq!(client.state(), ClientState::Stopped(reason));
        assert!(client.sender().request(Request::Ping).is_err());
        drop(client);
        let mut integration = Integration::default();
        let mut plots = PlotState::default();
        let sender = integration.sender();
        assert!(sender.connect_endpoint(endpoint).unwrap());
        assert_eq!(sender.status(), Phase::Connecting);
        let start = Instant::now();
        loop {
            // GUI work must still be admitted and applied while connection fails.
            sender
                .plot(PlotCommand::Point {
                    name: "manual".into(),
                    time: 1.0,
                    value: 2.0,
                })
                .unwrap();
            let frame = Instant::now();
            assert!(integration.update(&mut plots) <= 256);
            assert!(frame.elapsed() < Duration::from_secs(1));
            if integration.phase() == Phase::Stopped {
                break;
            }
            assert!(start.elapsed() < WAIT);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!plots.series["manual"].points.is_empty());
        assert!(integration.reason.as_ref().unwrap().contains("use wss://"));
        for entry in &integration.journal {
            let text = entry.text();
            assert!(!text.contains("uri-secret") && !text.contains("header-secret"));
        }
        integration.begin_close();
        assert!(integration.is_closed());
        // The fixture is reachable locally, but its unspecified (non-loopback)
        // destination must be refused before even connecting, regardless of headers.
        assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    }
}

#[test]
fn ws_security_environment_headers_are_not_sent() {
    // Isolate environment changes in a child process; no unsafe set_var or races
    // with parallel tests / the user's real environment.
    if std::env::var_os("LAB_PLOT_SECURITY_TEST_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ws_security_environment_headers_are_not_sent"])
            .env("LAB_PLOT_SECURITY_TEST_CHILD", "1")
            .env(
                "LAB_PLOT_WS_HEADERS",
                r#"{"Authorization":"header-secret","X-Token":"header-secret"}"#,
            )
            .env("LAB_PLOT_WS_ORIGIN", "http://localhost")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if start.elapsed() > WAIT {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child test timed out");
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "ws://{}/application/v1?private=uri-secret",
        listener.local_addr().unwrap()
    )
    .parse::<Endpoint>()
    .unwrap()
    .with_environment()
    .unwrap();
    let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
    assert_loopback_refusal(&stopped(&client).1);
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn websocket_headers_hello_scalars_out_of_order_and_independent_events() {
    let (done_tx, done_rx) = mpsc::channel();
    let (address, task) = mock(move |socket| {
        #[allow(clippy::result_large_err)]
        let callback = |request: &Upgrade, response: UpgradeResponse| {
            assert_eq!(request.headers()["origin"], "https://test.invalid");
            assert_eq!(request.headers()["authorization"], "Bearer test-secret");
            assert_eq!(request.headers()["x-custom"], "custom");
            upgrade(request, response)
        };
        let mut ws = tungstenite::accept_hdr(socket.into_inner(), callback).unwrap();
        hello(&mut ws);
        let values = [json!(i64::MIN), json!(i64::MAX), json!(1.5), json!(true)];
        for value in &values {
            let r = read(&mut ws);
            assert_eq!(r["value"], *value);
        }
        send(
            &mut ws,
            json!({"event":"measurement","status":"ok","key":"b","value":true,"unit":"","completed_at_unix_ms":1}),
        );
        for id in (2..6).rev() {
            send(&mut ws, json!({"id":id,"status":"ok"}));
        }
        for (i, value) in values.iter().enumerate() {
            let r = read(&mut ws);
            send(
                &mut ws,
                json!({"id":r["id"],"status":"ok","key":"v","value":value,"unit":if i==3 {""} else {"V"},"completed_at_unix_ms":2}),
            );
        }
        done_rx.recv_timeout(WAIT).unwrap();
    });
    let endpoint: Endpoint = format!("ws://{address}/application/v1").parse().unwrap();
    let options = WebSocketOptions::default()
        .header("Origin", "https://test.invalid")
        .unwrap()
        .header("Authorization", "Bearer test-secret")
        .unwrap()
        .header("x-custom", "custom")
        .unwrap();
    assert!(!format!("{options:?}").contains("test-secret"));
    let client = ready(
        endpoint.with_websocket_options(options),
        ClientOptions::default(),
    );
    let values = [
        ScalarValue::Integer(i64::MIN),
        ScalarValue::Integer(i64::MAX),
        ScalarValue::Float(1.5),
        ScalarValue::Boolean(true),
    ];
    for value in values {
        client
            .sender()
            .request(Request::Set {
                key: "v".into(),
                value,
            })
            .unwrap();
    }
    assert!(matches!(
        event(&client),
        ClientEvent::Measurement(Measurement {
            reading: Reading::Value {
                value: ScalarValue::Boolean(true),
                ..
            },
            ..
        })
    ));
    for id in (2..6).rev() {
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id,
                data: ResponseData::Ack
            })
        );
    }
    for value in values {
        client
            .sender()
            .request(Request::Get { key: "v".into() })
            .unwrap();
        assert!(
            matches!(event(&client), ClientEvent::Response(Response {data:ResponseData::Measurement(Measurement {reading:Reading::Value {value:actual, ..}, ..}), ..}) if actual==value)
        );
    }
    client.request_shutdown();
    assert_eq!(stopped(&client).1, StopReason::Shutdown);
    done_tx.send(()).unwrap();
    task.join().unwrap();
}

#[test]
fn websocket_ping_pong_fragmentation_close() {
    let (endpoint, task) = server(|mut ws| {
        assert_eq!(read(&mut ws)["op"], "hello");
        let reply = r#"{"id":1,"status":"ok","protocol":"lab-server","version":1}"#;
        ws.send(Message::Frame(Frame::message(
            reply.as_bytes()[..20].to_vec(),
            OpCode::Data(Data::Text),
            false,
        )))
        .unwrap();
        ws.send(Message::Ping(vec![1, 2, 3].into())).unwrap();
        ws.send(Message::Frame(Frame::message(
            reply.as_bytes()[20..].to_vec(),
            OpCode::Data(Data::Continue),
            true,
        )))
        .unwrap();
        assert_eq!(ws.read().unwrap(), Message::Pong(vec![1, 2, 3].into()));
        ws.close(None).unwrap();
        let _ = ws.read();
    });
    let client = ready(endpoint, ClientOptions::default());
    assert_eq!(stopped(&client).1, StopReason::WebSocketClosed);
    task.join().unwrap();
}

#[test]
fn websocket_rejects_bad_protocol_json_ids_binary_and_oversize() {
    for (message, expected) in [
        (
            Message::Text(
                json!({"id":1,"status":"ok","protocol":"other","version":1})
                    .to_string()
                    .into(),
            ),
            StopReason::IncompatibleProtocol,
        ),
        (
            Message::Text(json!({"id":99,"status":"ok"}).to_string().into()),
            StopReason::UnexpectedResponseId(99),
        ),
        (
            Message::Binary(vec![0].into()),
            StopReason::WebSocketUnsupported,
        ),
        (
            Message::Text("x".repeat(MAX_RESPONSE_FRAME_BYTES + 1).into()),
            StopReason::Frame(FrameError::FrameTooLong),
        ),
    ] {
        let (endpoint, task) = server(move |mut ws| {
            read(&mut ws);
            ws.send(message).unwrap();
            let _ = ws.read();
        });
        let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
        assert_eq!(stopped(&client).1, expected);
        task.join().unwrap();
    }
    let (endpoint, task) = server(|mut ws| {
        read(&mut ws);
        ws.send(Message::Text("{broken".into())).unwrap();
        let _ = ws.read();
    });
    let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
    assert!(matches!(stopped(&client).1, StopReason::Protocol(_)));
    task.join().unwrap();
}

#[test]
fn websocket_subprotocol_and_upgrade_are_required_and_bounded() {
    for protocol in [None, Some("wrong")] {
        let (address, task) = mock(move |socket| {
            #[allow(clippy::result_large_err)]
            let callback = move |_: &Upgrade, mut response: UpgradeResponse| {
                if let Some(protocol) = protocol {
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", protocol.parse().unwrap());
                }
                Ok(response)
            };
            let mut ws = tungstenite::accept_hdr(socket.into_inner(), callback).unwrap();
            let _ = ws.read();
        });
        let client = Client::start_endpoint(
            format!("ws://{address}/application/v1").parse().unwrap(),
            ClientOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            stopped(&client).1,
            StopReason::WebSocketHandshake | StopReason::WebSocketSubprotocol
        ));
        task.join().unwrap();
    }
    let (address, task) = mock(|mut socket| {
        let mut b = [0; 2048];
        assert!(socket.get_mut().read(&mut b).unwrap() > 0);
        let _ = socket.get_mut().write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nX-Fill: {}",
                "a".repeat(20 * 1024)
            )
            .as_bytes(),
        );
    });
    let client = Client::start_endpoint(
        format!("ws://{address}/application/v1").parse().unwrap(),
        ClientOptions::default(),
    )
    .unwrap();
    assert_eq!(stopped(&client).1, StopReason::WebSocketHandshake);
    task.join().unwrap();
}

#[test]
fn websocket_timeout_and_shutdown_during_upgrade_and_tls() {
    for scheme in ["ws", "wss"] {
        for cancel in [false, true] {
            let (seen_tx, seen_rx) = mpsc::channel();
            let (address, task) = mock(move |mut socket| {
                let mut b = [0; 4096];
                assert!(socket.get_mut().read(&mut b).unwrap() > 0);
                seen_tx.send(()).unwrap();
                // Drain handshake bytes until the client cancels/closes.
                loop {
                    match socket.get_mut().read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => (),
                    }
                }
            });
            let mut client = Client::start_endpoint(
                format!("{scheme}://{address}/application/v1")
                    .parse()
                    .unwrap(),
                ClientOptions {
                    connect_timeout: Duration::from_millis(150),
                    ..Default::default()
                },
            )
            .unwrap();
            seen_rx.recv_timeout(WAIT).unwrap();
            let start = Instant::now();
            if cancel {
                client.shutdown();
            }
            assert_eq!(
                stopped(&client).1,
                if cancel {
                    StopReason::Shutdown
                } else {
                    StopReason::Connect(std::io::ErrorKind::TimedOut)
                }
            );
            assert!(start.elapsed() < Duration::from_secs(1));
            task.join().unwrap();
        }
    }
}

#[test]
fn websocket_sent_set_unknown_unsent_queue_not_sent_no_duplicates_or_retries() {
    let (release_tx, release_rx) = mpsc::channel();
    let (endpoint, task) = server(move |mut ws| {
        hello(&mut ws);
        assert_eq!(read(&mut ws)["op"], "set");
        release_rx.recv_timeout(WAIT).unwrap();
    });
    let client = ready(
        endpoint,
        ClientOptions {
            pending_capacity: 1,
            ..Default::default()
        },
    );
    let first = client
        .sender()
        .request(Request::Set {
            key: "v".into(),
            value: ScalarValue::Integer(1),
        })
        .unwrap();
    // pending capacity 1 forces remaining requests to stay in the outgoing queue.
    let second = client
        .sender()
        .request(Request::Set {
            key: "v".into(),
            value: ScalarValue::Integer(2),
        })
        .unwrap();
    release_tx.send(()).unwrap();
    let (events, _) = stopped(&client);
    assert_eq!(
        events,
        vec![
            ClientEvent::RequestFailed {
                id: first,
                failure: RequestFailure::OutcomeUnknown
            },
            ClientEvent::RequestFailed {
                id: second,
                failure: RequestFailure::NotSent
            }
        ]
    );
    assert!(client.sender().request(Request::Ping).is_err());
    task.join().unwrap();
}

#[test]
fn websocket_slow_response_and_outgoing_limit() {
    let (endpoint, task) = server(|mut ws| {
        hello(&mut ws);
        assert_eq!(read(&mut ws)["op"], "set");
        let _ = ws.read();
    });
    let client = ready(
        endpoint,
        ClientOptions {
            request_timeout: Duration::from_millis(80),
            ..Default::default()
        },
    );
    assert_eq!(
        client.sender().request(Request::Get {
            key: "x".repeat(MAX_REQUEST_FRAME_BYTES)
        }),
        Err(RequestError::Encode(EncodeError::FrameTooLong))
    );
    assert_eq!(
        client.sender().request(Request::Set {
            key: "v".into(),
            value: ScalarValue::Integer(0)
        }),
        Ok(2)
    );
    let (events, reason) = stopped(&client);
    assert_eq!(reason, StopReason::RequestTimeout(2));
    assert_eq!(
        events,
        vec![ClientEvent::RequestFailed {
            id: 2,
            failure: RequestFailure::OutcomeUnknown
        }]
    );
    task.join().unwrap();
}

#[test]
fn websocket_event_overflow_preserves_final_ack_once() {
    let (go_tx, go_rx) = mpsc::channel();
    let (endpoint, task) = server(move |mut ws| {
        hello(&mut ws);
        let r = read(&mut ws);
        go_rx.recv_timeout(WAIT).unwrap();
        send(
            &mut ws,
            json!({"event":"measurement","status":"ok","key":"a","value":1,"unit":"V","completed_at_unix_ms":1}),
        );
        send(&mut ws, json!({"id":r["id"],"status":"ok"}));
        let _ = ws.read();
    });
    let client = ready(
        endpoint,
        ClientOptions {
            event_capacity: 1,
            ..Default::default()
        },
    );
    client
        .sender()
        .request(Request::Set {
            key: "a".into(),
            value: ScalarValue::Integer(1),
        })
        .unwrap();
    go_tx.send(()).unwrap();
    let start = Instant::now();
    while !matches!(client.state(), ClientState::Stopped(_)) {
        assert!(start.elapsed() < WAIT);
        thread::sleep(Duration::from_millis(1));
    }
    let (events, reason) = stopped(&client);
    assert_eq!(reason, StopReason::EventQueueOverflow);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1],
        ClientEvent::Response(Response {
            id: 2,
            data: ResponseData::Ack
        })
    );
    task.join().unwrap();
}

#[test]
fn wss_validates_trust_and_hostname_with_controlled_certificate() {
    for (trusted, correct_name) in [(true, true), (false, true), (true, false)] {
        let identity = rcgen::generate_simple_self_signed(vec![
            if correct_name {
                "localhost"
            } else {
                "wrong.invalid"
            }
            .into(),
        ])
        .unwrap();
        let cert = identity.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let (address, task) = mock(move |socket| {
            let tls = rustls::StreamOwned::new(
                rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap(),
                socket.into_inner(),
            );
            match tungstenite::accept_hdr(tls, upgrade) {
                Ok(mut ws) => {
                    assert!(trusted && correct_name);
                    hello(&mut ws);
                    assert_eq!(
                        read(&mut ws),
                        json!({"id":2,"op":"invoke_action","key":"furnace1/apply"})
                    );
                    send(&mut ws, json!({"id":2,"status":"ok"}));
                    done_rx.recv_timeout(WAIT).unwrap();
                }
                Err(_) => assert!(!trusted || !correct_name),
            }
        });
        let mut options = WebSocketOptions::default();
        if trusted {
            options = options.trust_certificate(cert.to_vec()).unwrap();
        }
        let endpoint: Endpoint = format!("wss://localhost:{}/application/v1", address.port())
            .parse()
            .unwrap();
        let client = Client::start_endpoint(
            endpoint.with_websocket_options(options),
            ClientOptions::default(),
        )
        .unwrap();
        if trusted && correct_name {
            assert_eq!(event(&client), ClientEvent::Ready);
            assert_eq!(
                client.sender().request(Request::InvokeAction {
                    key: "furnace1/apply".into()
                }),
                Ok(2)
            );
            assert_eq!(
                event(&client),
                ClientEvent::Response(Response {
                    id: 2,
                    data: ResponseData::Ack
                })
            );
            client.request_shutdown();
            assert_eq!(stopped(&client).1, StopReason::Shutdown);
            done_tx.send(()).unwrap();
        } else {
            assert_eq!(stopped(&client).1, StopReason::Tls);
        }
        task.join().unwrap();
    }
}

#[test]
fn endpoint_validation_and_header_redaction() {
    for valid in [
        "127.0.0.1:7240",
        "tcp://127.0.0.1:7240",
        "ws://localhost:7241/application/v1",
        "wss://example.org/application/v1",
        "ws://[::1]:7241/application/v1",
    ] {
        assert!(valid.parse::<Endpoint>().is_ok());
    }
    for invalid in [
        "https://example.org/",
        "ws://user:secret@example.org/",
        "ws://example.org/#secret",
        "bad",
    ] {
        let e = invalid.parse::<Endpoint>().unwrap_err();
        assert!(!e.to_string().contains("secret"));
    }
    for name in [
        "Host",
        "Connection",
        "Upgrade",
        "Sec-WebSocket-Protocol",
        "Content-Length",
    ] {
        assert!(WebSocketOptions::default().header(name, "secret").is_err());
    }
    assert!(
        WebSocketOptions::default()
            .header("Authorization", "secret\r\nInjected: yes")
            .is_err()
    );
    assert!(
        WebSocketOptions::default()
            .header("Authorization", &"s".repeat(8193))
            .is_err()
    );
    assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Warn);
}

#[test]
fn steel_ws_tcp_switch_keeps_history_gaps_and_ack_measurement_order() {
    use lab_plot::{
        integration::{Integration, Phase},
        plots::PlotState,
        steel_api::{register_lab_functions, register_plot_functions},
    };
    fn pump(
        c: &mut Integration,
        p: &mut PlotState,
        condition: impl Fn(&Integration, &PlotState) -> bool,
    ) {
        let start = Instant::now();
        loop {
            assert!(c.update(p) <= 256);
            if condition(c, p) {
                break;
            }
            assert!(start.elapsed() < WAIT, "{:?}", c.reason);
            thread::sleep(Duration::from_millis(1));
        }
    }
    let mut integration = Integration::default();
    let mut plots = PlotState::default();
    let mut steel = steel::steel_vm::engine::Engine::new();
    register_lab_functions(&mut steel, integration.sender());
    register_plot_functions(&mut steel, integration.sender());
    steel
        .compile_and_run_raw_program(
            r#"(plot/add! "A" "V") (plot/add! "B" "V") (plot/bind! "A" "a") (plot/bind! "B" "b")"#,
        )
        .unwrap();
    for (iteration, ws) in [true, false, true].into_iter().enumerate() {
        let (done_tx, done_rx) = mpsc::channel();
        let data = |key| json!({"event":"measurement","status":"ok","key":key,"value":1.5,"unit":"V","completed_at_unix_ms":10});
        let (endpoint, task) = if ws {
            let (endpoint, task) = server(move |mut ws| {
                hello(&mut ws);
                let r = read(&mut ws);
                assert_eq!(r["op"], "subscribe");
                send(
                    &mut ws,
                    json!({"id":r["id"],"status":"ok","keys":["a","b"]}),
                );
                send(&mut ws, data("a"));
                send(&mut ws, data("b"));
                done_rx.recv_timeout(WAIT).unwrap();
            });
            (endpoint, task)
        } else {
            let (address, task) = mock(move |mut socket| {
                support::hello(&mut socket);
                let r = support::read(&mut socket);
                assert_eq!(r["op"], "subscribe");
                support::send(
                    &mut socket,
                    json!({"id":r["id"],"status":"ok","keys":["a","b"]}),
                );
                support::send(&mut socket, data("a"));
                support::send(&mut socket, data("b"));
                done_rx.recv_timeout(WAIT).unwrap();
            });
            (format!("tcp://{address}").parse().unwrap(), task)
        };
        assert!(integration.sender().connect_endpoint(endpoint).unwrap());
        assert_eq!(integration.phase(), Phase::Connecting);
        pump(&mut integration, &mut plots, |c, _| {
            c.phase() == Phase::Ready
        });
        assert!(integration.subscriptions.is_empty());
        steel
            .compile_and_run_raw_program("(lab/subscribe! '(\"a\" \"b\"))")
            .unwrap();
        pump(&mut integration, &mut plots, |_, p| {
            p.series["A"].points.len() == iteration + 1
                && p.series["B"].points.len() == iteration + 1
        });
        assert_eq!(plots.series["A"].segments().len(), iteration + 1);
        steel
            .compile_and_run_raw_program("(lab/disconnect!)")
            .unwrap();
        pump(&mut integration, &mut plots, |c, _| {
            c.phase() == Phase::Stopped
        });
        assert!(integration.subscriptions.is_empty());
        assert!(integration.boolean_signals.is_empty());
        done_tx.send(()).unwrap();
        task.join().unwrap();
    }
    integration.begin_close();
    assert!(integration.is_closed());
}

/// Explicit opt-in: run against lab-server examples/websocket.toml (ports=[], devices=[]).
#[test]
#[ignore = "requires separately started, hardware-free lab-server"]
fn lab_server_tcp_and_ws_contract() {
    for address in ["tcp://127.0.0.1:7240", "ws://127.0.0.1:7241/application/v1"] {
        let client = ready(address.parse().unwrap(), ClientOptions::default());
        for request in [Request::Ping, Request::Describe] {
            let describe = request == Request::Describe;
            let id = client.sender().request(request).unwrap();
            let ClientEvent::Response(response) = event(&client) else {
                panic!()
            };
            assert_eq!(response.id, id);
            if describe {
                assert_eq!(response.data.parameters(), Some([].as_slice()));
                assert_eq!(response.data.actions(), Some([].as_slice()));
            } else {
                assert_eq!(response.data, ResponseData::Ack);
            }
        }
        client.request_shutdown();
        assert_eq!(stopped(&client).1, StopReason::Shutdown);
    }
}
