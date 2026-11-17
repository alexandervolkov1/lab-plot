use lab_plot::{
    plots::{PlotCommand, PlotState},
    tcp_client::*,
};
use lab_server::{
    config::ServerConfig,
    driver::DeviceIo,
    startup::{create_driver, start_with},
};
use serde_json::json;
use std::{
    io::{self, BufRead, Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
struct ArduinoMemory(Cursor<Vec<u8>>);
impl Read for ArduinoMemory {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl BufRead for ArduinoMemory {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.0.fill_buf()
    }
    fn consume(&mut self, n: usize) {
        self.0.consume(n)
    }
}
impl DeviceIo for ArduinoMemory {
    fn send(&mut self, b: &[u8]) -> io::Result<()> {
        let command = std::str::from_utf8(b).unwrap();
        let p = command.split_whitespace().collect::<Vec<_>>();
        assert_eq!(p[2], "get", "shadow pipeline must never write");
        self.0 = Cursor::new(
            format!(
                "@{} ok {}\n",
                p[1],
                if p[3] == "power" { "0" } else { "20.0" }
            )
            .into_bytes(),
        );
        Ok(())
    }
}
fn next(client: &Client) -> ClientEvent {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(e) = client.try_recv() {
            return e;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn real_arduino_driver_ema_subscription_reaches_plot_without_hardware() {
    let config=ServerConfig::from_toml("[tcp]\nbind='127.0.0.1:0'\n[[ports]]\nid='p'\nname='MEMORY'\nbaud=9600\ndata_bits=8\nparity='none'\nstop_bits=1\nflow_control='none'\ntimeout_ms=100\n[[devices]]\nid='furnace1'\ndriver='arduino'\nport='p'\n[devices.poll_periods_ms]\nsample_temperature=10\n").unwrap();
    let server = start_with(
        &config,
        |d| {
            let mut setup = create_driver(d)?;
            setup.post_open_delay = Duration::ZERO;
            Ok(setup)
        },
        |_, _| Ok(ArduinoMemory(Cursor::new(vec![]))),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = thread::spawn(move || server.run_until(&flag).unwrap());
    let client = Client::start_endpoint(address.into(), ClientOptions::default()).unwrap();
    assert_eq!(next(&client), ClientEvent::Ready);
    client.sender().request(Request::Components{command:json!({"action":"create","expected_revision":0,"configuration":{"name":"smooth","type_id":"ema","inputs":{"input":"furnace1/sample_temperature"},"parameters":{"tau":2.},"max_age_ms":10000}})}).unwrap();
    assert!(matches!(
        next(&client),
        ClientEvent::Response(Response {
            data: ResponseData::Components(_),
            ..
        })
    ));
    let mut plot = PlotState::default();
    component_request(
        &client,
        json!({"action":"create","expected_revision":1,"configuration":{"name":"loop","type_id":"pid","inputs":{"input":"processing/smooth/value"},"parameters":{"setpoint":20.},"max_age_ms":10000}}),
    );
    let changed = component_request(
        &client,
        json!({"action":"parameter_set","name":"loop","parameter":"setpoint","value":35.}),
    );
    assert_eq!(changed["revision"], 3);
    plot.apply(PlotCommand::Add {
        name: "Setpoint".into(),
        unit: "degC".into(),
    })
    .unwrap();
    plot.apply(PlotCommand::Bind {
        name: "Setpoint".into(),
        key: "processing/loop/setpoint".into(),
    })
    .unwrap();
    plot.apply(PlotCommand::Add {
        name: "Smooth".into(),
        unit: "degC".into(),
    })
    .unwrap();
    plot.apply(PlotCommand::Bind {
        name: "Smooth".into(),
        key: "processing/smooth/value".into(),
    })
    .unwrap();
    client
        .sender()
        .request(Request::Subscribe {
            keys: vec![
                "processing/smooth/value".into(),
                "processing/loop/setpoint".into(),
            ],
        })
        .unwrap();
    let ack = next(&client);
    assert!(
        matches!(
            &ack,
            ClientEvent::Response(Response {
                data: ResponseData::Subscriptions(_),
                ..
            })
        ),
        "{ack:?}"
    );
    while plot.series["Smooth"].points.len() < 3 || plot.series["Setpoint"].points.len() < 3 {
        let ClientEvent::Measurement(m) = next(&client) else {
            panic!("expected measurement")
        };
        assert!(plot.measurement(m).is_empty());
    }
    assert_eq!(plot.series["Smooth"].points.len(), 3);
    assert_eq!(plot.series["Setpoint"].points.len(), 3);
    for (smooth, setpoint) in plot.series["Smooth"]
        .points
        .iter()
        .zip(&plot.series["Setpoint"].points)
    {
        assert_eq!(smooth.xy[0], setpoint.xy[0]);
        assert_eq!(setpoint.xy[1], 35.);
    }
    assert!(
        plot.series["Smooth"]
            .points
            .iter()
            .all(|p| (p.xy[1] - 20.).abs() < 1e-10)
    );
    client.request_shutdown();
    while !matches!(next(&client), ClientEvent::Stopped(_)) {}
    stop.store(true, Ordering::SeqCst);
    join.join().unwrap();
}

struct ControlledArduinoMemory {
    inner: ArduinoMemory,
    writes: Arc<std::sync::Mutex<Vec<i64>>>,
}
impl Read for ControlledArduinoMemory {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.inner.read(b)
    }
}
impl BufRead for ControlledArduinoMemory {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }
    fn consume(&mut self, n: usize) {
        self.inner.consume(n)
    }
}
impl DeviceIo for ControlledArduinoMemory {
    fn send(&mut self, b: &[u8]) -> io::Result<()> {
        let command = std::str::from_utf8(b).unwrap();
        let parts = command.split_whitespace().collect::<Vec<_>>();
        if parts[2] == "get" {
            return self.inner.send(b);
        }
        assert_eq!(parts[2], "set");
        assert_eq!(parts[3], "power");
        self.writes
            .lock()
            .unwrap()
            .push(parts[4].parse::<i64>().expect("integer output"));
        self.inner.0 = Cursor::new(format!("@{} ok\n", parts[1]).into_bytes());
        Ok(())
    }
}
fn component_request(client: &Client, command: serde_json::Value) -> serde_json::Value {
    let id = client
        .sender()
        .request(Request::Components { command })
        .unwrap();
    let ClientEvent::Response(response) = next(client) else {
        panic!("expected response")
    };
    assert_eq!(response.id, id);
    let ResponseData::Components(data) = response.data else {
        panic!("{:?}", response.data)
    };
    data
}

#[test]
fn managed_arduino_controller_bindings_and_manual_auto_work_with_real_tcp_and_ws_client() {
    use lab_server::{
        pending::{Completion, Poll},
        port_worker::CommandError,
    };
    for ws in [false, true] {
        let config=ServerConfig::from_toml("[tcp]\nbind='127.0.0.1:0'\n[websocket]\nenabled=true\nbind='127.0.0.1:0'\n[[ports]]\nid='p'\nname='MEMORY'\nbaud=9600\ndata_bits=8\nparity='none'\nstop_bits=1\nflow_control='none'\ntimeout_ms=100\n[[devices]]\nid='furnace1'\ndriver='arduino'\nport='p'\n[devices.poll_periods_ms]\nsample_temperature=10\n").unwrap();
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = start_with(
            &config,
            |d| {
                let mut setup = create_driver(d)?;
                setup.post_open_delay = Duration::ZERO;
                Ok(setup)
            },
            |_, _| {
                Ok(ControlledArduinoMemory {
                    inner: ArduinoMemory(Cursor::new(vec![])),
                    writes: writes.clone(),
                })
            },
        )
        .unwrap();
        let handle = server
            .backend()
            .registry()
            .port("p")
            .unwrap()
            .handle()
            .unwrap()
            .clone();
        let action = server
            .backend()
            .registry()
            .actions()
            .next()
            .unwrap()
            .description
            .name
            .clone();
        let endpoint: Endpoint = if ws {
            format!(
                "ws://{}/application/v1",
                server.websocket_addr().unwrap().unwrap()
            )
            .parse()
            .unwrap()
        } else {
            server.local_addr().unwrap().into()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let join = thread::spawn(move || server.run_until(&flag).unwrap());
        let client = Client::start_endpoint(endpoint.clone(), ClientOptions::default()).unwrap();
        let peer = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
        assert_eq!(next(&client), ClientEvent::Ready);
        assert_eq!(next(&peer), ClientEvent::Ready);
        for mut pending in [
            handle
                .set(
                    1,
                    "furnace1",
                    "power",
                    lab_server::model::Value::Integer(99),
                    Instant::now() + Duration::from_secs(10),
                )
                .unwrap(),
            handle
                .invoke_action(
                    2,
                    "furnace1",
                    action,
                    Instant::now() + Duration::from_secs(10),
                )
                .unwrap(),
        ] {
            loop {
                match pending.poll(Instant::now()) {
                    Poll::Waiting(p) => {
                        pending = p;
                        thread::sleep(Duration::from_millis(1));
                    }
                    Poll::Finished(Completion::Received {
                        value: Err(CommandError::Unauthorized),
                        ..
                    }) => break,
                    _ => panic!("Managed startup admitted a direct mutation"),
                }
            }
        }
        assert!(writes.lock().unwrap().is_empty());
        component_request(
            &client,
            json!({"action":"create","expected_revision":0,"configuration":{"name":"smooth","type_id":"ema","inputs":{"input":"furnace1/sample_temperature"},"parameters":{"tau":1.},"max_age_ms":10000}}),
        );
        let created = component_request(
            &client,
            json!({"action":"create","expected_revision":1,"configuration":{"name":"loop","type_id":"pid","inputs":{"input":"processing/smooth/value"},"parameters":{"setpoint":23.},"max_age_ms":10000}}),
        );
        assert!(created["instances"][0]["binding"].is_null());
        let bound = component_request(
            &client,
            json!({"action":"bind","name":"loop","target":"furnace1/power","command_timeout_ms":1000,"expected_revision":2}),
        );
        assert_eq!(
            bound["instances"][0]["binding"],
            json!({"target":"furnace1/power","command_timeout_ms":1000})
        );
        assert_eq!(component_request(&peer, json!({"action":"list"})), bound);
        // Wait for a computed shadow result, without subscribing this request-only client.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            client
                .sender()
                .request(Request::Get {
                    key: "processing/loop/output".into(),
                })
                .unwrap();
            if matches!(
                next(&client),
                ClientEvent::Response(Response {
                    data: ResponseData::Measurement(_),
                    ..
                })
            ) {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(writes.lock().unwrap().is_empty());
        component_request(
            &client,
            json!({"action":"state","name":"loop","state":"auto","expected_revision":3}),
        );
        while writes.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(writes.lock().unwrap().iter().all(|v| *v == 3));
        client
            .sender()
            .request(Request::Set {
                key: "furnace1/power".into(),
                value: ScalarValue::Integer(2),
            })
            .unwrap();
        let ClientEvent::Response(Response {
            data: ResponseData::Error(error),
            ..
        }) = next(&client)
        else {
            panic!("manual set bypassed Auto")
        };
        assert_eq!(error.error, "output_owned");
        let stopped = component_request(
            &client,
            json!({"action":"state","name":"loop","state":"stopped","expected_revision":4}),
        );
        assert_eq!(
            stopped["instances"][0]["binding"],
            bound["instances"][0]["binding"]
        );
        client
            .sender()
            .request(Request::Set {
                key: "furnace1/power".into(),
                value: ScalarValue::Integer(2),
            })
            .unwrap();
        assert!(matches!(
            next(&client),
            ClientEvent::Response(Response {
                data: ResponseData::Ack,
                ..
            })
        ));
        assert_eq!(writes.lock().unwrap().last(), Some(&2));
        for c in [&client, &peer] {
            c.request_shutdown();
            while !matches!(next(c), ClientEvent::Stopped(_)) {}
        }
        stop.store(true, Ordering::SeqCst);
        join.join().unwrap();
        assert!(handle.stop_reason().is_some());
    }
}
