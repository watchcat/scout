//! The server stops as a pod is stopped: a SIGTERM starts its graceful
//! shutdown, and it ends of its own accord rather than by the signal. (That
//! a call under way is let finish is axum's graceful shutdown, and is not
//! tried here.)

#![cfg(unix)]

use std::io::Read;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_sigterm_stops_the_server_and_it_ends_on_its_own() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let mut server = Command::new(env!("CARGO_BIN_EXE_scout-browser"))
        .env("SCOUT_BROWSER_BIND", format!("127.0.0.1:{port}"))
        .env_remove("SCOUT_BROWSER_FENCE")
        // A file that is there, so the server starts: nothing is rendered.
        .env("SCOUT_CHROME", "/bin/sh")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let listening = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        if Instant::now() > listening || server.try_wait().unwrap().is_some() {
            let _ = server.kill();
            let mut said = String::new();
            let _ = server.stderr.take().unwrap().read_to_string(&mut said);
            panic!("it never listened: {said}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let pid = rustix::process::Pid::from_child(&server);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let ending = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = server.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > ending {
            let _ = server.kill();
            panic!("it did not stop within ten seconds of a SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut said = String::new();
    server.stdout.take().unwrap().read_to_string(&mut said).unwrap();
    // Killed by the signal, a process has no exit code of its own.
    assert!(status.success(), "it was ended by the signal, not by itself: {status:?}");
    assert!(said.contains("stopped"), "it did not say it stopped: {said}");
}
