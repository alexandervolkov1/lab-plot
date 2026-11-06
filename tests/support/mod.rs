use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub const WAIT: Duration = Duration::from_secs(5);

pub fn mock(
    action: impl FnOnce(BufReader<TcpStream>) + Send + 'static,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let start = Instant::now();
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < WAIT, "accept timeout");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        // Windows inherits the listener's nonblocking mode on accepted sockets.
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        action(BufReader::new(socket));
    });
    (address, handle)
}
pub fn read(reader: &mut BufReader<TcpStream>) -> Value {
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0, "unexpected EOF");
    serde_json::from_str(&line).unwrap()
}
pub fn send(reader: &mut BufReader<TcpStream>, value: Value) {
    writeln!(reader.get_mut(), "{value}").unwrap();
}
pub fn hello(reader: &mut BufReader<TcpStream>) {
    assert_eq!(read(reader), json!({"id":1,"op":"hello"}));
    send(
        reader,
        json!({"id":1,"status":"ok","protocol":"lab-server","version":1}),
    );
}
pub fn until_closed(reader: &mut BufReader<TcpStream>) {
    let mut bytes = Vec::new();
    // Closing while a handshake reply is still unread may produce RST on
    // Windows. Both EOF and reset terminate the connection; a timeout is not OK.
    match reader.read_to_end(&mut bytes) {
        Ok(_) => (),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Err(error) => panic!("waiting for client close: {error}"),
    }
    assert!(bytes.is_empty(), "unexpected extra requests: {bytes:?}");
}
