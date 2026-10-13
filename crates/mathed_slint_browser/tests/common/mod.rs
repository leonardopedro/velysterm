//! Shared helper for the browser's integration tests: a `Browser` client that
//! speaks the wire protocol over a pipe to the real binary.
//!
//! Not every test binary uses every helper here, hence the blanket allow.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// One reply from the browser.
#[derive(Debug)]
pub enum Reply {
    Ok {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Empty,
    Err(String),
    Ids(Vec<u64>),
}

pub struct Browser {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Browser {
    pub fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mathed_slint_browser"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mathed_slint_browser");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    pub fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send a request line (and optional payload) and read one reply.
    pub fn send(&mut self, header: &str, payload: Option<&str>) -> Reply {
        self.stdin.write_all(header.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        if let Some(payload) = payload {
            self.stdin.write_all(payload.as_bytes()).unwrap();
        }
        self.stdin.flush().unwrap();
        self.read_reply()
    }

    pub fn create(&mut self, id: u64, width: u32, height: u32, source: &str) -> Reply {
        self.send(
            &format!("CREATE {id} {width} {height} {}", source.len()),
            Some(source),
        )
    }

    pub fn event(&mut self, id: u64, args: &str, payload: Option<&str>) -> Reply {
        self.send(&format!("EVENT {id} {args}"), payload)
    }

    fn read_reply(&mut self) -> Reply {
        let mut header = String::new();
        self.stdout.read_line(&mut header).unwrap();
        let parts: Vec<&str> = header.split_whitespace().collect();
        match parts.first().copied().unwrap_or("") {
            "OK" => {
                let len: usize = parts[4].parse().unwrap();
                if len == 0 {
                    return Reply::Empty;
                }
                let mut rgba = vec![0u8; len];
                self.stdout.read_exact(&mut rgba).unwrap();
                Reply::Ok {
                    width: parts[2].parse().unwrap(),
                    height: parts[3].parse().unwrap(),
                    rgba,
                }
            }
            "ERR" => {
                let len: usize = parts[2].parse().unwrap();
                let mut msg = vec![0u8; len];
                self.stdout.read_exact(&mut msg).unwrap();
                Reply::Err(String::from_utf8_lossy(&msg).into_owned())
            }
            "IDS" => {
                let count: usize = parts[1].parse().unwrap();
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut line = String::new();
                    self.stdout.read_line(&mut line).unwrap();
                    ids.push(line.trim().parse().unwrap());
                }
                Reply::Ids(ids)
            }
            other => panic!("unexpected reply header {other:?} (full: {header:?})"),
        }
    }

    pub fn ids(&mut self) -> Vec<u64> {
        self.stdin.write_all(b"LIST\n").unwrap();
        self.stdin.flush().unwrap();
        match self.read_reply() {
            Reply::Ids(ids) => ids,
            other => panic!("expected IDS, got {other:?}"),
        }
    }

    /// Ask the browser to exit and reap it.
    pub fn quit(&mut self) -> std::process::ExitStatus {
        self.stdin.write_all(b"QUIT\n").unwrap();
        self.stdin.flush().unwrap();
        self.child.wait().unwrap()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Unwrap a frame reply, panicking with the error otherwise.
pub fn ok(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Ok { rgba, .. } => rgba,
        Reply::Empty => panic!("expected a frame, got an empty reply"),
        Reply::Err(e) => panic!("expected a frame, got an error: {e}"),
        Reply::Ids(_) => panic!("expected a frame, got ids"),
    }
}

/// Most common colour in the frame, used to read a widget's state off its
/// dominant pixel colour.
pub fn dominant(rgba: &[u8]) -> [u8; 3] {
    let mut counts: std::collections::HashMap<[u8; 3], usize> = Default::default();
    for px in rgba.chunks(4) {
        *counts.entry([px[0], px[1], px[2]]).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| c)
        .unwrap()
}

pub const SOLID: &str = "export component Demo inherits Window { background: #203040; }";
