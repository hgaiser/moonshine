//! Constant-space, five-second streaming diagnostics. No per-packet I/O.

use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use tokio::sync::mpsc;

use super::gso_socket::{SendStats, duration_micros_u64};
use super::{FrameStats, VideoPacketMessage};
use crate::session::manager::SessionShutdownReason;

const INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct TransportWindow {
	enabled: bool,
	started: Instant,
	frames: u64,
	would_block: u64,
	fallback: u64,
	rebases: u64,
	gso: u64,
	per_shard: u64,
	max_send: Duration,
	send_total: Duration,
	max_lateness: Duration,
	max_queue: usize,
	attempted_bytes: u64,
	submitted_bytes: u64,
	submitted_datagrams: u64,
	failed_datagrams: u64,
	discarded_datagrams: u64,
	discarded_bytes: u64,
}

impl TransportWindow {
	pub fn new(enabled: bool) -> Self {
		Self {
			enabled,
			started: Instant::now(),
			frames: 0,
			would_block: 0,
			fallback: 0,
			rebases: 0,
			gso: 0,
			per_shard: 0,
			max_send: Duration::ZERO,
			send_total: Duration::ZERO,
			max_lateness: Duration::ZERO,
			max_queue: 0,
			attempted_bytes: 0,
			submitted_bytes: 0,
			submitted_datagrams: 0,
			failed_datagrams: 0,
			discarded_datagrams: 0,
			discarded_bytes: 0,
		}
	}

	pub fn record(&mut self, stats: &SendStats, queue: usize) {
		if !self.enabled {
			return;
		}
		self.attempted_bytes += stats.outcome.attempted_payload_bytes as u64;
		self.submitted_bytes += stats.outcome.submitted_payload_bytes as u64;
		self.submitted_datagrams += stats.outcome.submitted_datagrams as u64;
		self.discarded_datagrams += stats.discarded_datagrams as u64;
		self.discarded_bytes += stats.discarded_payload_bytes as u64;
		self.failed_datagrams += stats.outcome.failed_datagrams as u64;
		self.frames += 1;
		self.would_block += u64::from(stats.would_block_events);
		self.fallback += u64::from(stats.fallback_chunks);
		self.rebases += u64::from(stats.backpressure_rebases);
		self.gso += u64::from(stats.gso_sends);
		self.per_shard += u64::from(stats.per_shard_sends);
		self.max_send = self.max_send.max(stats.elapsed);
		self.send_total = self.send_total.saturating_add(stats.elapsed);
		self.max_lateness = self.max_lateness.max(stats.max_pacing_lateness);
		self.max_queue = self.max_queue.max(queue);
		if self.started.elapsed() >= INTERVAL {
			tracing::info!(
				attempted_udp_payload_bytes = self.attempted_bytes,
				submitted_udp_payload_bytes = self.submitted_bytes,
				submitted_datagrams = self.submitted_datagrams,
				failed_datagrams = self.failed_datagrams,
				discarded_datagrams = self.discarded_datagrams,
				discarded_payload_bytes = self.discarded_bytes,
				resource_release_completions = self.frames,
				submitted_udp_payload_mbps =
					self.submitted_bytes as f64 * 8.0 / self.started.elapsed().as_secs_f64() / 1e6,
				frames = self.frames,
				would_block_events = self.would_block,
				fallback_chunks = self.fallback,
				backpressure_rebases = self.rebases,
				gso_sends = self.gso,
				per_shard_sends = self.per_shard,
				max_send_us = duration_micros_u64(self.max_send),
				send_us = duration_micros_u64(self.send_total) / self.frames,
				max_pacing_lateness_us = duration_micros_u64(self.max_lateness),
				max_packet_queue = self.max_queue,
				packet_queue = queue,
				"Video transport summary"
			);
			*self = Self::new(self.enabled);
		}
	}
}

pub(super) struct PipelineWindow {
	enabled: bool,
	started: Instant,
	frames: u64,
	stale: u64,
	encoded_bytes: u64,

	sums: [u64; 9],
	maxima: [u64; 9],
}

impl PipelineWindow {
	pub fn new(enabled: bool) -> Self {
		Self {
			enabled,
			started: Instant::now(),
			frames: 0,
			stale: 0,
			encoded_bytes: 0,

			sums: [0; 9],
			maxima: [0; 9],
		}
	}

	pub fn record(&mut self, stats: &FrameStats, in_flight: usize, packet_queue: usize, imports: Option<usize>) {
		if !self.enabled {
			return;
		}
		self.frames += 1;
		self.stale += u64::from(stats.stale_frames_dropped);
		self.encoded_bytes += stats.encoded_bytes as u64;

		for (i, duration) in [
			stats.channel_wait,
			stats.import,
			stats.convert,
			stats.submit,
			stats.consumer_queue,
			stats.encode_wait,
			stats.packetize,
			stats.send,
			stats.total,
		]
		.into_iter()
		.enumerate()
		{
			let us = duration_micros_u64(duration);
			self.sums[i] = self.sums[i].saturating_add(us);
			self.maxima[i] = self.maxima[i].max(us);
		}
		let elapsed = self.started.elapsed();
		if elapsed >= INTERVAL {
			let avg = self.sums.map(|us| us / self.frames);
			tracing::info!(
				encoded_frames = self.frames, fps = self.frames as f64 / elapsed.as_secs_f64(),
				encoded_mbps = self.encoded_bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1e6,
				stale_frames_dropped = self.stale, encoder_in_flight = in_flight,
				packet_queue, import_cache = ?imports,
				channel_wait_us = avg[0], max_channel_wait_us = self.maxima[0],
				import_us = avg[1], max_import_us = self.maxima[1],
				convert_us = avg[2], max_convert_us = self.maxima[2],
				submit_us = avg[3], max_submit_us = self.maxima[3],
				consumer_queue_us = avg[4], max_consumer_queue_us = self.maxima[4],
				queue_to_readback_us = avg[5], max_queue_to_readback_us = self.maxima[5],
				packetize_us = avg[6], max_packetize_us = self.maxima[6],
				channel_send_us = avg[7], max_channel_send_us = self.maxima[7],
				total_us = avg[8], max_total_us = self.maxima[8],
				"Video pipeline summary"
			);
			*self = Self::new(self.enabled);
		}
	}
}

/// Runs independently of frame completion, so a stuck encode/send still leaves
/// a process/resource sample. A weak sender cannot keep the packet channel alive.
pub(super) fn spawn_watchdog(
	stop: ShutdownManager<SessionShutdownReason>,
	packets: mpsc::WeakSender<VideoPacketMessage>,
) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(INTERVAL);
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
		let mut previous: Option<(u64, Instant)> = None;
		while let Ok(scheduled) = stop.wrap_cancel(interval.tick()).await {
			let now = Instant::now();
			let runtime_lateness_us = duration_micros_u64(scheduled.elapsed());
			let packet_queue = packets.upgrade().map(|tx| tx.max_capacity() - tx.capacity());
			let cpu_ticks = std::fs::read_to_string("/proc/self/stat")
				.ok()
				.and_then(|stat| process_cpu_ticks(&stat));
			let cpu_percent = cpu_ticks.zip(previous).map(|(ticks, (old, sampled))| {
				ticks.saturating_sub(old) as f64 / ticks_per_second / now.duration_since(sampled).as_secs_f64() * 100.0
			});
			previous = cpu_ticks.map(|ticks| (ticks, now));
			let open_fds = std::fs::read_dir("/proc/self/fd")
				.ok()
				.map(|fds| fds.count().saturating_sub(1));
			let rss_kib = std::fs::read_to_string("/proc/self/status").ok().and_then(|status| {
				status.lines().find_map(|line| {
					line.strip_prefix("VmRSS:")?
						.split_whitespace()
						.next()?
						.parse::<u64>()
						.ok()
				})
			});
			tracing::info!(
				?cpu_percent,
				?open_fds,
				?rss_kib,
				?packet_queue,
				runtime_lateness_us,
				"Video runtime health"
			);
		}
	});
}

fn process_cpu_ticks(stat: &str) -> Option<u64> {
	// comm may contain spaces and parentheses. Field 3 starts after its final ')'.
	let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
	let user = fields.get(11)?.parse::<u64>().ok()?;
	let system = fields.get(12)?.parse::<u64>().ok()?;
	Some(user.saturating_add(system))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn enabled_stats_resume_accumulation_after_summary_reset() {
		let mut window = TransportWindow::new(true);
		window.started = Instant::now() - INTERVAL;
		window.record(&SendStats::default(), 0);
		assert_eq!(window.frames, 0);
		assert!(window.enabled);
		window.record(&SendStats::default(), 1);
		assert_eq!(window.frames, 1);
		assert_eq!(window.max_queue, 1);
	}

	#[test]
	fn disabled_transport_stats_do_not_accumulate_or_emit() {
		let mut window = TransportWindow::new(false);
		window.started = Instant::now() - INTERVAL;
		let original_start = window.started;
		let stats = SendStats {
			would_block_events: 3,
			fallback_chunks: 2,
			elapsed: Duration::from_millis(1),
			..Default::default()
		};
		for _ in 0..10_000 {
			window.record(&stats, 128);
		}
		assert_eq!(window.frames, 0);
		assert_eq!(window.would_block, 0);
		assert_eq!(window.max_queue, 0);
		// A summary would reset the window's start time.
		assert_eq!(window.started, original_start);
	}

	#[test]
	fn disabled_pipeline_stats_leave_benchmark_samples_unchanged() {
		let mut window = PipelineWindow::new(false);
		window.started = Instant::now() - INTERVAL;
		let original_start = window.started;
		let stats = FrameStats {
			channel_wait: Duration::from_micros(1),
			import: Duration::from_micros(2),
			convert: Duration::from_micros(3),
			submit: Duration::from_micros(4),
			consumer_queue: Duration::from_micros(5),
			encode_wait: Duration::from_micros(6),
			packetize: Duration::from_micros(7),
			enqueue: Duration::ZERO,
			send: Duration::from_micros(8),
			total: Duration::from_micros(36),
			encoded_bytes: 100,
			wire_bytes: 120,
			packet_count: 10,
			attempted_packet_count: 10,
			failed_packet_count: 0,
			discarded_packet_count: 0,
			stale_frames_dropped: 2,
			is_key_frame: true,
		};
		for _ in 0..10_000 {
			window.record(&stats, 3, 128, Some(3));
		}
		assert_eq!(window.frames, 0);
		assert_eq!(window.stale, 0);
		assert_eq!(window.sums, [0; 9]);
		assert_eq!(window.maxima, [0; 9]);
		assert_eq!(window.started, original_start);
		assert_eq!(stats.encoded_bytes, 100);
		assert_eq!(stats.total, Duration::from_micros(36));
	}

	#[test]
	fn process_cpu_parser_handles_spaces_and_parentheses_in_comm() {
		assert_eq!(
			process_cpu_ticks("1 (a tricky) name) R 0 0 0 0 0 0 0 0 0 0 123 45 0"),
			Some(168)
		);
		assert_eq!(process_cpu_ticks("1 (incomplete) R"), None);
	}
}
