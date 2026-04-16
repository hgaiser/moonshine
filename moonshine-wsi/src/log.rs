//! Lightweight logging for the Vulkan layer.
//!
//! Controlled by the `MOONSHINE_WSI_LOG` environment variable:
//! - `error` — only errors (default)
//! - `warn`  — errors + warnings
//! - `info`  — errors + warnings + info
//! - `debug` — all messages including debug
//! - `trace` — extremely verbose (every Vulkan call)
//!
//! If `MOONSHINE_WSI_LOG_FILE` is set, logs are written to that file instead
//! of stderr.
//!
//! Messages use a `[moonshine-wsi:<LEVEL>]` prefix.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Numeric log levels, higher = more verbose.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
	Error = 0,
	Warn = 1,
	Info = 2,
	Debug = 3,
	Trace = 4,
}

/// Cached log level from MOONSHINE_WSI_LOG.
static LOG_LEVEL: OnceLock<Level> = OnceLock::new();

/// Optional log file from MOONSHINE_WSI_LOG_FILE.
static LOG_FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Returns the current log level (parsed once from env).
pub fn log_level() -> Level {
	*LOG_LEVEL.get_or_init(|| {
		std::env::var("MOONSHINE_WSI_LOG")
			.ok()
			.and_then(|s| match s.to_lowercase().as_str() {
				"error" => Some(Level::Error),
				"warn" | "warning" => Some(Level::Warn),
				"info" => Some(Level::Info),
				"debug" => Some(Level::Debug),
				"trace" => Some(Level::Trace),
				_ => None,
			})
			.unwrap_or(Level::Error)
	})
}

/// Returns the log file handle, if MOONSHINE_WSI_LOG_FILE is set.
fn log_file() -> &'static Option<Mutex<File>> {
	LOG_FILE.get_or_init(|| {
		std::env::var("MOONSHINE_WSI_LOG_FILE").ok().and_then(|path| {
			OpenOptions::new()
				.create(true)
				.append(true)
				.open(&path)
				.ok()
				.map(Mutex::new)
		})
	})
}

/// Returns true if the given level should be logged.
#[inline]
pub fn enabled(level: Level) -> bool {
	level <= log_level()
}

/// Write a log message to file or stderr.
pub fn write_log(message: &str) {
	// Prepend a wall-clock timestamp in HH:MM:SS.mmm format so that log
	// lines can be correlated with the Moonshine compositor's own logs.
	let ts = {
		use std::time::{SystemTime, UNIX_EPOCH};
		let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
		let total_secs = dur.as_secs();
		let h = (total_secs / 3600) % 24;
		let m = (total_secs / 60) % 60;
		let s = total_secs % 60;
		let ms = dur.subsec_millis();
		format!("{h:02}:{m:02}:{s:02}.{ms:03}")
	};
	let line = format!("{ts} {message}");
	if let Some(file_mutex) = log_file() {
		if let Ok(mut file) = file_mutex.lock() {
			let _ = writeln!(file, "{}", line);
			return;
		}
	}
	eprintln!("{}", line);
}

/// Internal macro to emit a log line.
#[macro_export]
macro_rules! log_impl {
    ($level:expr, $tag:literal, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::write_log(&format!(concat!("[moonshine-wsi:", $tag, "] {}"), format_args!($($arg)*)));
        }
    };
}

/// Log an error message.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::log::Level::Error, "ERROR", $($arg)*)
    };
}

/// Log a warning message.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::log::Level::Warn, "WARN", $($arg)*)
    };
}

/// Log an info message.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::log::Level::Info, "INFO", $($arg)*)
    };
}

/// Log a debug message.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::log::Level::Debug, "DEBUG", $($arg)*)
    };
}

/// Log a trace message (very verbose).
#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        $crate::log_impl!($crate::log::Level::Trace, "TRACE", $($arg)*)
    };
}
