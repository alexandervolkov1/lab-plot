//! The same action contract runs over both adapters, with no hardware.
use lab_plot::{
    integration::*, plots::PlotState, steel_api::register_lab_functions, tcp_client::*,
};
use serde_json::{Value, json};
use std::{
    io::BufReader,
    net::TcpStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use steel::steel_vm::engine::Engine;
use tungstenite::{Message, WebSocket};
#[allow(dead_code)]
mod support;
use support::WAIT;

enum Peer {
    Tcp(BufReader<TcpStream>),
    Ws(Box<WebSocket<TcpStream>>),
}
impl Peer {
    fn read(&mut self) -> Value {
        match self {
            Self::Tcp(s) => support::read(s),
            Self::Ws(s) => {
                let Message::Text(text) = s.read().unwrap() else {
                    panic!("expected Text")
                };
                assert!(!text.ends_with('\n'));
                serde_json::from_str(&text).unwrap()
            }
        }
    }
    fn send(&mut self, value: Value) {
        match self {
            Self::Tcp(s) => support::send(s, value),
            Self::Ws(s) => s.send(Message::Text(value.to_string().into())).unwrap(),
        }
    }
    fn hello(&mut self) {
        assert_eq!(self.read(), json!({"id":1,"op":"hello"}));
        self.send(json!({"id":1,"status":"ok","protocol":"lab-server","version":1}));
    }
    fn until_closed(&mut self) {
        match self {
            Self::Tcp(s) => support::until_closed(s),
            Self::Ws(s) => match s.read() {
                Ok(Message::Close(_)) => (),
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => (),
                Err(tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                )) => (),
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                other => panic!("unexpected traffic after final request: {other:?}"),
            },
        }
    }
}
#[allow(clippy::result_large_err)] // tungstenite's handshake callback has a fixed error type.
fn server(
    ws: bool,
    action: impl FnOnce(Peer) + Send + 'static,
) -> (Endpoint, thread::JoinHandle<()>) {
    let (address, task) = support::mock(move |socket| {
        let peer = if ws {
            let socket = tungstenite::accept_hdr(
                socket.into_inner(),
                |_: &tungstenite::handshake::server::Request,
                 mut response: tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "lab-server.application.v1".parse().unwrap(),
                    );
                    Ok(response)
                },
            )
            .unwrap();
            Peer::Ws(Box::new(socket))
        } else {
            Peer::Tcp(socket)
        };
        action(peer);
    });
    let endpoint = if ws {
        format!("ws://{address}/application/v1").parse().unwrap()
    } else {
        address.into()
    };
    (endpoint, task)
}

fn pump(c: &mut Integration, p: &mut PlotState, condition: impl Fn(&Integration) -> bool) {
    let start = Instant::now();
    loop {
        c.update(p);
        if condition(c) {
            return;
        }
        assert!(start.elapsed() < WAIT, "GUI pump timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
type EvalResult = Result<Vec<String>, String>;
struct SteelWorker {
    tx: mpsc::Sender<(String, mpsc::Sender<EvalResult>)>,
}
impl SteelWorker {
    fn new(sender: ControlSender) -> Self {
        let (tx, rx) = mpsc::channel::<(String, mpsc::Sender<EvalResult>)>();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut engine = Engine::new();
            register_lab_functions(&mut engine, sender);
            ready_tx.send(()).unwrap();
            while let Ok((source, reply)) = rx.recv() {
                let result = engine
                    .compile_and_run_raw_program(source)
                    .map(|v| v.into_iter().map(|v| v.to_string()).collect())
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        });
        // Engine bootstrap is unrelated to socket deadlines. Finish it before
        // starting the mock server, just as the app retains one Engine per REPL.
        ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        Self { tx }
    }
    fn eval(&self, source: String) -> mpsc::Receiver<EvalResult> {
        let (tx, rx) = mpsc::channel();
        self.tx.send((source, tx)).unwrap();
        rx
    }
}
fn result(
    c: &mut Integration,
    p: &mut PlotState,
    rx: mpsc::Receiver<Result<Vec<String>, String>>,
) -> Result<Vec<String>, String> {
    let start = Instant::now();
    loop {
        c.update(p);
        if let Ok(r) = rx.try_recv() {
            return r;
        }
        assert!(start.elapsed() < WAIT, "Steel result timeout");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn steel_confirmed_results_types_errors_and_structured_describe_over_tcp_and_ws() {
    for ws in [false, true] {
        let mut c = Integration::default();
        let mut p = PlotState::default();
        let steel = SteelWorker::new(c.sender());
        let (endpoint, task) = server(ws, |mut peer| {
            peer.hello();
            loop {
                let request = peer.read();
                let id = request["id"].clone();
                let reply = match request["op"].as_str().unwrap() {
                    "get" => {
                        let key = request["key"].as_str().unwrap();
                        match key {
                            "none" => json!({"id":id,"status":"no_data","key":key}),
                            "error" => {
                                json!({"id":id,"status":"error","key":key,"error":"read_failed","completed_at_unix_ms":1234})
                            }
                            _ => {
                                json!({"id":id,"status":"ok","key":key,"value": match key { "integer"=>json!(i64::MAX), "float"=>json!(68.4), "true"=>json!(true), _=>json!(false) },"unit":"","completed_at_unix_ms":1234})
                            }
                        }
                    }
                    "set" => {
                        assert_eq!(request["value"], json!(9007199254740993i64));
                        json!({"id":id,"status":"ok"})
                    }
                    "invoke_action" => match request["key"].as_str().unwrap() {
                        "ok" => json!({"id":id,"status":"ok"}),
                        "finish" => {
                            peer.send(json!({"id":id,"status":"ok"}));
                            peer.until_closed();
                            return;
                        }
                        key => json!({"id":id,"status":"error","key":key,"error":key}),
                    },
                    "subscribe" => json!({"id":id,"status":"ok","keys":["a","b"]}),
                    "unsubscribe" => json!({"id":id,"status":"ok","keys":["b"]}),
                    "describe" => {
                        json!({"id":id,"status":"ok","parameters":[{"key":"a","type":"integer","min":-9223372036854775808i64,"max":9223372036854775807i64,"unit":"","writable":true,"poll_period_ns":18446744073709551615u64}],"actions":[{"key":"ok","description":"Apply"}]})
                    }
                    other => panic!("{other}"),
                };
                peer.send(reply);
            }
        });
        c.sender().connect_endpoint(endpoint).unwrap();
        pump(&mut c, &mut p, |c| c.phase() == Phase::Ready);
        let source = r#"(lab/get "integer") (lab/get "float") (lab/get "true") (lab/get "false") (lab/get "none") (lab/set! "a" 9007199254740993) (lab/invoke-action! "ok") (lab/subscribe! '("a" "b")) (lab/unsubscribe! '("a")) (lab/describe)"#;
        let rx = steel.eval(source.into());
        let values = result(&mut c, &mut p, rx).unwrap();
        assert_eq!(
            &values[..9],
            &[
                "9223372036854775807",
                "68.4",
                "#true",
                "#false",
                "()",
                "#true",
                "#true",
                "(\"a\" \"b\")",
                "(\"b\")"
            ]
        );
        for text in [
            "parameters",
            "actions",
            "Apply",
            "18446744073709551615",
            "-9223372036854775808",
        ] {
            assert!(values[9].contains(text), "{}", values[9]);
        }
        for (expression, error) in [
            (r#"(lab/get "error")"#, "read_failed"),
            (r#"(lab/invoke-action! "unknown_key")"#, "unknown_key"),
            (
                r#"(lab/invoke-action! "outcome_unknown")"#,
                "outcome_unknown",
            ),
        ] {
            let rx = steel.eval(expression.into());
            assert!(result(&mut c, &mut p, rx).unwrap_err().contains(error));
        }
        let rx = steel.eval(r#"(lab/invoke-action! "finish")"#.into());
        assert_eq!(result(&mut c, &mut p, rx).unwrap(), vec!["#true"]);
        c.begin_close();
        pump(&mut c, &mut p, Integration::is_closed);
        task.join().unwrap();
    }
}

#[test]
fn slow_server_does_not_block_gui_and_replies_are_matched_out_of_order() {
    for ws in [false, true] {
        let mut c = Integration::default();
        let mut p = PlotState::default();
        let steel = SteelWorker::new(c.sender());
        let steel_b = SteelWorker::new(c.sender());
        let (seen_tx, seen_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel();
        let (endpoint, task) = server(ws, move |mut peer| {
            peer.hello();
            let first = peer.read();
            let second = peer.read();
            seen_tx.send(()).unwrap();
            gate_rx.recv_timeout(WAIT).unwrap();
            for request in [second, first] {
                peer.send(json!({"id":request["id"],"status":"ok","key":request["key"],"value": if request["key"]=="a" {11} else {22},"unit":"","completed_at_unix_ms":1234}));
            }
            peer.until_closed();
        });
        c.sender().connect_endpoint(endpoint).unwrap();
        pump(&mut c, &mut p, |c| c.phase() == Phase::Ready);
        let a = steel.eval(r#"(lab/get "a")"#.into());
        let b = steel_b.eval(r#"(lab/get "b")"#.into());
        seen_rx.recv_timeout(WAIT).unwrap();
        assert!(a.try_recv().is_err() && b.try_recv().is_err());
        // The server cannot reply until these GUI frames have completed.
        let ctx = eframe::egui::Context::default();
        for _ in 0..10 {
            ctx.run_ui(Default::default(), |ui| {
                c.update(&mut p);
                ui.label("GUI remains responsive");
            })
            .drop_without_applying_deltas();
        }
        gate_tx.send(()).unwrap();
        assert_eq!(result(&mut c, &mut p, a).unwrap(), vec!["11"]);
        assert_eq!(result(&mut c, &mut p, b).unwrap(), vec!["22"]);
        c.begin_close();
        pump(&mut c, &mut p, Integration::is_closed);
        task.join().unwrap();
    }
}

#[test]
fn disconnect_and_shutdown_wake_steel_with_uncertainty_over_both_transports() {
    for ws in [false, true] {
        for shutdown in [false, true] {
            let mut c = Integration::default();
            let mut p = PlotState::default();
            let steel = SteelWorker::new(c.sender());
            let (seen_tx, seen_rx) = mpsc::channel();
            let (endpoint, task) = server(ws, move |mut peer| {
                peer.hello();
                peer.read();
                seen_tx.send(()).unwrap();
                peer.until_closed();
            });
            c.sender().connect_endpoint(endpoint).unwrap();
            pump(&mut c, &mut p, |c| c.phase() == Phase::Ready);
            let rx = steel.eval(r#"(lab/invoke-action! "apply")"#.into());
            seen_rx.recv_timeout(WAIT).unwrap();
            if shutdown {
                c.begin_close();
            } else {
                c.sender().disconnect().unwrap();
            }
            assert!(
                result(&mut c, &mut p, rx)
                    .unwrap_err()
                    .contains("OutcomeUnknown")
            );
            pump(&mut c, &mut p, |c| c.phase() == Phase::Stopped);
            task.join().unwrap();
            let rx = steel.eval(r#"(lab/set! "a" 1)"#.into());
            assert!(result(&mut c, &mut p, rx).unwrap_err().contains("NotSent"));
        }
    }
}
