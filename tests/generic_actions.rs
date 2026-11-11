//! The same action contract runs over both adapters, with no hardware.
use lab_plot::tcp_client::*;
use serde_json::{Value, json};
use std::{
    io::BufReader,
    net::TcpStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
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
fn event(client: &Client) -> ClientEvent {
    let start = Instant::now();
    loop {
        if let Some(event) = client.try_recv() {
            return event;
        }
        assert!(start.elapsed() < WAIT, "{:?}", client.state());
        thread::sleep(Duration::from_millis(1));
    }
}
fn action() -> Request {
    Request::InvokeAction {
        key: "furnace1/apply".into(),
    }
}

#[test]
fn tcp_and_ws_catalogs_out_of_order_action_results_and_existing_commands() {
    for ws in [false, true] {
        let (endpoint, task) = server(ws, |mut peer| {
            peer.hello();
            for id in [2, 3] {
                assert_eq!(peer.read(), json!({"id":id,"op":"describe"}));
                let mut reply = json!({"id":id,"status":"ok","parameters":[]});
                if id == 3 {
                    reply["actions"] = json!([{"key":"furnace1/apply","description":"Apply staged configuration"}]);
                }
                peer.send(reply);
            }
            for id in 4..=7 {
                assert_eq!(
                    peer.read(),
                    json!({"id":id,"op":"invoke_action","key":"furnace1/apply"})
                );
            }
            // Deliberately answer in reverse order. Errors retain both code and key.
            for (id, error) in [
                (7, "outcome_unknown"),
                (6, "runtime_timeout"),
                (5, "unknown_key"),
            ] {
                peer.send(json!({"id":id,"status":"error","error":error,"key":"furnace1/apply"}));
            }
            peer.send(json!({"id":4,"status":"ok"}));
            for (id, op, reply) in [
                (8, "get", json!({"id":8,"status":"no_data","key":"p"})),
                (9, "set", json!({"id":9,"status":"ok"})),
                (10, "subscribe", json!({"id":10,"status":"ok","keys":["p"]})),
                (11, "unsubscribe", json!({"id":11,"status":"ok","keys":[]})),
                (12, "ping", json!({"id":12,"status":"ok"})),
            ] {
                let request = peer.read();
                assert_eq!(request["id"], id);
                assert_eq!(request["op"], op);
                peer.send(reply);
            }
            peer.until_closed();
        });
        let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
        assert_eq!(event(&client), ClientEvent::Ready);
        for id in [2, 3] {
            assert_eq!(client.sender().request(Request::Describe), Ok(id));
            let ClientEvent::Response(response) = event(&client) else {
                panic!()
            };
            assert_eq!(response.id, id);
            assert_eq!(response.data.parameters(), Some([].as_slice()));
            assert_eq!(response.data.actions().unwrap().len(), (id - 2) as usize);
        }
        for id in 4..=7 {
            assert_eq!(client.sender().request(action()), Ok(id));
        }
        for (id, error) in [
            (7, "outcome_unknown"),
            (6, "runtime_timeout"),
            (5, "unknown_key"),
        ] {
            assert_eq!(
                event(&client),
                ClientEvent::Response(Response {
                    id,
                    data: ResponseData::Error(ServerError {
                        error: error.into(),
                        key: Some("furnace1/apply".into())
                    })
                })
            );
        }
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id: 4,
                data: ResponseData::Ack
            })
        );
        for request in [
            Request::Get { key: "p".into() },
            Request::Set {
                key: "p".into(),
                value: ScalarValue::Boolean(true),
            },
            Request::Subscribe {
                keys: vec!["p".into()],
            },
            Request::Unsubscribe {
                keys: vec!["p".into()],
            },
            Request::Ping,
        ] {
            let id = client.sender().request(request).unwrap();
            assert!(matches!(event(&client), ClientEvent::Response(r) if r.id == id));
        }
        client.request_shutdown();
        assert_eq!(event(&client), ClientEvent::Stopped(StopReason::Shutdown));
        assert!(client.try_recv().is_none());
        task.join().unwrap();
    }
}

#[test]
fn tcp_and_ws_action_disconnect_timeout_and_queue_limits_never_retry() {
    for ws in [false, true] {
        for mode in ["disconnect", "eof", "timeout"] {
            let (seen_tx, seen_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (endpoint, task) = server(ws, move |mut peer| {
                peer.hello();
                assert_eq!(
                    peer.read(),
                    json!({"id":2,"op":"invoke_action","key":"furnace1/apply"})
                );
                seen_tx.send(()).unwrap();
                release_rx.recv_timeout(WAIT).unwrap();
                if mode != "eof" {
                    peer.until_closed();
                }
            });
            let client = Client::start_endpoint(
                endpoint,
                ClientOptions {
                    pending_capacity: 1,
                    outgoing_capacity: 1,
                    request_timeout: if mode == "timeout" {
                        Duration::from_millis(500)
                    } else {
                        WAIT
                    },
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(event(&client), ClientEvent::Ready);
            assert_eq!(
                client.sender().request(Request::InvokeAction {
                    key: "x".repeat(MAX_REQUEST_FRAME_BYTES)
                }),
                Err(RequestError::Encode(EncodeError::FrameTooLong))
            );
            assert_eq!(client.sender().request(action()), Ok(2));
            seen_rx.recv_timeout(WAIT).unwrap();
            assert_eq!(client.sender().request(action()), Ok(3));
            assert_eq!(
                client.sender().request(action()),
                Err(RequestError::QueueFull)
            );
            if mode == "disconnect" {
                client.request_shutdown();
            }
            release_tx.send(()).unwrap();
            for (id, failure) in [
                (2, RequestFailure::OutcomeUnknown),
                (3, RequestFailure::NotSent),
            ] {
                assert_eq!(event(&client), ClientEvent::RequestFailed { id, failure });
            }
            let ClientEvent::Stopped(reason) = event(&client) else {
                panic!()
            };
            if mode == "timeout" {
                assert_eq!(reason, StopReason::RequestTimeout(2));
            }
            if mode == "disconnect" {
                assert_eq!(reason, StopReason::Shutdown);
            }
            assert!(client.try_recv().is_none());
            assert_eq!(
                client.sender().request(action()),
                Err(RequestError::Stopped)
            );
            task.join().unwrap();
        }
    }
}

#[test]
fn tcp_and_ws_untrustworthy_action_reply_preserves_uncertainty_and_one_final_result() {
    for ws in [false, true] {
        for reply in [
            json!({"id":2,"status":"no_data","key":"furnace1/apply"}),
            json!({"id":2,"status":"error","error":"unknown_key","key":"other/apply"}),
            json!({"id":2,"status":"ok","parameters":[],"actions":[]}),
            json!({"id":99,"status":"ok"}),
        ] {
            let (endpoint, task) = server(ws, move |mut peer| {
                peer.hello();
                peer.read();
                peer.send(reply);
                peer.until_closed();
            });
            let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
            assert_eq!(event(&client), ClientEvent::Ready);
            client.sender().request(action()).unwrap();
            assert_eq!(
                event(&client),
                ClientEvent::RequestFailed {
                    id: 2,
                    failure: RequestFailure::OutcomeUnknown
                }
            );
            assert!(matches!(
                event(&client),
                ClientEvent::Stopped(
                    StopReason::Protocol(_) | StopReason::UnexpectedResponseId(99)
                )
            ));
            assert!(client.try_recv().is_none());
            task.join().unwrap();
        }
        let (endpoint, task) = server(ws, |mut peer| {
            peer.hello();
            peer.read();
            for _ in 0..2 {
                peer.send(json!({"id":2,"status":"ok"}));
            }
            peer.until_closed();
        });
        let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
        assert_eq!(event(&client), ClientEvent::Ready);
        client.sender().request(action()).unwrap();
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id: 2,
                data: ResponseData::Ack
            })
        );
        assert_eq!(
            event(&client),
            ClientEvent::Stopped(StopReason::UnexpectedResponseId(2))
        );
        assert!(client.try_recv().is_none());
        task.join().unwrap();
    }
}
