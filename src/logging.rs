//! Allocation-free runtime logging with a text or JSON Lines encoding.

use core::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::LogFormat;
use crate::util::StackStr;

static JSON: AtomicBool = AtomicBool::new(false);

pub fn configure(format: LogFormat) {
    JSON.store(matches!(format, LogFormat::Json), Ordering::Relaxed);
}

pub fn info(event: &str, message: &str) {
    write_event("info", event, message);
}

pub fn info_args(event: &str, arguments: core::fmt::Arguments<'_>) {
    let mut message = StackStr::<2048>::new();
    let _ = message.write_fmt(arguments);
    write_event("info", event, message.as_str());
}

pub fn warn(event: &str, message: &str) {
    write_event("warn", event, message);
}

pub fn error(event: &str, message: &str) {
    write_event("error", event, message);
}

pub fn error_args(event: &str, arguments: core::fmt::Arguments<'_>) {
    let mut message = StackStr::<2048>::new();
    let _ = message.write_fmt(arguments);
    write_event("error", event, message.as_str());
}

fn write_event(level: &str, event: &str, message: &str) {
    let mut line = StackStr::<16384>::new();
    if JSON.load(Ordering::Relaxed) {
        let milliseconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let _ = write!(line, "{{\"timestamp_unix_ms\":{milliseconds},\"level\":\"");
        append_json(&mut line, limited(level, 16));
        let _ = write!(line, "\",\"event\":\"");
        append_json(&mut line, limited(event, 128));
        let _ = write!(line, "\",\"message\":\"");
        append_json(&mut line, limited(message, 2048));
        let _ = writeln!(line, "\"}}");
    } else {
        let _ = writeln!(line, "pos3ql: {}", limited(message, 2048));
    }
    raw_stderr(line.as_str().as_bytes());
}

fn limited(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn append_json<const N: usize>(out: &mut StackStr<N>, value: &str) {
    for character in value.chars() {
        match character {
            '"' => {
                let _ = write!(out, "\\\"");
            }
            '\\' => {
                let _ = write!(out, "\\\\");
            }
            '\n' => {
                let _ = write!(out, "\\n");
            }
            '\r' => {
                let _ = write!(out, "\\r");
            }
            '\t' => {
                let _ = write!(out, "\\t");
            }
            character if character < ' ' => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => {
                let _ = write!(out, "{character}");
            }
        }
    }
}

fn raw_stderr(message: &[u8]) {
    unsafe {
        libc::write(2, message.as_ptr().cast(), message.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping_is_complete_for_log_fields() {
        let mut out = StackStr::<128>::new();
        append_json(&mut out, "quote \" slash \\ line\n\t\u{1}");
        assert_eq!(out.as_str(), "quote \\\" slash \\\\ line\\n\\t\\u0001");
    }
}
