//! SIGTERM reaches a thread blocked in a read.
//!
//! Its own integration binary: the shutdown flag is process-global.
//!
//! The handler used to be installed with `signal`, which on Linux glibc
//! and macOS sets `SA_RESTART`. The kernel then restarted the blocked
//! read after the handler ran, the flag it set was never looked at, and
//! a capture blocked on a silent peer ignored SIGTERM indefinitely.

#![allow(unsafe_code)]
#![cfg(unix)]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::thread::JoinHandleExt;
use std::sync::mpsc;
use std::time::Duration;

use oq_l2feed::session::{install_signal_handlers, shutdown_requested};

#[test]
fn a_blocked_read_returns_when_sigterm_arrives() {
    install_signal_handlers();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        // No read timeout: the only way out of this read is the signal.
        let mut stream = TcpStream::connect(addr).expect("connect");
        let mut buf = [0_u8; 16];
        let _ = tx.send(stream.read(&mut buf).map_err(|e| e.kind()));
    });
    let _held = listener.accept().expect("accept");
    std::thread::sleep(Duration::from_millis(200));

    // SAFETY: the thread is alive (it has not sent yet) and the handler
    // is installed above.
    unsafe {
        libc::pthread_kill(reader.as_pthread_t(), libc::SIGTERM);
    }

    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the read must return once the signal arrives, not be restarted");
    assert_eq!(result, Err(std::io::ErrorKind::Interrupted));
    assert!(shutdown_requested());
    reader.join().expect("reader");
}
