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
            keys: vec!["processing/smooth/value".into()],
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
    for _ in 0..3 {
        let ClientEvent::Measurement(m) = next(&client) else {
            panic!("expected measurement")
        };
        assert!(plot.measurement(m).is_empty());
    }
    assert_eq!(plot.series["Smooth"].points.len(), 3);
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
