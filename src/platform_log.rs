//! Routes the kernel's and orbital-canvas's `tracing` events to the platform log (logcat, the Apple
//! unified log), installed once on the host's first call. A host that installed its own subscriber
//! keeps it.

use std::io::Write;
use std::sync::Once;
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

/// Longest chunk one platform log entry keeps whole; longer lines are split, never cut.
#[cfg(target_os = "android")]
const ENTRY_BYTES: usize = 4000;
#[cfg(not(target_os = "android"))]
const ENTRY_BYTES: usize = 900;

pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(PlatformLog)
            .try_init();
    });
}

/// 0 trace/debug, 1 info, 2 warn... mapped per platform below; error is 3.
fn rank(level: &Level) -> i32 {
    match *level {
        Level::ERROR => 3,
        Level::WARN => 2,
        Level::INFO => 1,
        _ => 0,
    }
}

/// `line` in chunks of at most `max` bytes, split on char boundaries.
pub(crate) fn chunks(line: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

#[cfg(target_os = "android")]
fn write_entry(rank: i32, text: &std::ffi::CStr) {
    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(
            prio: i32,
            tag: *const std::ffi::c_char,
            text: *const std::ffi::c_char,
        ) -> i32;
    }
    // android_LogPriority: DEBUG 3, INFO 4, WARN 5, ERROR 6.
    unsafe { __android_log_write(rank + 3, c"Orbital".as_ptr(), text.as_ptr()) };
}

#[cfg(target_vendor = "apple")]
fn write_entry(rank: i32, text: &std::ffi::CStr) {
    extern "C" {
        fn orbital_ffi_os_log(level: i32, line: *const std::ffi::c_char);
    }
    unsafe { orbital_ffi_os_log(rank, text.as_ptr()) };
}

#[cfg(not(any(target_os = "android", target_vendor = "apple")))]
fn write_entry(_rank: i32, text: &std::ffi::CStr) {
    eprintln!("{}", text.to_string_lossy());
}

fn emit(rank: i32, line: &str) {
    let line = line.trim_end_matches('\n');
    for chunk in chunks(line, ENTRY_BYTES) {
        let text = std::ffi::CString::new(chunk.replace('\0', "\u{fffd}")).expect("NULs replaced");
        write_entry(rank, &text);
    }
}

struct PlatformLog;

/// One formatted event; written to the platform log when the formatter drops it.
pub(crate) struct EventLine {
    rank: i32,
    buf: Vec<u8>,
}

impl Write for EventLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for EventLine {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            emit(self.rank, &String::from_utf8_lossy(&self.buf));
        }
    }
}

impl<'a> MakeWriter<'a> for PlatformLog {
    type Writer = EventLine;
    fn make_writer(&'a self) -> EventLine {
        EventLine {
            rank: 1,
            buf: Vec::new(),
        }
    }
    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> EventLine {
        EventLine {
            rank: rank(meta.level()),
            buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_split_on_char_boundaries_and_keep_every_byte() {
        let line = format!("{}é{}", "a".repeat(8), "b".repeat(5));
        let parts = chunks(&line, 9);
        assert_eq!(parts, vec!["aaaaaaaa", "ébbbbb"]);
        assert_eq!(parts.concat(), line);
        assert!(parts.iter().all(|p| p.len() <= 9));
    }

    #[test]
    fn a_short_line_is_one_entry_and_levels_rank_error_highest() {
        assert_eq!(
            chunks("canvas init failed", 900),
            vec!["canvas init failed"]
        );
        assert_eq!(chunks("", 900), vec![""]);
        assert!(rank(&Level::ERROR) > rank(&Level::WARN));
        assert!(rank(&Level::WARN) > rank(&Level::INFO));
        assert_eq!(rank(&Level::DEBUG), rank(&Level::TRACE));
    }
}
