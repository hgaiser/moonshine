//! Keeps the most recent log lines in memory for the logs page.

use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// How many lines the buffer keeps.
const CAPACITY: usize = 2000;

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
	/// Milliseconds since the Unix epoch.
	pub time: u64,
	pub level: &'static str,
	pub target: String,
	pub message: String,
}

/// The most recent log lines. Cloning is cheap and all clones share the same lines.
#[derive(Clone)]
pub struct LogBuffer {
	lines: Arc<Mutex<VecDeque<LogLine>>>,
	tx: broadcast::Sender<LogLine>,
}

impl Default for LogBuffer {
	fn default() -> Self {
		Self::new()
	}
}

impl LogBuffer {
	pub fn new() -> Self {
		Self {
			lines: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
			tx: broadcast::channel(1024).0,
		}
	}

	/// A tracing layer that writes into this buffer. Give it its own filter, so it captures
	/// lines that the terminal does not show.
	pub fn layer(&self) -> LogLayer {
		LogLayer { buffer: self.clone() }
	}

	/// The most recent `count` lines, oldest first.
	pub fn tail(&self, count: usize) -> Vec<LogLine> {
		let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
		lines.iter().skip(lines.len().saturating_sub(count)).cloned().collect()
	}

	pub(crate) fn subscribe(&self) -> broadcast::Receiver<LogLine> {
		self.tx.subscribe()
	}

	fn push(&self, line: LogLine) {
		{
			let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
			if lines.len() == CAPACITY {
				lines.pop_front();
			}
			lines.push_back(line.clone());
		}
		let _ = self.tx.send(line);
	}
}

pub struct LogLayer {
	buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for LogLayer {
	fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
		let metadata = event.metadata();
		let mut visitor = MessageVisitor::default();
		event.record(&mut visitor);

		let level = match *metadata.level() {
			Level::ERROR => "ERROR",
			Level::WARN => "WARN",
			Level::INFO => "INFO",
			Level::DEBUG => "DEBUG",
			Level::TRACE => "TRACE",
		};
		let time = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_millis() as u64)
			.unwrap_or(0);

		self.buffer.push(LogLine {
			time,
			level,
			target: metadata.target().to_string(),
			message: visitor.finish(),
		});
	}
}

#[derive(Default)]
struct MessageVisitor {
	message: String,
	fields: String,
}

impl MessageVisitor {
	fn finish(self) -> String {
		if self.fields.is_empty() {
			self.message
		} else if self.message.is_empty() {
			self.fields
		} else {
			format!("{} {}", self.message, self.fields)
		}
	}
}

impl Visit for MessageVisitor {
	fn record_str(&mut self, field: &Field, value: &str) {
		if field.name() == "message" {
			self.message = value.to_string();
		} else {
			self.record_debug(field, &value);
		}
	}

	fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
		if field.name() == "message" {
			self.message = format!("{value:?}");
		} else {
			if !self.fields.is_empty() {
				self.fields.push(' ');
			}
			let _ = write!(self.fields, "{}={value:?}", field.name());
		}
	}
}
