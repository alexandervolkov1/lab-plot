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

// Evaluate on a real separate Engine thread. A withheld server ACK cannot be
// released until eval returns, so the async assertions don't rely on timing.
type EvalReply = mpsc::SyncSender<Result<Vec<String>, String>>;
struct SteelThread {
    tx: mpsc::SyncSender<(String, EvalReply)>,
    handle: Option<thread::JoinHandle<()>>,
}
impl SteelThread {
    fn new(sender: ControlSender) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(String, EvalReply)>(1);
        let handle = thread::spawn(move || {
            let mut engine = Engine::new();
            register_plot_functions(&mut engine, sender.clone());
            register_lab_functions(&mut engine, sender);
            while let Ok((source, reply)) = rx.recv() {
                let result = engine
                    .compile_and_run_raw_program(source)
                    .map(|values| values.into_iter().map(|v| v.to_string()).collect())
                    .map_err(|e| e.to_string());
                reply.send(result).unwrap();
            }
        });
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
    let mut connection = Integration::default();
    let mut plots = PlotState::default();
    let steel = SteelThread::new(connection.sender());
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
        steel.eval("(lab/subscribe! '(\"a\" \"b\"))").unwrap(),
        vec!["2"]
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
    assert_eq!(
        steel
            .eval("(lab/get \"a\") (lab/set! \"power\" 20) (lab/set! \"reference\" 20.0)")
            .unwrap(),
        vec!["3", "4", "5"]
    );
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
        steel.eval("(lab/unsubscribe! '(\"a\"))").unwrap(),
        vec!["6"]
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
    assert_eq!(steel.eval("(lab/subscribe! '(\"a\"))").unwrap(), vec!["7"]);
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
