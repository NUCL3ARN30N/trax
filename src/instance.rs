//! One trax at a time. A second start (e.g. double-clicking a song in Explorer) hands its
//! files to the running instance over a local socket and exits.
//!
//! The running instance writes `instance` (port and a random token) into the data folder;
//! only processes that can read that file can talk to it.

use crate::paths;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

fn instance_file() -> PathBuf {
    paths::data_dir().join("instance")
}

/// Try to hand `files` to an already running trax. True if one took them.
pub fn forward(files: &[PathBuf]) -> bool {
    let Ok(info) = std::fs::read_to_string(instance_file()) else { return false };
    let Some((port, token)) = info.trim().split_once(' ') else { return false };
    let Ok(port) = port.parse::<u16>() else { return false };
    let Ok(mut stream) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut msg = format!("{token}\n");
    for f in files {
        msg.push_str(&f.to_string_lossy());
        msg.push('\n');
    }
    msg.push('\n');
    if stream.write_all(msg.as_bytes()).is_err() {
        return false;
    }
    // Wait for the acknowledgement so a stale port of another program doesn't count.
    let mut ack = String::new();
    BufReader::new(stream).read_line(&mut ack).is_ok() && ack.trim() == "ok"
}

/// Become the running instance. `on_open` gets the files of every later start (empty = just
/// show the window).
pub fn listen(on_open: impl Fn(Vec<PathBuf>) + Send + 'static) {
    let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else { return };
    let Ok(addr) = listener.local_addr() else { return };
    let token = format!("{:032x}", rand::random::<u128>());
    if std::fs::write(instance_file(), format!("{} {token}", addr.port())).is_err() {
        return;
    }
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim() != token {
                continue;
            }
            let mut files = vec![];
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.trim().is_empty() => break,
                    Ok(_) => files.push(PathBuf::from(line.trim_end_matches(['\r', '\n']))),
                }
            }
            let _ = (&stream).write_all(b"ok\n");
            on_open(files);
        }
    });
}

/// Stop accepting hand-overs (at exit, before an update starts the new version).
pub fn release() {
    let _ = std::fs::remove_file(instance_file());
}
