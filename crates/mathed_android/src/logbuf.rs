//! Logging for a process with no terminal.
//!
//! Every line goes two places: Android's logcat (so `adb logcat` /
//! `waydroid logcat` shows it) and an in-process ring buffer that the debug
//! protocol serves as `{"cmd":"log"}`. The ring buffer is the load-bearing
//! half: an autonomous debug loop reads the log over the same socket it
//! drives the app with, instead of shelling out to `logcat` and re-parsing
//! its timestamps.
//!
//! On a non-Android build there is no logcat, so lines go to stderr as well —
//! which is what makes the host-side tests of this crate readable.

use std::sync::{Mutex, OnceLock};

/// How many lines the ring buffer keeps. Bounded on purpose: an Android app
/// runs for days, and the buffer exists to explain the last few interactions,
/// not to be a log file.
const CAPACITY: usize = 512;

const TAG: &str = "mathed_android";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Line {
    /// Milliseconds since process start.
    pub t: u128,
    pub level: &'static str,
    pub msg: String,
}

fn buffer() -> &'static Mutex<Vec<Line>> {
    static BUFFER: OnceLock<Mutex<Vec<Line>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(Vec::with_capacity(CAPACITY)))
}

fn start() -> std::time::Instant {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

/// Append a line, oldest-first, dropping the oldest when full.
fn record(level: &'static str, msg: String) {
    let line = Line {
        t: start().elapsed().as_millis(),
        level,
        msg,
    };
    if let Ok(mut buf) = buffer().lock() {
        if buf.len() == CAPACITY {
            buf.remove(0);
        }
        buf.push(line);
    }
}

/// The most recent lines, oldest first, capped at `n`.
pub fn tail(n: usize) -> Vec<Line> {
    let Ok(buf) = buffer().lock() else {
        return Vec::new();
    };
    let from = buf.len().saturating_sub(n);
    buf[from..].to_vec()
}

/// Every line currently held.
pub fn all() -> Vec<Line> {
    buffer().lock().map(|b| b.clone()).unwrap_or_default()
}

#[cfg(target_os = "android")]
mod platform {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    // liblog ships with the platform; declaring the three symbols we use keeps
    // this crate off `ndk-sys` (and therefore off a second copy of the NDK
    // bindings with its own version).
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    /// Android's `ANDROID_LOG_*` priorities.
    pub(super) const INFO: c_int = 4;
    pub(super) const WARN: c_int = 5;
    pub(super) const ERROR: c_int = 6;

    pub(super) fn write(prio: c_int, msg: &str) {
        let tag = CString::new(super::TAG).unwrap_or_default();
        // A log line with an interior NUL would otherwise be truncated
        // silently; the loss is preferable to dropping the line entirely.
        let text = CString::new(msg.replace('\0', " ")).unwrap_or_default();
        // SAFETY: both pointers are valid, NUL-terminated, and outlive the call.
        unsafe {
            let _ = __android_log_write(prio, tag.as_ptr(), text.as_ptr());
        }
    }
}

#[cfg(not(target_os = "android"))]
mod platform {
    pub(super) const INFO: i32 = 4;
    pub(super) const WARN: i32 = 5;
    pub(super) const ERROR: i32 = 6;

    pub(super) fn write(prio: i32, msg: &str) {
        let level = match prio {
            p if p >= ERROR => "error",
            p if p >= WARN => "warn",
            _ => "info",
        };
        eprintln!("[{level}] {}: {msg}", super::TAG);
    }
}

/// Log at info priority.
pub fn info(msg: impl Into<String>) {
    let msg = msg.into();
    record("info", msg.clone());
    platform::write(platform::INFO, &msg);
}

/// Log at warn priority.
pub fn warn(msg: impl Into<String>) {
    let msg = msg.into();
    record("warn", msg.clone());
    platform::write(platform::WARN, &msg);
}

/// Log at error priority.
pub fn error(msg: impl Into<String>) {
    let msg = msg.into();
    record("error", msg.clone());
    platform::write(platform::ERROR, &msg);
}

/// `format!`-style info logging.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logbuf::info(format!($($arg)*)) };
}

/// `format!`-style warn logging.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logbuf::warn(format!($($arg)*)) };
}

/// `format!`-style error logging.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::logbuf::error(format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_the_most_recent_lines_in_order() {
        for i in 0..5 {
            info(format!("line {i}"));
        }
        let tail = tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].msg, "line 3");
        assert_eq!(tail[1].msg, "line 4");
    }

    #[test]
    fn the_ring_buffer_is_bounded() {
        for i in 0..(CAPACITY + 10) {
            info(format!("filler {i}"));
        }
        assert_eq!(all().len(), CAPACITY, "oldest lines are dropped");
        assert!(
            all().last().is_some_and(|l| l.msg.starts_with("filler")),
            "the newest line survives"
        );
    }
}
