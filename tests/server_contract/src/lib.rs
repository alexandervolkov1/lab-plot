//! Opt-in contract against the actual server, with no physical I/O.
#[cfg(test)]
mod tests {
    use lab_plot::tcp_client::*;
    use lab_server::{
        command_router::CommandRouter,
        device_registry::DeviceRegistry,
        driver::{DeviceIo, Driver, DriverError, PortState},
        model::{ActionDescription as ServerAction, ParameterDescription, Spec, Value},
        port_worker::{DeviceInstance, PortWorker},
        service::DeviceService,
    };
    use std::{
        io::{BufReader, Cursor},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    struct MemoryDriver {
        calls: Arc<AtomicUsize>,
        enabled: bool,
    }
    impl Driver for MemoryDriver {
        fn parameters(&self) -> Vec<ParameterDescription> {
            vec![ParameterDescription {
                id: "enabled".into(),
                spec: Spec::Boolean,
                unit: String::new(),
                writable: true,
                default_poll_period: None,
            }]
        }
        fn actions(&self) -> Vec<ServerAction> {
            ["apply", "fail"]
                .into_iter()
                .map(|name| ServerAction {
                    name: name.into(),
                    description: format!("Perform {name}"),
                })
                .collect()
        }
        fn invoke_action(&mut self, action: &str, _: &mut dyn DeviceIo) -> Result<(), DriverError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match action {
                "apply" => Ok(()),
                "fail" => Err(DriverError::new(
                    "injected failure",
                    PortState::Synchronized,
                )),
                other => panic!("unexpected action: {other}"),
            }
        }
        fn read(&mut self, _: &str, _: &mut dyn DeviceIo) -> Result<Value, DriverError> {
            Ok(Value::Boolean(self.enabled))
        }
        fn write(
            &mut self,
            _: &str,
            value: &Value,
            _: &mut dyn DeviceIo,
        ) -> Result<(), DriverError> {
            let Value::Boolean(value) = value else {
                panic!("expected Boolean")
            };
            self.enabled = *value;
            Ok(())
        }
    }
    fn event(client: &Client) -> ClientEvent {
        let start = Instant::now();
        loop {
            if let Some(event) = client.try_recv() {
                return event;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{:?}",
                client.state()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn request(client: &Client, request: Request) -> ResponseData {
        let id = client.sender().request(request).unwrap();
        let ClientEvent::Response(response) = event(client) else {
            panic!("expected response")
        };
        assert_eq!(response.id, id);
        response.data
    }
    fn contract(ws: bool) {
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = MemoryDriver {
            calls: calls.clone(),
            enabled: false,
        };
        let (worker, channels) = PortWorker::new(
            BufReader::new(Cursor::new(Vec::<u8>::new())),
            vec![DeviceInstance::new("furnace1", driver).unwrap()],
            8,
            8,
        )
        .unwrap();
        let mut registry = DeviceRegistry::new();
        registry
            .register_port("memory", channels.commands.clone())
            .unwrap();
        for device in worker.devices() {
            registry.register_device("memory", device).unwrap();
        }
        let service = DeviceService::new(CommandRouter::new(registry), 8);
        let worker = thread::spawn(move || worker.run());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let start = Instant::now();
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(start.elapsed() < Duration::from_secs(10));
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                if ws {
                    lab_server::websocket::serve_connection(
                        socket,
                        &service,
                        &[],
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                } else {
                    service.serve_connection(socket).unwrap();
                }
            }
        });
        let endpoint: Endpoint = if ws {
            format!("ws://{address}/application/v1").parse().unwrap()
        } else {
            address.into()
        };
        let client = Client::start_endpoint(endpoint.clone(), ClientOptions::default()).unwrap();
        assert_eq!(event(&client), ClientEvent::Ready);
        let description = request(&client, Request::Describe);
        assert_eq!(description.parameters().unwrap().len(), 1);
        assert_eq!(
            description.actions().unwrap(),
            [
                ActionDescription {
                    key: "furnace1/apply".into(),
                    description: "Perform apply".into()
                },
                ActionDescription {
                    key: "furnace1/fail".into(),
                    description: "Perform fail".into()
                },
            ]
        );
        assert_eq!(
            request(
                &client,
                Request::InvokeAction {
                    key: "furnace1/apply".into()
                }
            ),
            ResponseData::Ack
        );
        for (key, expected) in [
            ("furnace1/missing", "unknown_key"),
            ("furnace1/fail", "outcome_unknown"),
        ] {
            let ResponseData::Error(error) =
                request(&client, Request::InvokeAction { key: key.into() })
            else {
                panic!()
            };
            assert_eq!(error.error, expected);
        }
        assert_eq!(
            request(
                &client,
                Request::Set {
                    key: "furnace1/enabled".into(),
                    value: ScalarValue::Boolean(true)
                }
            ),
            ResponseData::Ack
        );
        assert!(matches!(
            request(
                &client,
                Request::Get {
                    key: "furnace1/enabled".into()
                }
            ),
            ResponseData::Measurement(Measurement {
                reading: Reading::Value {
                    value: ScalarValue::Boolean(true),
                    ..
                },
                ..
            })
        ));
        client.request_shutdown();
        assert!(matches!(event(&client), ClientEvent::Stopped(_)));
        assert!(client.try_recv().is_none());
        let client = Client::start_endpoint(endpoint, ClientOptions::default()).unwrap();
        assert_eq!(event(&client), ClientEvent::Ready);
        assert_eq!(request(&client, Request::Ping), ResponseData::Ack);
        client.request_shutdown();
        assert!(matches!(event(&client), ClientEvent::Stopped(_)));
        server.join().unwrap();
        channels.commands.shutdown();
        worker.join().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "no action retries, including after reconnect"
        );
    }
    #[test]
    fn tcp_actual_server_with_memory_driver() {
        contract(false);
    }
    #[test]
    fn ws_actual_server_with_memory_driver() {
        contract(true);
    }
}
