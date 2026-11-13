use lab_plot::{integration::*, plots::*, steel_api::*, tcp_client::*};
use serde_json::json;
use std::{
    io::Write,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use steel::steel_vm::engine::Engine;
mod support;
use support::*;

fn sample(key: &str, value: ScalarValue, unit: &str, time: u64) -> Measurement {
    Measurement {
        key: key.into(),
        completed_at_unix_ms: time,
        reading: Reading::Value {
            value,
            unit: unit.into(),
        },
    }
}
fn add(plots: &mut PlotState, name: &str, key: &str) {
    plots
        .apply(PlotCommand::Add {
            name: name.into(),
            unit: "degC".into(),
        })
        .unwrap();
    plots
        .apply(PlotCommand::Bind {
            name: name.into(),
            key: key.into(),
        })
        .unwrap();
}
fn pump(
    connection: &mut Integration,
    plots: &mut PlotState,
    condition: impl Fn(&Integration, &PlotState) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    loop {
        connection.update(plots);
        if condition(connection, plots) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "GUI timeout: {:?}, {:?}",
            connection.phase(),
            connection.reason
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn close(connection: &mut Integration, plots: &mut PlotState) {
    connection.begin_close();
    pump(connection, plots, |c, _| c.is_closed());
}

fn has_result(connection: &Integration, id: u64) -> bool {
    connection
        .journal
        .iter()
        .any(|entry| entry.request_id() == Some(id))
}

#[test]
fn admitted_describe_and_actions_are_journaled_once_and_not_replayed_on_reconnect() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let steel = SteelThread::new(connection.sender());
    let (gate_tx, gate_rx) = mpsc::channel();
    let (seen_tx, seen_rx) = mpsc::channel();
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        assert_eq!(read(&mut socket), json!({"id":2,"op":"describe"}));
        for id in 3..=6 {
            assert_eq!(
                read(&mut socket),
                json!({"id":id,"op":"invoke_action","key":"furnace1/apply"})
            );
        }
        seen_tx.send(()).unwrap();
        gate_rx.recv_timeout(WAIT).unwrap();
        send(
            &mut socket,
            json!({"id":2,"status":"ok","parameters":[],"actions":[{"key":"furnace1/apply","description":"Apply staged configuration"}]}),
        );
        send(&mut socket, json!({"id":3,"status":"ok"}));
        for (id, error) in [
            (4, "unknown_key"),
            (5, "runtime_timeout"),
            (6, "outcome_unknown"),
        ] {
            send(
                &mut socket,
                json!({"id":id,"status":"error","error":error,"key":"furnace1/apply"}),
            );
        }
        assert_eq!(
            read(&mut socket),
            json!({"id":7,"op":"invoke_action","key":"furnace1/apply"})
        );
        // Drop the connection after receiving an action, without an ACK.
    });
    for source in ["(lab/describe)", r#"(lab/invoke-action! "furnace1/apply")"#] {
        assert!(steel.eval(source).is_err());
    }
    steel
        .eval(&format!("(lab/connect! \"{address}\")"))
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    for source in [
        "(lab/invoke-action! #t)",
        "(lab/invoke-action!)",
        "(lab/describe 1)",
    ] {
        assert!(steel.eval(source).is_err());
    }
    assert!(
        steel
            .eval(&format!(
                "(lab/invoke-action! \"{}\")",
                "x".repeat(MAX_REQUEST_FRAME_BYTES)
            ))
            .is_err()
    );
    let sender = connection.sender();
    assert_eq!(sender.request(Request::Describe).unwrap(), 2);
    for id in 3..=6 {
        assert_eq!(
            sender
                .request(Request::InvokeAction {
                    key: "furnace1/apply".into()
                })
                .unwrap(),
            id
        );
    }
    seen_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(steel.eval("(+ 20 22)").unwrap(), vec!["42"]);
    connection.update(&mut plots);
    assert!(!has_result(&connection, 2));
    assert!(!has_result(&connection, 3));
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, _| has_result(c, 6));
    for (id, text) in [
        (2, "Apply staged configuration"),
        (3, "ACK"),
        (4, "unknown_key"),
        (5, "runtime_timeout"),
        (6, "outcome_unknown"),
    ] {
        let entries: Vec<_> = connection
            .journal
            .iter()
            .filter(|e| e.request_id() == Some(id))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].text().contains(text));
    }
    assert_eq!(
        sender
            .request(Request::InvokeAction {
                key: "furnace1/apply".into()
            })
            .unwrap(),
        7
    );
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Stopped
    });
    assert!(
        connection
            .journal
            .iter()
            .any(|e| e.request_id() == Some(7) && e.text().contains("OutcomeUnknown"))
    );
    server.join().unwrap();

    let (address, server) = mock(|mut socket| {
        hello(&mut socket);
        // The new connection receives only explicitly submitted commands.
        assert_eq!(read(&mut socket), json!({"id":2,"op":"describe"}));
        send(&mut socket, json!({"id":2,"status":"ok","parameters":[]}));
        assert_eq!(read(&mut socket), json!({"id":3,"op":"get","key":"p"}));
        send(&mut socket, json!({"id":3,"status":"no_data","key":"p"}));
        until_closed(&mut socket);
    });
    steel
        .eval(&format!("(lab/connect! \"{address}\")"))
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    assert_eq!(sender.request(Request::Describe).unwrap(), 2);
    assert_eq!(sender.request(Request::Get { key: "p".into() }).unwrap(), 3);
    pump(&mut connection, &mut plots, |c, _| {
        c.journal
            .iter()
            .any(|e| e.connection == 2 && e.request_id() == Some(3))
    });
    close(&mut connection, &mut plots);
    assert!(plots.series.is_empty());
    for id in 2..=7 {
        assert_eq!(
            connection
                .journal
                .iter()
                .filter(|e| e.connection == 1 && e.request_id() == Some(id))
                .count(),
            1
        );
    }
    server.join().unwrap();
}

fn observation(key: &str, value: serde_json::Value, time: u64) -> serde_json::Value {
    json!({"event":"measurement","status":"ok","key":key,"value":value,"unit":"","completed_at_unix_ms":time})
}

// Evaluate validation and plot commands on a real separate Engine thread.
// Raw request admission below exercises delayed/out-of-order journal delivery;
// confirmed Steel results are tested in steel_results.rs.
type EvalReply = mpsc::SyncSender<Result<Vec<String>, String>>;
struct SteelThread {
    tx: mpsc::SyncSender<(String, EvalReply)>,
    handle: Option<thread::JoinHandle<()>>,
}
impl SteelThread {
    fn new(sender: ControlSender) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(String, EvalReply)>(1);
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut engine = Engine::new();
            register_plot_functions(&mut engine, sender.clone());
            register_lab_functions(&mut engine, sender);
            ready_tx.send(()).unwrap();
            while let Ok((source, reply)) = rx.recv() {
                let result = engine
                    .compile_and_run_raw_program(source)
                    .map(|values| values.into_iter().map(|v| v.to_string()).collect())
                    .map_err(|e| e.to_string());
                reply.send(result).unwrap();
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        Self {
            tx,
            handle: Some(handle),
        }
    }
    fn eval(&self, source: &str) -> Result<Vec<String>, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx.send((source.into(), tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(15))
            .expect("Steel blocked awaiting GUI or server")
    }
    fn stop(mut self) {
        drop(self.tx);
        self.handle.take().unwrap().join().unwrap();
    }
}

#[test]
fn numeric_routing_metadata_shared_observations_and_discontinuities() {
    let mut plots = PlotState::default();
    add(&mut plots, "Heater", "opaque/device/heater");
    add(&mut plots, "Copy", "opaque/device/heater");
    add(&mut plots, "Sample", "sample");
    let exact = sample(
        "opaque/device/heater",
        ScalarValue::Integer(9_007_199_254_740_993),
        "degC",
        1_700_000_000_123,
    );
    assert!(plots.measurement(exact.clone()).is_empty());
    assert!(plots.series["Sample"].points.is_empty());
    let a = &plots.series["Heater"].points[0];
    let b = &plots.series["Copy"].points[0];
    assert_eq!(a.xy[0], 1_700_000_000.123);
    assert_eq!(a.measurement.as_deref(), Some(&exact));
    assert!(Arc::ptr_eq(
        a.measurement.as_ref().unwrap(),
        b.measurement.as_ref().unwrap()
    ));
    plots.measurement(Measurement {
        key: exact.key.clone(),
        completed_at_unix_ms: 1_700_000_001_000,
        reading: Reading::Error("Empty response".into()),
    });
    assert_eq!(plots.series["Heater"].points.len(), 1);
    assert_eq!(
        plots.series["Heater"].error.as_deref(),
        Some("Empty response")
    );
    plots.measurement(sample(
        &exact.key,
        ScalarValue::Float(2.5),
        "degC",
        1_700_000_002_000,
    ));
    assert_eq!(plots.series["Heater"].segments(), vec![0..1, 1..2]);
    assert!(plots.series["Heater"].error.is_none());
    plots
        .apply(PlotCommand::Bind {
            name: "Heater".into(),
            key: "sample".into(),
        })
        .unwrap();
    plots.measurement(sample(
        "sample",
        ScalarValue::Float(3.0),
        "degC",
        1_700_000_003_000,
    ));
    assert_eq!(plots.series["Heater"].segments(), vec![0..1, 1..2, 2..3]);
    assert_eq!(plots.series["Sample"].points.len(), 1);
    plots
        .apply(PlotCommand::Unbind {
            name: "Heater".into(),
        })
        .unwrap();
    plots.measurement(sample(
        "sample",
        ScalarValue::Float(4.0),
        "degC",
        1_700_000_004_000,
    ));
    assert_eq!(plots.series["Heater"].points.len(), 3);
    plots
        .apply(PlotCommand::Remove {
            name: "Heater".into(),
        })
        .unwrap();
    assert!(!plots.series.contains_key("Heater"));
}

#[test]
fn unit_mismatch_skips_points_and_keeps_historical_units() {
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    plots.measurement(sample("a", ScalarValue::Float(1.0), "degC", 1000));
    let errors = plots.measurement(sample("a", ScalarValue::Float(273.0), "K", 2000));
    assert_eq!(errors.len(), 1);
    assert!(
        plots.series["A"]
            .error
            .as_ref()
            .unwrap()
            .contains("unit mismatch")
    );
    assert_eq!(plots.series["A"].points.len(), 1);
    plots
        .apply(PlotCommand::Add {
            name: "A".into(),
            unit: "K".into(),
        })
        .unwrap();
    plots.measurement(sample("a", ScalarValue::Float(274.0), "K", 3000));
    assert_eq!(plots.series["A"].segments(), vec![0..1, 1..2]);
    assert_eq!(plots.series["A"].sample_unit(0), "degC");
    assert_eq!(plots.series["A"].sample_unit(1), "K");
    assert!(plots.series["A"].error.is_none());
}

#[test]
fn measurement_history_is_bounded_and_manual_commands_keep_behavior() {
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    for i in 0..MAX_POINTS + 3 {
        plots.measurement(sample(
            "a",
            ScalarValue::Integer(i as i64),
            "degC",
            i as u64,
        ));
    }
    assert_eq!(plots.series["A"].points.len(), MAX_POINTS);
    assert_eq!(
        plots.series["A"].points[0]
            .measurement
            .as_ref()
            .unwrap()
            .completed_at_unix_ms,
        3
    );
    plots
        .apply(PlotCommand::Point {
            name: "manual".into(),
            time: 1.0,
            value: 2.0,
        })
        .unwrap();
    assert_eq!(plots.series["manual"].points[0].xy, [1.0, 2.0]);
    plots
        .apply(PlotCommand::Add {
            name: "manual".into(),
            unit: "%".into(),
        })
        .unwrap();
    assert_eq!(plots.series["manual"].points.len(), 1);
    plots
        .apply(PlotCommand::Clear {
            name: "manual".into(),
        })
        .unwrap();
    assert!(plots.series["manual"].points.is_empty());
    plots
        .apply(PlotCommand::Clear {
            name: "missing".into(),
        })
        .unwrap();
}

#[test]
fn gui_command_errors_and_journal_eviction_are_visible_and_bounded() {
    let mut connection = Integration::default();
    let sender = connection.sender();
    let mut plots = PlotState::default();
    for i in 0..CONTROL_CAPACITY {
        sender
            .plot(PlotCommand::Bind {
                name: format!("missing-{i}"),
                key: "a".into(),
            })
            .unwrap();
    }
    assert!(
        sender
            .plot(PlotCommand::Clear { name: "a".into() })
            .unwrap_err()
            .contains("full")
    );
    connection.update(&mut plots);
    assert_eq!(connection.journal.len(), JOURNAL_CAPACITY);
    assert_eq!(
        connection.evicted,
        (CONTROL_CAPACITY - JOURNAL_CAPACITY) as u64
    );
    assert!(
        connection
            .journal
            .back()
            .unwrap()
            .text()
            .contains("Unknown curve")
    );
    close(&mut connection, &mut plots);
    assert!(sender.connect("127.0.0.1:0".parse().unwrap()).is_err());
    assert!(sender.request(Request::Ping).is_err());
}

#[test]
fn steel_controls_one_socket_async_requests_types_bindings_and_subscriptions() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let steel = SteelThread::new(connection.sender());
    let (gate_tx, gate_rx) = mpsc::sync_channel(1);
    let (seen_tx, seen_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        assert_eq!(
            read(&mut socket),
            json!({"id":2,"op":"subscribe","keys":["a","b"]})
        );
        seen_tx.send(()).unwrap();
        gate_rx.recv_timeout(WAIT).unwrap();
        send(&mut socket, json!({"id":2,"status":"ok","keys":["a","b"]}));
        for (key, value) in [("a", 12.5), ("b", 25.0)] {
            send(
                &mut socket,
                json!({"event":"measurement","status":"ok","key":key,"value":value,"unit":"degC","completed_at_unix_ms":1234}),
            );
        }
        assert_eq!(read(&mut socket), json!({"id":3,"op":"get","key":"a"}));
        assert_eq!(
            read(&mut socket),
            json!({"id":4,"op":"set","key":"power","value":20})
        );
        assert_eq!(
            read(&mut socket),
            json!({"id":5,"op":"set","key":"reference","value":20.0})
        );
        seen_tx.send(()).unwrap();
        gate_rx.recv_timeout(WAIT).unwrap();
        send(&mut socket, json!({"id":5,"status":"ok"}));
        send(
            &mut socket,
            json!({"id":4,"status":"error","error":"outcome_unknown"}),
        );
        send(&mut socket, json!({"id":3,"status":"no_data","key":"a"}));
        assert_eq!(
            read(&mut socket),
            json!({"id":6,"op":"unsubscribe","keys":["a"]})
        );
        send(
            &mut socket,
            json!({"event":"measurement","status":"ok","key":"a","value":13,"unit":"degC","completed_at_unix_ms":2000}),
        );
        seen_tx.send(()).unwrap();
        gate_rx.recv_timeout(WAIT).unwrap();
        send(&mut socket, json!({"id":6,"status":"ok","keys":["b"]}));
        send(
            &mut socket,
            json!({"event":"measurement","status":"ok","key":"b","value":26,"unit":"degC","completed_at_unix_ms":2000}),
        );
        assert_eq!(
            read(&mut socket),
            json!({"id":7,"op":"subscribe","keys":["a"]})
        );
        send(&mut socket, json!({"id":7,"status":"ok","keys":["a","b"]}));
        send(
            &mut socket,
            json!({"event":"measurement","status":"ok","key":"a","value":14,"unit":"degC","completed_at_unix_ms":3000}),
        );
        until_closed(&mut socket); // includes proof that outcome_unknown was not retried
    });
    assert_eq!(
        steel
            .eval(&format!("(lab/connect! \"{address}\")"))
            .unwrap(),
        vec!["#true"]
    );
    assert!(steel.eval("(lab/get \"a\")").is_err());
    assert!(
        steel
            .eval(&format!("(lab/connect! \"{address}\")"))
            .is_err()
    );
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    assert_eq!(steel.eval("(lab/status)").unwrap(), vec!["\"Ready\""]);
    steel.eval("(plot/add! \"A\" \"degC\") (plot/add! \"B\" \"degC\") (plot/add! \"Copy\" \"degC\") (plot/bind! \"A\" \"a\") (plot/bind! \"B\" \"b\") (plot/bind! \"Copy\" \"a\")").unwrap();
    assert_eq!(
        connection
            .sender()
            .request(Request::Subscribe {
                keys: vec!["a".into(), "b".into()]
            })
            .unwrap(),
        2
    );
    seen_rx.recv_timeout(WAIT).unwrap();
    connection.update(&mut plots);
    assert!(connection.subscriptions.is_empty()); // admission is not ACK
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |_, p| {
        p.series["A"].points.len() == 1 && p.series["B"].points.len() == 1
    });
    assert_eq!(plots.series["Copy"].points.len(), 1);
    assert_eq!(plots.series["A"].points[0].xy, [1.234, 12.5]);
    assert!(
        steel
            .eval("(lab/set! \"power\" 9223372036854775808)")
            .is_err()
    );
    for (id, request) in [
        (3, Request::Get { key: "a".into() }),
        (
            4,
            Request::Set {
                key: "power".into(),
                value: ScalarValue::Integer(20),
            },
        ),
        (
            5,
            Request::Set {
                key: "reference".into(),
                value: ScalarValue::Float(20.0),
            },
        ),
    ] {
        assert_eq!(connection.sender().request(request).unwrap(), id);
    }
    assert_eq!(steel.eval("(+ 1 2)").unwrap(), vec!["3"]);
    seen_rx.recv_timeout(WAIT).unwrap();
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.journal
            .iter()
            .filter(|e| matches!(e.request_id(), Some(3..=5)))
            .count()
            == 3
    });
    assert!(
        connection
            .journal
            .iter()
            .any(|e| e.text().contains("outcome_unknown"))
    );
    assert_eq!(plots.series["A"].points.len(), 1); // get results are not streamed points
    assert_eq!(
        connection
            .sender()
            .request(Request::Unsubscribe {
                keys: vec!["a".into()]
            })
            .unwrap(),
        6
    );
    seen_rx.recv_timeout(WAIT).unwrap();
    pump(&mut connection, &mut plots, |_, p| {
        p.series["A"].points.len() == 2
    });
    assert!(connection.subscriptions.contains("a")); // queued measurement before ACK
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, p| {
        !c.subscriptions.contains("a") && p.series["B"].points.len() == 2
    });
    steel
        .eval("(plot/unbind! \"Copy\") (plot/remove! \"B\")")
        .unwrap();
    connection.update(&mut plots);
    assert!(connection.subscriptions.contains("b"));
    assert_eq!(
        connection
            .sender()
            .request(Request::Subscribe {
                keys: vec!["a".into()]
            })
            .unwrap(),
        7
    );
    pump(&mut connection, &mut plots, |_, p| {
        p.series["A"].points.len() == 3
    });
    assert_eq!(plots.series["Copy"].points.len(), 2);
    assert_eq!(plots.series["A"].segments(), vec![0..2, 2..3]);
    steel
        .eval("(plot/point! \"manual\" 1.0 2.0) (plot/add! \"manual\" \"%\")")
        .unwrap();
    connection.update(&mut plots);
    assert_eq!(plots.series["manual"].points[0].xy, [1.0, 2.0]);
    steel
        .eval("(plot/clear! \"manual\") (plot/bind! \"absent\" \"a\")")
        .unwrap();
    connection.update(&mut plots);
    assert!(plots.series["manual"].points.is_empty());
    assert!(
        connection
            .journal
            .back()
            .unwrap()
            .text()
            .contains("Unknown curve")
    );
    steel.eval("(lab/disconnect!)").unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Stopped
    });
    assert!(connection.subscriptions.is_empty());
    server.join().unwrap();
    steel.stop();
}

#[test]
fn frame_budget_keeps_remaining_observations_for_later_frames() {
    let (sent_tx, sent_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        read(&mut socket);
        send(&mut socket, json!({"id":2,"status":"ok","keys":["a"]}));
        for time in 0..600 {
            send(
                &mut socket,
                json!({"event":"measurement","status":"ok","key":"a","value":time,"unit":"degC","completed_at_unix_ms":time}),
            );
        }
        sent_tx.send(()).unwrap();
        until_closed(&mut socket);
    });
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    connection.sender().connect(address).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    connection
        .sender()
        .request(Request::Subscribe {
            keys: vec!["a".into()],
        })
        .unwrap();
    sent_rx.recv_timeout(WAIT).unwrap();
    let deadline = Instant::now() + WAIT;
    while plots.series["A"].points.len() < 600 {
        let before = plots.series["A"].points.len();
        assert!(connection.update(&mut plots) <= EVENTS_PER_FRAME);
        assert!(plots.series["A"].points.len() - before <= EVENTS_PER_FRAME);
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(plots.series["A"].points.len(), 600);
    close(&mut connection, &mut plots);
    server.join().unwrap();
}

#[test]
fn read_errors_failed_subscription_and_runtime_timeout_remain_visible() {
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        read(&mut socket);
        send(&mut socket, json!({"id":2,"status":"ok","keys":["a"]}));
        send(
            &mut socket,
            json!({"event":"measurement","status":"ok","key":"a","value":1,"unit":"degC","completed_at_unix_ms":1000}),
        );
        release_rx.recv_timeout(WAIT).unwrap();
        send(
            &mut socket,
            json!({"event":"measurement","status":"error","key":"a","error":"Empty response","completed_at_unix_ms":2000}),
        );
        assert_eq!(read(&mut socket)["op"], "subscribe");
        send(
            &mut socket,
            json!({"id":3,"status":"error","error":"unknown_key","key":"b"}),
        );
        assert_eq!(read(&mut socket)["op"], "set");
        send(
            &mut socket,
            json!({"id":4,"status":"error","error":"runtime_timeout"}),
        );
        release_rx.recv_timeout(WAIT).unwrap();
        send(
            &mut socket,
            json!({"event":"measurement","status":"ok","key":"a","value":3,"unit":"degC","completed_at_unix_ms":3000}),
        );
        until_closed(&mut socket);
    });
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    let sender = connection.sender();
    sender.connect(address).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    sender
        .request(Request::Subscribe {
            keys: vec!["a".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |_, p| {
        p.series["A"].points.len() == 1
    });
    release_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.signal_errors.contains_key("a")
    });
    assert_eq!(plots.series["A"].points.len(), 1);
    assert_eq!(plots.series["A"].error.as_deref(), Some("Empty response"));
    sender
        .request(Request::Subscribe {
            keys: vec!["b".into()],
        })
        .unwrap();
    sender
        .request(Request::Set {
            key: "power".into(),
            value: ScalarValue::Integer(20),
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.journal.iter().any(|e| e.request_id() == Some(4))
    });
    assert_eq!(
        connection
            .subscriptions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert!(
        connection
            .journal
            .iter()
            .any(|e| e.text().contains("runtime_timeout"))
    );
    release_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |_, p| {
        p.series["A"].points.len() == 2
    });
    assert_eq!(plots.series["A"].segments(), vec![0..1, 1..2]);
    assert!(connection.signal_errors.is_empty());
    close(&mut connection, &mut plots);
    server.join().unwrap();
}

#[test]
fn stream_failures_stop_points_and_finalize_set_without_retry() {
    for error in [
        Some("observation_data_lost"),
        Some("observation_stream_closed"),
        None,
    ] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            read(&mut socket);
            send(&mut socket, json!({"id":2,"status":"ok","keys":["a"]}));
            send(
                &mut socket,
                json!({"event":"measurement","status":"ok","key":"a","value":1,"unit":"degC","completed_at_unix_ms":1000}),
            );
            assert_eq!(
                read(&mut socket),
                json!({"id":3,"op":"set","key":"power","value":20})
            );
            if let Some(error) = error {
                send(&mut socket, json!({"status":"error","error":error}));
                until_closed(&mut socket);
            }
        });
        let mut connection = Integration::default();
        let mut plots = PlotState::default();
        add(&mut plots, "A", "a");
        let sender = connection.sender();
        sender.connect(address).unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Ready
        });
        sender
            .request(Request::Subscribe {
                keys: vec!["a".into()],
            })
            .unwrap();
        pump(&mut connection, &mut plots, |_, p| {
            p.series["A"].points.len() == 1
        });
        sender
            .request(Request::Set {
                key: "power".into(),
                value: ScalarValue::Integer(20),
            })
            .unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Stopped
        });
        assert!(connection.stream_error.is_some());
        assert_eq!(plots.series["A"].points.len(), 1);
        let finals: Vec<_> = connection
            .journal
            .iter()
            .filter(|e| e.request_id() == Some(3))
            .collect();
        assert_eq!(finals.len(), 1);
        assert!(matches!(
            finals[0].result,
            JournalResult::RequestFailed {
                failure: RequestFailure::OutcomeUnknown,
                ..
            }
        ));
        if let Some(error) = error {
            assert!(connection.journal.iter().any(|e| e.text().contains(error)));
        }
        let persistent = connection.stream_error.clone();
        for _ in 0..201 {
            sender
                .plot(PlotCommand::Bind {
                    name: "missing".into(),
                    key: "a".into(),
                })
                .unwrap();
        }
        connection.update(&mut plots);
        assert_eq!(connection.stream_error, persistent);
        assert!(connection.evicted > 0);
        assert!(sender.request(Request::Ping).is_err());
        server.join().unwrap();
    }
}

#[test]
fn disconnect_then_reconnect_preserves_bindings_but_not_subscriptions_or_lines() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    let sender = connection.sender();
    for number in 1..=2 {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            // The first application command must be our explicit ping, not an
            // automatically restored subscribe from the preceding connection.
            assert_eq!(read(&mut socket), json!({"id":2,"op":"ping"}));
            send(&mut socket, json!({"id":2,"status":"ok"}));
            assert_eq!(
                read(&mut socket),
                json!({"id":3,"op":"subscribe","keys":["a"]})
            );
            send(&mut socket, json!({"id":3,"status":"ok","keys":["a"]}));
            send(
                &mut socket,
                json!({"event":"measurement","status":"ok","key":"a","value":number,"unit":"degC","completed_at_unix_ms":number*1000}),
            );
            until_closed(&mut socket);
        });
        sender.connect(address).unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Ready
        });
        assert_eq!(connection.connection, number);
        assert!(connection.subscriptions.is_empty());
        assert_eq!(sender.request(Request::Ping).unwrap(), 2);
        sender
            .request(Request::Subscribe {
                keys: vec!["a".into()],
            })
            .unwrap();
        pump(&mut connection, &mut plots, |_, p| {
            p.series["A"].points.len() == number as usize
        });
        sender.disconnect().unwrap();
        assert!(sender.connect(address).is_err());
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Stopped
        });
        server.join().unwrap();
    }
    assert_eq!(plots.series["A"].segments(), vec![0..1, 1..2]);
    assert!(
        connection
            .journal
            .iter()
            .any(|e| e.connection == 1 && e.request_id() == Some(3))
    );
    assert!(
        connection
            .journal
            .iter()
            .any(|e| e.connection == 2 && e.request_id() == Some(3))
    );
}

#[test]
fn connection_failure_and_close_during_handshake_do_not_block_gui() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    connection
        .sender()
        .connect("127.0.0.1:0".parse().unwrap())
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Stopped
    });
    assert!(connection.reason.as_ref().unwrap().contains("Connect"));
    let (seen_tx, seen_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        assert_eq!(read(&mut socket)["op"], "hello");
        seen_tx.send(()).unwrap();
        until_closed(&mut socket); // no hello reply
    });
    connection.sender().connect(address).unwrap();
    connection.update(&mut plots);
    seen_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(connection.phase(), Phase::Connecting);
    close(&mut connection, &mut plots);
    assert_eq!(connection.phase(), Phase::Stopped);
    server.join().unwrap();
}

#[test]
fn event_overflow_is_reported_even_before_gui_drains_events() {
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        read(&mut socket);
        send(&mut socket, json!({"id":2,"status":"ok","keys":["a"]}));
        release_rx.recv_timeout(WAIT).unwrap();
        let frames = (0..20).map(|time| format!("{}\n", json!({"event":"measurement","status":"ok","key":"a","value":1,"unit":"degC","completed_at_unix_ms":time}))).collect::<String>();
        socket.get_mut().write_all(frames.as_bytes()).unwrap();
        until_closed(&mut socket);
    });
    let mut connection = Integration::new(ClientOptions {
        event_capacity: 1,
        ..Default::default()
    });
    let mut plots = PlotState::default();
    add(&mut plots, "A", "a");
    connection.sender().connect(address).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    connection
        .sender()
        .request(Request::Subscribe {
            keys: vec!["a".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.subscriptions.contains("a")
    });
    release_tx.send(()).unwrap();
    // Server finishes only after the client closes on overflow, before any GUI
    // reads. No scheduler-dependent sleep is used to manufacture a full queue.
    server.join().unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Stopped
    });
    assert_eq!(plots.series["A"].points.len(), 0);
    assert!(
        connection
            .stream_error
            .as_ref()
            .unwrap()
            .contains("EventQueueOverflow")
    );
}

#[test]
fn mixed_scalar_requests_and_steel_plot_controls_share_one_socket() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let steel = SteelThread::new(connection.sender());
    let (gate_tx, gate_rx) = mpsc::sync_channel(1);
    let (seen_tx, seen_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        assert_eq!(
            read(&mut socket),
            json!({"id":2,"op":"subscribe","keys":["integer","float","enabled","fault"]})
        );
        send(
            &mut socket,
            json!({"id":2,"status":"ok","keys":["integer","float","enabled","fault"]}),
        );
        // Partial and coalesced frames carry all three scalar types.
        let frames = [
            ("integer", json!(9_007_199_254_740_993i64)),
            ("float", json!(20.0)),
            ("enabled", json!(true)),
            ("fault", json!(false)),
            ("unsubscribed", json!(true)),
        ]
        .into_iter()
        .map(|(key, value)| format!("{}\n", observation(key, value, 1234)))
        .collect::<String>();
        socket
            .get_mut()
            .write_all(&frames.as_bytes()[..19])
            .unwrap();
        socket
            .get_mut()
            .write_all(&frames.as_bytes()[19..])
            .unwrap();
        for (index, (key, value)) in [
            ("enabled", json!(true)),
            ("enabled", json!(false)),
            ("enabled", json!(true)),
            ("enabled", json!(false)),
            ("power", json!(20)),
            ("reference", json!(20.0)),
            ("exact", json!(9_007_199_254_740_993i64)),
        ]
        .into_iter()
        .enumerate()
        {
            let request = read(&mut socket);
            assert_eq!(
                request,
                json!({"id":index+3,"op":"set","key":key,"value":value})
            );
            assert_eq!(request["value"].to_string(), value.to_string());
        }
        assert_eq!(
            read(&mut socket),
            json!({"id":10,"op":"get","key":"enabled"})
        );
        seen_tx.send(()).unwrap();
        // No response is released until all requests have been admitted.
        gate_rx.recv_timeout(WAIT).unwrap();
        send(
            &mut socket,
            json!({"id":10,"status":"ok","key":"enabled","value":false,"unit":"","completed_at_unix_ms":5678}),
        );
        for id in (4..=9).rev() {
            send(&mut socket, json!({"id":id,"status":"ok"}));
        }
        send(
            &mut socket,
            json!({"id":3,"status":"error","error":"outcome_unknown"}),
        );
        until_closed(&mut socket); // Any retry, including Boolean set, fails the test.
    });
    steel
        .eval(&format!("(lab/connect! \"{address}\")"))
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    steel.eval(r#"(plot/add! "Integer" "") (plot/bind! "Integer" "integer") (plot/add! "Float" "") (plot/bind! "Float" "float") (plot/add! "Wrong" "") (plot/point! "Wrong" 0.0 42.0) (plot/bind! "Wrong" "enabled")"#).unwrap();
    assert_eq!(
        connection
            .sender()
            .request(Request::Subscribe {
                keys: ["integer", "float", "enabled", "fault"]
                    .map(str::to_owned)
                    .to_vec()
            })
            .unwrap(),
        2
    );
    pump(&mut connection, &mut plots, |c, p| {
        c.boolean_signals.len() == 2 && p.series["Float"].points.len() == 1
    });
    assert_eq!(connection.phase(), Phase::Ready);
    assert_eq!(
        connection.boolean_signals["enabled"],
        BooleanState {
            value: true,
            completed_at_unix_ms: 1234
        }
    );
    assert!(!connection.boolean_signals["fault"].value);
    assert!(!connection.boolean_signals.contains_key("unsubscribed"));
    steel
        .eval(r#"(plot/add! "Late binding" "") (plot/bind! "Late binding" "enabled")"#)
        .unwrap();
    connection.update(&mut plots);
    assert!(
        plots.series["Late binding"]
            .error
            .as_ref()
            .unwrap()
            .contains("Boolean")
    );
    assert!(plots.series["Late binding"].points.is_empty());
    assert_eq!(
        plots.series["Integer"].points[0]
            .measurement
            .as_ref()
            .unwrap()
            .reading,
        Reading::Value {
            value: ScalarValue::Integer(9_007_199_254_740_993),
            unit: String::new()
        }
    );
    assert_eq!(plots.series["Float"].points[0].xy, [1.234, 20.0]);
    assert_eq!(plots.series["Wrong"].points.len(), 1);
    assert_eq!(plots.series["Wrong"].points[0].xy, [0.0, 42.0]);
    assert!(
        plots.series["Wrong"]
            .error
            .as_ref()
            .unwrap()
            .contains("Boolean")
    );
    for expression in [
        r#"(lab/set! "enabled" "true")"#,
        r#"(lab/set! "enabled" '(#t))"#,
        r#"(lab/set! "power" 9223372036854775808)"#,
    ] {
        assert!(steel.eval(expression).is_err());
    }
    steel.eval(r#"(define enabled "enabled")"#).unwrap();
    for (key, value) in [
        ("enabled", ScalarValue::Boolean(true)),
        ("enabled", ScalarValue::Boolean(false)),
        ("enabled", ScalarValue::Boolean(true)),
        ("enabled", ScalarValue::Boolean(false)),
        ("power", ScalarValue::Integer(20)),
        ("reference", ScalarValue::Float(20.0)),
        ("exact", ScalarValue::Integer(9007199254740993)),
    ] {
        connection
            .sender()
            .request(Request::Set {
                key: key.into(),
                value,
            })
            .unwrap();
    }
    assert_eq!(
        connection
            .sender()
            .request(Request::Get {
                key: "enabled".into()
            })
            .unwrap(),
        10
    );
    assert_eq!(steel.eval("(+ 1 2)").unwrap(), vec!["3"]);
    seen_rx.recv_timeout(WAIT).unwrap();
    assert!(!has_result(&connection, 3));
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        (3..=10).all(|id| has_result(c, id))
    });
    assert!(connection.boolean_signals["enabled"].value); // get is a journal result, not a stream update.
    assert_eq!(
        connection.boolean_signals["enabled"].completed_at_unix_ms,
        1234
    );
    let get = connection
        .journal
        .iter()
        .find(|e| e.request_id() == Some(10))
        .unwrap();
    assert!(get.text().contains("value: false"));
    assert!(get.text().contains("5678"));
    let results: Vec<_> = connection
        .journal
        .iter()
        .filter(|e| e.request_id() == Some(3))
        .collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].text().contains("outcome_unknown"));
    assert_eq!(connection.phase(), Phase::Ready);
    close(&mut connection, &mut plots);
    assert!(connection.boolean_signals.is_empty());
    server.join().unwrap();
    steel.stop();
}

#[test]
fn boolean_read_error_recovery_and_unsubscribe_update_current_states_after_ack() {
    let (gate_tx, gate_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        assert_eq!(read(&mut socket)["id"], 2);
        send(
            &mut socket,
            json!({"id":2,"status":"ok","keys":["enabled","fault"]}),
        );
        send(&mut socket, observation("enabled", json!(true), 1000));
        send(&mut socket, observation("fault", json!(false), 1000));
        assert_eq!(read(&mut socket), json!({"id":3,"op":"ping"}));
        send(
            &mut socket,
            json!({"event":"measurement","status":"error","key":"enabled","error":"Boolean read failed","completed_at_unix_ms":2000}),
        );
        send(&mut socket, json!({"id":3,"status":"ok"}));
        assert_eq!(read(&mut socket), json!({"id":4,"op":"ping"}));
        send(&mut socket, observation("enabled", json!(false), 3000));
        send(&mut socket, json!({"id":4,"status":"ok"}));
        assert_eq!(
            read(&mut socket),
            json!({"id":5,"op":"unsubscribe","keys":["enabled"]})
        );
        send(&mut socket, observation("enabled", json!(true), 4000));
        gate_rx.recv_timeout(WAIT).unwrap();
        send(&mut socket, json!({"id":5,"status":"ok","keys":["fault"]}));
        send(&mut socket, observation("enabled", json!(false), 5000)); // ignored after ACK
        assert_eq!(
            read(&mut socket),
            json!({"id":6,"op":"unsubscribe","keys":["fault"]})
        );
        send(
            &mut socket,
            json!({"id":6,"status":"error","error":"runtime_timeout"}),
        );
        until_closed(&mut socket);
    });
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let sender = connection.sender();
    sender.connect(address).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    sender
        .request(Request::Subscribe {
            keys: vec!["enabled".into(), "fault".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.boolean_signals.len() == 2
    });
    sender.request(Request::Ping).unwrap();
    pump(&mut connection, &mut plots, |c, _| has_result(c, 3));
    assert!(!connection.boolean_signals.contains_key("enabled"));
    assert!(connection.boolean_signals.contains_key("fault"));
    assert_eq!(connection.signal_errors["enabled"], "Boolean read failed");
    sender.request(Request::Ping).unwrap();
    pump(&mut connection, &mut plots, |c, _| has_result(c, 4));
    assert_eq!(
        connection.boolean_signals["enabled"],
        BooleanState {
            value: false,
            completed_at_unix_ms: 3000
        }
    );
    assert!(!connection.signal_errors.contains_key("enabled"));
    sender
        .request(Request::Unsubscribe {
            keys: vec!["enabled".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.boolean_signals["enabled"].completed_at_unix_ms == 4000
    });
    assert!(connection.subscriptions.contains("enabled"));
    gate_tx.send(()).unwrap();
    pump(&mut connection, &mut plots, |c, _| has_result(c, 5));
    assert!(!connection.boolean_signals.contains_key("enabled"));
    sender
        .request(Request::Unsubscribe {
            keys: vec!["fault".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| has_result(c, 6));
    assert_eq!(connection.boolean_signals.len(), 1);
    assert!(connection.boolean_signals.contains_key("fault"));
    assert!(connection.subscriptions.contains("fault")); // failed unsubscribe retains state
    close(&mut connection, &mut plots);
    assert!(connection.boolean_signals.is_empty());
    server.join().unwrap();
}

#[test]
fn boolean_disconnect_reconnect_requires_fresh_subscription_and_measurement() {
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let sender = connection.sender();
    for number in 1..=2 {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            assert_eq!(read(&mut socket), json!({"id":2,"op":"ping"}));
            send(&mut socket, json!({"id":2,"status":"ok"}));
            assert_eq!(
                read(&mut socket),
                json!({"id":3,"op":"subscribe","keys":["enabled"]})
            );
            send(
                &mut socket,
                json!({"id":3,"status":"ok","keys":["enabled"]}),
            );
            send(
                &mut socket,
                observation("enabled", json!(number == 1), number * 1000),
            );
            until_closed(&mut socket);
        });
        sender.connect(address).unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Ready
        });
        assert!(connection.boolean_signals.is_empty());
        assert!(connection.subscriptions.is_empty());
        assert_eq!(connection.connection, number);
        sender.request(Request::Ping).unwrap();
        sender
            .request(Request::Subscribe {
                keys: vec!["enabled".into()],
            })
            .unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.boolean_signals.len() == 1
        });
        assert_eq!(
            connection.boolean_signals["enabled"],
            BooleanState {
                value: number == 1,
                completed_at_unix_ms: number * 1000
            }
        );
        sender.disconnect().unwrap();
        connection.update(&mut plots);
        assert!(connection.boolean_signals.is_empty());
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Stopped
        });
        server.join().unwrap();
    }
}

#[test]
fn boolean_stream_loss_clears_state_and_preserves_pending_set_uncertainty() {
    for error in [
        Some("observation_data_lost"),
        Some("observation_stream_closed"),
        None,
    ] {
        let (address, server) = mock(move |mut socket| {
            hello(&mut socket);
            read(&mut socket);
            send(
                &mut socket,
                json!({"id":2,"status":"ok","keys":["enabled"]}),
            );
            send(&mut socket, observation("enabled", json!(true), 1000));
            assert_eq!(
                read(&mut socket),
                json!({"id":3,"op":"set","key":"enabled","value":false})
            );
            if let Some(error) = error {
                send(&mut socket, json!({"status":"error","error":error}));
                until_closed(&mut socket);
            }
        });
        let mut connection = Integration::default();
        let mut plots = PlotState::default();
        let sender = connection.sender();
        sender.connect(address).unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Ready
        });
        sender
            .request(Request::Subscribe {
                keys: vec!["enabled".into()],
            })
            .unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.boolean_signals.len() == 1
        });
        sender
            .request(Request::Set {
                key: "enabled".into(),
                value: ScalarValue::Boolean(false),
            })
            .unwrap();
        pump(&mut connection, &mut plots, |c, _| {
            c.phase() == Phase::Stopped
        });
        assert!(connection.boolean_signals.is_empty());
        assert!(connection.stream_error.is_some());
        let results: Vec<_> = connection
            .journal
            .iter()
            .filter(|e| e.request_id() == Some(3))
            .collect();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].result,
            JournalResult::RequestFailed {
                failure: RequestFailure::OutcomeUnknown,
                ..
            }
        ));
        if let Some(error) = error {
            assert!(connection.journal.iter().any(|e| e.text().contains(error)));
        }
        server.join().unwrap();
    }
}

#[test]
fn boolean_queue_overflow_clears_current_state_before_draining_backlog() {
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (address, server) = mock(move |mut socket| {
        hello(&mut socket);
        read(&mut socket);
        send(
            &mut socket,
            json!({"id":2,"status":"ok","keys":["enabled"]}),
        );
        send(&mut socket, observation("enabled", json!(true), 1000));
        release_rx.recv_timeout(WAIT).unwrap();
        let frames = (0..20)
            .map(|time| format!("{}\n", observation("enabled", json!(false), time + 2000)))
            .collect::<String>();
        socket.get_mut().write_all(frames.as_bytes()).unwrap();
        until_closed(&mut socket);
    });
    let mut connection = Integration::new(ClientOptions {
        event_capacity: 2,
        ..Default::default()
    });
    let mut plots = PlotState::default();
    let sender = connection.sender();
    sender.connect(address).unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Ready
    });
    sender
        .request(Request::Subscribe {
            keys: vec!["enabled".into()],
        })
        .unwrap();
    pump(&mut connection, &mut plots, |c, _| {
        c.boolean_signals.len() == 1
    });
    release_tx.send(()).unwrap();
    server.join().unwrap(); // overflow occurred without GUI polling
    connection.update(&mut plots);
    assert!(connection.boolean_signals.is_empty());
    assert!(
        connection
            .stream_error
            .as_ref()
            .unwrap()
            .contains("EventQueueOverflow")
    );
    pump(&mut connection, &mut plots, |c, _| {
        c.phase() == Phase::Stopped
    });
}

#[test]
fn boolean_numeric_curve_rejection_keeps_history_and_breaks_the_next_segment() {
    let mut plots = PlotState::default();
    add(&mut plots, "A", "signal");
    plots.measurement(sample("signal", ScalarValue::Integer(20), "degC", 1000));
    let errors = plots.measurement(sample("signal", ScalarValue::Boolean(true), "", 2000));
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Boolean")); // type error takes precedence over unit mismatch
    assert_eq!(plots.series["A"].points.len(), 1);
    assert!(
        plots
            .measurement(sample("signal", ScalarValue::Boolean(false), "", 3000))
            .is_empty()
    );
    plots.measurement(sample("signal", ScalarValue::Float(21.0), "degC", 4000));
    assert_eq!(plots.series["A"].segments(), vec![0..1, 1..2]);
    assert!(plots.series["A"].error.is_none());
}
