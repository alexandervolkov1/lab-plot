use lab_plot::tcp_client::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener},
    sync::{Arc, Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};

mod support;
use support::*;
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
    let mut events = Vec::new();
    loop {
        match event(client) {
            ClientEvent::Stopped(reason) => {
                assert!(client.try_recv().is_none());
                return (events, reason);
            }
            event => events.push(event),
        }
    }
}
fn wait_state(client: &Client, predicate: impl Fn(&ClientState) -> bool) {
    let start = Instant::now();
    while !predicate(&client.state()) {
        assert!(
            start.elapsed() < WAIT,
            "state timeout: {:?}",
            client.state()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn ready(address: SocketAddr, options: ClientOptions) -> Client {
    let client = Client::start(address, options).unwrap();
    assert_eq!(event(&client), ClientEvent::Ready);
    client
}
fn measurement(key: &str, value: Value) -> Value {
    json!({"event":"measurement","status":"ok","key":key,"value":value,"unit":"degC","completed_at_unix_ms":123})
}

#[test]
fn hello_is_required_and_version_is_checked() {
    for (protocol, version) in [("lab-server", 2), ("wrong", 1)] {
        let (seen_tx, seen_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (address, server) = mock(move |mut socket| {
            assert_eq!(read(&mut socket), json!({"id":1,"op":"hello"}));
            seen_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            send(
                &mut socket,
                json!({"id":1,"status":"ok","protocol":protocol,"version":version}),
            );
            until_closed(&mut socket);
        });
        let client = Client::start(address, ClientOptions::default()).unwrap();
        seen_rx.recv_timeout(WAIT).unwrap();
        assert_eq!(
            client.sender().request(Request::Ping),
            Err(RequestError::NotReady)
        );
        release_tx.send(()).unwrap();
        assert_eq!(stopped(&client), (vec![], StopReason::IncompatibleProtocol));
        server.join().unwrap();
    }
}

#[test]
fn concurrent_ids_multiple_pending_reverse_responses_and_measurements() {
    const COUNT: usize = 48;
    let (address, server) = mock(|mut socket| {
        hello(&mut socket);
        let mut ids = Vec::new();
        for expected in 2..COUNT as u64 + 2 {
            let request = read(&mut socket);
            assert_eq!(request, json!({"id":expected,"op":"ping"}));
            ids.push(expected);
        }
        for id in ids.into_iter().rev() {
            send(&mut socket, measurement("a", json!(25.0)));
            send(&mut socket, json!({"id":id,"status":"ok"}));
        }
        until_closed(&mut socket);
    });
    let mut client = ready(address, ClientOptions::default());
    let barrier = Arc::new(Barrier::new(8));
    let senders: Vec<_> = (0..8)
        .map(|_| {
            let sender = client.sender();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                (0..6)
                    .map(|_| sender.request(Request::Ping).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut ids: Vec<_> = senders
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, (2..COUNT as u64 + 2).collect::<Vec<_>>());
    for id in ids.into_iter().rev() {
        assert!(matches!(
            event(&client),
            ClientEvent::Measurement(Measurement {
                reading: Reading::Value {
                    value: ScalarValue::Float(25.0),
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id,
                data: ResponseData::Ack
            })
        );
    }
    client.shutdown();
    assert_eq!(stopped(&client), (vec![], StopReason::Shutdown));
    assert_eq!(
        client.sender().request(Request::Ping),
        Err(RequestError::Stopped)
    );
    server.join().unwrap();
}

#[test]
fn subscriptions_are_additive_and_unsubscribe_allows_prior_events() {
    let (address, server) = mock(|mut socket| {
        hello(&mut socket);
        for (id, op, keys, full) in [
            (2, "subscribe", vec!["a"], vec!["a"]),
            (3, "subscribe", vec!["b"], vec!["a", "b"]),
            (4, "unsubscribe", vec!["a"], vec!["b"]),
        ] {
            assert_eq!(read(&mut socket), json!({"id":id,"op":op,"keys":keys}));
            if id == 4 {
                send(&mut socket, measurement("a", json!(1)));
            }
            send(&mut socket, json!({"id":id,"status":"ok","keys":full}));
            for key in full {
                send(&mut socket, measurement(key, json!(2)));
            }
        }
        until_closed(&mut socket);
    });
    let mut client = ready(address, ClientOptions::default());
    for (request, full, before) in [
        (
            Request::Subscribe {
                keys: vec!["a".into()],
            },
            vec!["a"],
            false,
        ),
        (
            Request::Subscribe {
                keys: vec!["b".into()],
            },
            vec!["a", "b"],
            false,
        ),
        (
            Request::Unsubscribe {
                keys: vec!["a".into()],
            },
            vec!["b"],
            true,
        ),
    ] {
        let id = client.sender().request(request).unwrap();
        if before {
            assert!(
                matches!(event(&client), ClientEvent::Measurement(Measurement { key, .. }) if key == "a")
            );
        }
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id,
                data: ResponseData::Subscriptions(full.iter().map(|s| (*s).into()).collect())
            })
        );
        for key in full {
            assert!(
                matches!(event(&client), ClientEvent::Measurement(Measurement { key: actual, .. }) if key == actual)
            );
        }
    }
    client.shutdown();
    server.join().unwrap();
}

#[test]
fn all_request_fields_and_get_results() {
    let (address, server) = mock(|mut socket| {
        hello(&mut socket);
        assert_eq!(read(&mut socket), json!({"id":2,"op":"describe"}));
        send(
            &mut socket,
            json!({"id":2,"status":"ok","parameters":[{"key":"p","type":"integer","min":0,"max":100,"unit":"%","writable":true,"poll_period_ns":null}]}),
        );
        for id in 3..=5 {
            assert_eq!(read(&mut socket), json!({"id":id,"op":"get","key":"a"}));
            let response = match id {
                3 => json!({"id":id,"status":"no_data","key":"a"}),
                4 => {
                    json!({"id":id,"status":"error","key":"a","error":"Empty response","completed_at_unix_ms":123})
                }
                _ => {
                    json!({"id":id,"status":"ok","key":"a","value":-3,"unit":"%","completed_at_unix_ms":124})
                }
            };
            send(&mut socket, response);
        }
        for (id, value, error) in [
            (6, json!(20), "outcome_unknown"),
            (7, json!(20.0), "runtime_timeout"),
        ] {
            assert_eq!(
                read(&mut socket),
                json!({"id":id,"op":"set","key":"p","value":value})
            );
            send(&mut socket, json!({"id":id,"status":"error","error":error}));
        }
        until_closed(&mut socket);
    });
    let mut client = ready(address, ClientOptions::default());
    client.sender().request(Request::Describe).unwrap();
    assert!(
        matches!(event(&client), ClientEvent::Response(Response { data: ResponseData::Parameters(p), .. }) if p.len() == 1 && p[0].poll_period_ns.is_none())
    );
    for id in 3..=5 {
        client
            .sender()
            .request(Request::Get { key: "a".into() })
            .unwrap();
        let ClientEvent::Response(response) = event(&client) else {
            panic!()
        };
        assert_eq!(response.id, id);
        assert_eq!(
            response.status(),
            match id {
                3 => Status::NoData,
                4 => Status::Error,
                _ => Status::Ok,
            }
        );
        if id == 4 {
            assert!(
                matches!(response.data, ResponseData::Measurement(Measurement { completed_at_unix_ms: 123, reading: Reading::Error(error), .. }) if error == "Empty response")
            );
        }
    }
    for (value, error) in [
        (ScalarValue::Integer(20), "outcome_unknown"),
        (ScalarValue::Float(20.0), "runtime_timeout"),
    ] {
        let id = client
            .sender()
            .request(Request::Set {
                key: "p".into(),
                value,
            })
            .unwrap();
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id,
                data: ResponseData::Error(ServerError {
                    error: error.into(),
                    key: None
                })
            })
        );
    }
    client.shutdown();
    assert_eq!(stopped(&client).0, vec![]);
    server.join().unwrap();
}

#[test]
fn malformed_frames_preserve_preceding_response() {
    let cases = vec![
        (b"{bad}\n".to_vec(), None),
        (b"\n".to_vec(), Some(FrameError::EmptyFrame)),
        (vec![0xff, b'\n'], Some(FrameError::InvalidUtf8)),
        (
            vec![b'x'; MAX_RESPONSE_FRAME_BYTES + 1],
            Some(FrameError::FrameTooLong),
        ),
        (b"{unfinished".to_vec(), Some(FrameError::IncompleteFrame)),
    ];
    for (bad, expected) in cases {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            assert_eq!(read(&mut socket)["id"], 2);
            socket.get_mut().write_all(b"{\"id\":").unwrap();
            let mut rest = b"2,\"status\":\"ok\"}\r\n".to_vec();
            rest.extend(bad);
            socket.get_mut().write_all(&rest).unwrap();
        });
        let client = ready(address, ClientOptions::default());
        client.sender().request(Request::Ping).unwrap();
        assert_eq!(
            event(&client),
            ClientEvent::Response(Response {
                id: 2,
                data: ResponseData::Ack
            })
        );
        let (events, reason) = stopped(&client);
        assert!(events.is_empty());
        match expected {
            Some(error) => assert_eq!(reason, StopReason::Frame(error)),
            None => assert!(matches!(reason, StopReason::Protocol(_))),
        }
        server.join().unwrap();
    }
}

#[test]
fn unknown_and_duplicate_response_ids_are_fatal() {
    for duplicate in [false, true] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            read(&mut socket);
            if duplicate {
                send(&mut socket, json!({"id":2,"status":"ok"}));
            }
            send(
                &mut socket,
                json!({"id":if duplicate { 2 } else { 99 },"status":"ok"}),
            );
            until_closed(&mut socket);
        });
        let client = ready(address, ClientOptions::default());
        client.sender().request(Request::Ping).unwrap();
        let (events, reason) = stopped(&client);
        assert_eq!(
            reason,
            StopReason::UnexpectedResponseId(if duplicate { 2 } else { 99 })
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ClientEvent::Response(_)) == duplicate);
        server.join().unwrap();
    }
}

#[test]
fn eof_with_pending_set_is_unknown_even_with_partial_response() {
    for partial in [false, true] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            assert_eq!(read(&mut socket)["op"], "set");
            if partial {
                socket
                    .get_mut()
                    .write_all(b"{\"id\":2,\"status\":")
                    .unwrap();
            }
        });
        let client = ready(address, ClientOptions::default());
        let id = client
            .sender()
            .request(Request::Set {
                key: "p".into(),
                value: ScalarValue::Integer(1),
            })
            .unwrap();
        let (events, reason) = stopped(&client);
        assert_eq!(
            events,
            vec![ClientEvent::RequestFailed {
                id,
                failure: RequestFailure::OutcomeUnknown
            }]
        );
        assert_eq!(
            reason,
            if partial {
                StopReason::Frame(FrameError::IncompleteFrame)
            } else {
                StopReason::Eof
            }
        );
        server.join().unwrap();
    }
}

#[test]
fn observation_failures_visible_even_with_full_event_queue() {
    for (error, reason) in [
        ("observation_data_lost", StopReason::ObservationDataLost),
        (
            "observation_stream_closed",
            StopReason::ObservationStreamClosed,
        ),
    ] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            send(&mut socket, json!({"status":"error","error":error}));
            until_closed(&mut socket);
        });
        let client = Client::start(
            address,
            ClientOptions {
                event_capacity: 1,
                ..Default::default()
            },
        )
        .unwrap();
        wait_state(&client, |state| matches!(state, ClientState::Stopped(_)));
        assert_eq!(client.state(), ClientState::Stopped(reason.clone()));
        let (events, actual) = stopped(&client);
        assert_eq!(actual, reason);
        assert_eq!(
            events,
            vec![
                ClientEvent::Ready,
                ClientEvent::ServerError(ServerError {
                    error: error.into(),
                    key: None
                })
            ]
        );
        server.join().unwrap();
    }
}

#[test]
fn event_overflow_is_fatal_and_retains_definitive_response() {
    for response in [true, false] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            read(&mut socket);
            if response {
                send(&mut socket, json!({"id":2,"status":"ok"}));
            } else {
                send(&mut socket, measurement("a", json!(1)));
            }
            until_closed(&mut socket);
        });
        let client = Client::start(
            address,
            ClientOptions {
                event_capacity: 1,
                ..Default::default()
            },
        )
        .unwrap();
        wait_state(&client, |s| *s == ClientState::Ready);
        client
            .sender()
            .request(Request::Set {
                key: "p".into(),
                value: ScalarValue::Integer(1),
            })
            .unwrap();
        wait_state(&client, |s| matches!(s, ClientState::Stopped(_)));
        assert_eq!(
            client.state(),
            ClientState::Stopped(StopReason::EventQueueOverflow)
        );
        let (events, reason) = stopped(&client);
        assert_eq!(reason, StopReason::EventQueueOverflow);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    ClientEvent::Response(_) | ClientEvent::RequestFailed { .. }
                ))
                .count(),
            1
        );
        if response {
            assert!(matches!(events[1], ClientEvent::Response(_)));
        } else {
            assert_eq!(
                events[2],
                ClientEvent::RequestFailed {
                    id: 2,
                    failure: RequestFailure::OutcomeUnknown
                }
            );
        }
        server.join().unwrap();
    }
}

#[test]
fn bounded_pending_outgoing_and_shutdown_classify_unsent_set() {
    let (seen_tx, seen_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        assert_eq!(read(&mut socket)["id"], 2);
        seen_tx.send(()).unwrap();
        until_closed(&mut socket);
    });
    let mut client = ready(
        address,
        ClientOptions {
            pending_capacity: 1,
            outgoing_capacity: 1,
            ..Default::default()
        },
    );
    client.sender().request(Request::Ping).unwrap();
    seen_rx.recv_timeout(WAIT).unwrap();
    let id = client
        .sender()
        .request(Request::Set {
            key: "p".into(),
            value: ScalarValue::Integer(1),
        })
        .unwrap();
    assert_eq!(
        client.sender().request(Request::Ping),
        Err(RequestError::QueueFull)
    );
    client.shutdown();
    client.shutdown();
    let (events, reason) = stopped(&client);
    assert_eq!(reason, StopReason::Shutdown);
    assert_eq!(
        events,
        vec![
            ClientEvent::RequestFailed {
                id: 2,
                failure: RequestFailure::ResponseUnavailable
            },
            ClientEvent::RequestFailed {
                id,
                failure: RequestFailure::NotSent
            }
        ]
    );
    server.join().unwrap();
}

#[test]
fn hello_timeout_and_shutdown_during_handshake() {
    for shutdown in [false, true] {
        let (seen_tx, seen_rx) = mpsc::sync_channel(1);
        let (address, server) = mock(move |mut socket| {
            read(&mut socket);
            seen_tx.send(()).unwrap();
            until_closed(&mut socket);
        });
        let mut client = Client::start(
            address,
            ClientOptions {
                hello_timeout: if shutdown {
                    WAIT
                } else {
                    Duration::from_millis(100)
                },
                ..Default::default()
            },
        )
        .unwrap();
        seen_rx.recv_timeout(WAIT).unwrap();
        if shutdown {
            client.shutdown();
        }
        assert_eq!(
            stopped(&client).1,
            if shutdown {
                StopReason::Shutdown
            } else {
                StopReason::HelloTimeout
            }
        );
        server.join().unwrap();
    }
}

#[test]
fn request_timeout_does_not_claim_set_was_cancelled() {
    let (address, server) = mock(|mut socket| {
        hello(&mut socket);
        read(&mut socket);
        until_closed(&mut socket);
    });
    let client = ready(
        address,
        ClientOptions {
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        },
    );
    client
        .sender()
        .request(Request::Set {
            key: "p".into(),
            value: ScalarValue::Integer(1),
        })
        .unwrap();
    assert_eq!(
        stopped(&client),
        (
            vec![ClientEvent::RequestFailed {
                id: 2,
                failure: RequestFailure::OutcomeUnknown
            }],
            StopReason::RequestTimeout(2)
        )
    );
    server.join().unwrap();
}

#[test]
fn connect_failure_and_invalid_options() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut address = listener.local_addr().unwrap();
    assert!(
        Client::start(
            address,
            ClientOptions {
                event_capacity: 0,
                ..Default::default()
            }
        )
        .is_err()
    );
    // Port zero cannot be a listening destination; avoid a released-port race.
    address.set_port(0);
    let client = Client::start(address, ClientOptions::default()).unwrap();
    assert!(matches!(stopped(&client).1, StopReason::Connect(_)));
}

#[test]
fn full_pending_and_outgoing_queues_each_get_one_final_result_on_overflow() {
    let (seen_tx, seen_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        for id in 2..66 {
            assert_eq!(read(&mut socket)["id"], id);
        }
        seen_tx.send(()).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
        send(&mut socket, measurement("a", json!(1)));
        send(&mut socket, measurement("a", json!(2)));
        until_closed(&mut socket);
    });
    let client = ready(
        address,
        ClientOptions {
            event_capacity: 1,
            ..Default::default()
        },
    );
    for id in 2..66 {
        assert_eq!(
            client.sender().request(Request::Set {
                key: "p".into(),
                value: ScalarValue::Integer(id as i64)
            }),
            Ok(id)
        );
    }
    seen_rx.recv_timeout(WAIT).unwrap();
    for id in 66..130 {
        assert_eq!(
            client.sender().request(Request::Set {
                key: "p".into(),
                value: ScalarValue::Integer(id as i64)
            }),
            Ok(id)
        );
    }
    assert_eq!(
        client.sender().request(Request::Ping),
        Err(RequestError::QueueFull)
    );
    release_tx.send(()).unwrap();
    wait_state(&client, |s| matches!(s, ClientState::Stopped(_)));
    assert_eq!(
        client.state(),
        ClientState::Stopped(StopReason::EventQueueOverflow)
    );
    let (events, _) = stopped(&client);
    assert_eq!(events.len(), 130);
    for (id, result) in (2..130).zip(&events[2..]) {
        assert_eq!(
            *result,
            ClientEvent::RequestFailed {
                id,
                failure: if id < 66 {
                    RequestFailure::OutcomeUnknown
                } else {
                    RequestFailure::NotSent
                }
            }
        );
    }
    server.join().unwrap();
}

#[test]
fn idless_error_does_not_consume_pending_and_mismatched_reply_is_rejected() {
    for mismatch in [false, true] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            read(&mut socket);
            send(
                &mut socket,
                json!({"status":"error","error":"duplicate_id"}),
            );
            if mismatch {
                send(&mut socket, json!({"id":2,"status":"ok","keys":[]}));
            } else {
                send(&mut socket, json!({"id":2,"status":"ok"}));
            }
            until_closed(&mut socket);
        });
        let mut client = ready(address, ClientOptions::default());
        client
            .sender()
            .request(Request::Set {
                key: "p".into(),
                value: ScalarValue::Integer(1),
            })
            .unwrap();
        assert_eq!(
            event(&client),
            ClientEvent::ServerError(ServerError {
                error: "duplicate_id".into(),
                key: None
            })
        );
        if mismatch {
            let (events, reason) = stopped(&client);
            assert!(matches!(reason, StopReason::Protocol(_)));
            assert_eq!(
                events,
                vec![ClientEvent::RequestFailed {
                    id: 2,
                    failure: RequestFailure::OutcomeUnknown
                }]
            );
        } else {
            assert_eq!(
                event(&client),
                ClientEvent::Response(Response {
                    id: 2,
                    data: ResponseData::Ack
                })
            );
            client.shutdown();
            assert_eq!(stopped(&client).0, vec![]);
        }
        server.join().unwrap();
    }
}
