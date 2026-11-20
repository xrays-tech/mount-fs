//! Loopback networking helpers for multi-process engines.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Bind 127.0.0.1:0, read the assigned port, and release it.
pub fn free_loopback_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// Wait until a TCP connection to 127.0.0.1:`port` succeeds.
pub fn wait_for_tcp_port(port: u16, timeout: Duration) -> io::Result<()> {
    let address: SocketAddr = ([127, 0, 0, 1], port).into();
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for 127.0.0.1:{port}"),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_can_be_rebound() {
        let port = free_loopback_port().expect("port");
        assert!(port > 0);
        let _listener = TcpListener::bind(("127.0.0.1", port)).expect("rebind");
    }

    #[test]
    fn waits_for_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        wait_for_tcp_port(port, Duration::from_secs(1)).expect("ready");
        drop(listener);
    }
}
