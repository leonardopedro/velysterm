//! The real-kernel stdio driver (N11 followup).
//!
//! [`run_stdio_kernel`] runs one `\kernel` segment against a *real*
//! kernel process over the stdio transport: the process is launched
//! (`MATHED_KERNEL_BIN`), and the framed Jupyter exchange — a
//! `kernel_info_request` handshake, the `execute_request` carrying the
//! segment's code, then a `shutdown_request` — happens over the
//! kernel's stdin/stdout using [`crate::jupyter_stdio`]'s framing.
//! Messages the kernel publishes before its `execute_reply` are
//! normalized into the `kernel_exec` op's [`KernelOutput`] contract,
//! so a real kernel and the australVM module backend answer the same
//! op. The worker's grant + language gates stay in front unchanged:
//! safety is grants, not per-kernel isolation.
//!
//! Like the module backend, the session is one-shot per segment — a
//! `\kernel` block never shares runtime state with the next one, and
//! the kernel is shut down when the segment finishes. Everything runs
//! under the request's `timeout_ms` and `cap_bytes`.

use crate::KernelOutput;
use crate::jupyter_stdio::{decode_partial, encode_frame};
use std::io::{Read as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The shell reply types the driver waits for.
const INFO_REPLY: &str = "kernel_info_reply";
const EXEC_REPLY: &str = "execute_reply";

/// Run one `\kernel` segment against a real kernel over the stdio
/// transport. `bin` is the kernel launch command (`MATHED_KERNEL_BIN`:
/// e.g. an ipykernel launcher or a stdio-speaking kernel binary),
/// `_language` names the runtime for diagnostics only (a real kernel
/// owns its language), and `code` is the segment body. Returns the
/// kernel's outputs (the `kernel_exec` op contract) or an `Err`
/// message suitable for a UK-4913 failure.
pub fn run_stdio_kernel(
    bin: &str,
    _language: &str,
    code: &str,
    timeout_ms: u64,
    cap_bytes: usize,
) -> Result<Vec<KernelOutput>, String> {
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("kernel launch failed: {e}"))?;
    let stdin = child.stdin.take().ok_or("kernel stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("kernel stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("kernel stderr unavailable")?;

    // The kernel outlives the request, so its output is read by
    // background threads into shared buffers; the driver polls the
    // buffers against its deadline instead of blocking on reads.
    // `overrun` trips when a reader sees the output cap exceeded, and
    // the driver then kills the kernel instead of buffering forever.
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_total = Arc::new(AtomicUsize::new(0));
    let overrun = Arc::new(AtomicBool::new(false));
    {
        let buf = Arc::clone(&stdout_buf);
        let total = Arc::clone(&stdout_total);
        let overrun = Arc::clone(&overrun);
        std::thread::spawn(move || {
            let mut r = stdout;
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if total.fetch_add(n, Ordering::SeqCst) + n > cap_bytes {
                            overrun.store(true, Ordering::SeqCst);
                            break;
                        }
                        buf.lock().unwrap().extend_from_slice(&chunk[..n]);
                    }
                }
            }
        });
    }
    {
        let buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut r = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
    }

    let mut session = Session {
        child,
        stdin: Some(stdin),
        stdout_buf,
        stderr_buf,
        stdout_total,
        overrun,
        deadline: Instant::now() + Duration::from_millis(timeout_ms.max(1)),
        session_id: format!(
            "mathed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ),
        seq: 0,
    };

    let outcome = (|| {
        // 1. Handshake: a kernel_info exchange proves the process
        // actually speaks the Jupyter wire protocol before any code
        // is sent (a silent binary would otherwise hang the execute).
        session.write_msg("kernel_info_request", serde_json::json!({}))?;
        let (_info_msgs, _info_reply) =
            session.collect_until(|m| msg_type(m) == Some(INFO_REPLY))?;

        // 2. Execute the segment, collecting everything the kernel
        // publishes until its execute_reply (Jupyter kernels send
        // stream/execute_result/error iopub messages before the shell
        // reply; any that arrive after it belong to no run).
        session.write_msg(
            "execute_request",
            serde_json::json!({
                "code": code,
                "silent": false,
                "store_history": false,
                "user_expressions": {},
                "allow_stdin": false,
                "stop_on_error": true,
            }),
        )?;
        let (msgs, exec_reply) = session.collect_until(|m| msg_type(m) == Some(EXEC_REPLY))?;
        let mut outputs = crate::jupyter_stdio::outputs_from_messages(&msgs);
        // Belt-and-braces: an execute_reply with status "error" and no
        // error iopub captured still answers with an Error output.
        let status = exec_reply
            .get("content")
            .and_then(|c| c.get("status"))
            .and_then(|s| s.as_str());
        if status == Some("error")
            && !outputs
                .iter()
                .any(|o| matches!(o, KernelOutput::Error { .. }))
        {
            let content = exec_reply.get("content").cloned().unwrap_or_default();
            outputs.push(KernelOutput::Error {
                ename: content
                    .get("ename")
                    .and_then(|e| e.as_str())
                    .unwrap_or("KernelFailed")
                    .to_string(),
                evalue: content
                    .get("evalue")
                    .and_then(|e| e.as_str())
                    .unwrap_or_default()
                    .to_string(),
                traceback: content
                    .get("traceback")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|l| l.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }

        // 3. Shut the kernel down politely; the reaper below closes
        // stdin and waits a short grace before killing.
        let _ = session.write_msg("shutdown_request", serde_json::json!({ "restart": false }));
        Ok(outputs)
    })();

    // Reap: close stdin (EOF after shutdown_request), then wait a
    // short grace for the kernel's own exit before killing it — no
    // zombie is left behind whether the exchange succeeded or not.
    session.stdin.take();
    let reap_deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match session.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= reap_deadline {
                    let _ = session.child.kill();
                    let _ = session.child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    outcome
}

/// The live kernel under the driver's control.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stdout_total: Arc<AtomicUsize>,
    overrun: Arc<AtomicBool>,
    deadline: Instant,
    session_id: String,
    seq: u64,
}

impl Session {
    /// Send one Jupyter envelope as a stdio frame.
    fn write_msg(&mut self, msg_type: &str, content: serde_json::Value) -> Result<(), String> {
        self.seq += 1;
        let msg = serde_json::json!({
            "header": {
                "msg_id": format!("{}-{}", self.session_id, self.seq),
                "username": "mathed",
                "session": self.session_id,
                "msg_type": msg_type,
                "version": "5.3",
            },
            "parent_header": {},
            "metadata": {},
            "content": content,
        });
        let frame = encode_frame(&msg);
        let stdin = self.stdin.as_mut().ok_or("kernel stdin closed")?;
        stdin
            .write_all(&frame)
            .map_err(|e| format!("kernel stdin write failed: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("kernel stdin flush failed: {e}"))
    }

    /// Pull every complete frame currently buffered, enforcing the
    /// output cap (a tripped reader overrun kills the session).
    fn drain(&mut self) -> Result<Vec<serde_json::Value>, String> {
        if self.overrun.load(Ordering::SeqCst) {
            return Err(format!(
                "kernel output exceeded the {} byte cap",
                self.stdout_total.load(Ordering::SeqCst)
            ));
        }
        let mut buf = self.stdout_buf.lock().unwrap();
        let (msgs, rest) = decode_partial(&buf)?;
        *buf = rest;
        Ok(msgs)
    }

    /// Wait (against the deadline) for a message matching `pred`,
    /// returning everything decoded so far plus the match. Dies with a
    /// readable error when the kernel exits first, the deadline
    /// passes, or the output cap trips.
    fn collect_until<F>(
        &mut self,
        pred: F,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value), String>
    where
        F: Fn(&serde_json::Value) -> bool,
    {
        let mut seen = Vec::new();
        loop {
            if self.overrun.load(Ordering::SeqCst) {
                return Err(format!(
                    "kernel output exceeded the {} byte cap",
                    self.stdout_total.load(Ordering::SeqCst)
                ));
            }
            let msgs = self.drain()?;
            for m in msgs {
                if pred(&m) {
                    return Ok((seen, m));
                }
                seen.push(m);
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                let err = self.stderr_text();
                return Err(format!(
                    "kernel exited (code {}) before replying: {}",
                    status.code().unwrap_or(-1),
                    if err.trim().is_empty() {
                        "(no stderr)".to_string()
                    } else {
                        err.trim().to_string()
                    }
                ));
            }
            if Instant::now() >= self.deadline {
                return Err("kernel timed out (no reply within the request timeout)".to_string());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The kernel's captured stderr (for error messages).
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr_buf.lock().unwrap()).to_string()
    }
}

/// The message type of a Jupyter envelope (or a bare output object).
fn msg_type(m: &serde_json::Value) -> Option<&str> {
    m.get("header")
        .and_then(|h| h.get("msg_type"))
        .and_then(|t| t.as_str())
        .or_else(|| m.get("msg_type").and_then(|t| t.as_str()))
}
