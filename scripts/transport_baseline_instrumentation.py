"""Measurement-only completion timestamps for the original transport.

No change to original admission, packetization, or pacing.
"""
from pathlib import Path

def instrument(root, current_root):
    p = root / 'moonshine-core/src/session/stream/video/shard_batch.rs'
    s = p.read_text()
    s = s.replace('\tsend_completion: Option', '\tbench_started: Option<std::time::Instant>,\n\tbench_submitted: usize,\n\tbench_observer: Option<Box<dyn FnOnce(std::time::Instant, std::time::Instant, usize, usize) + Send + Sync>>,\n\tsend_completion: Option')
    s = s.replace('send_completion: None,', 'send_completion: None,\n\t\t\tbench_started: None,\n\t\t\tbench_submitted: 0,\n\t\t\tbench_observer: None,')
    idx = s.index('\n\tpub fn set_send_completion')
    s = s[:idx] + "\n    \tpub fn bench_start(&mut self) { self.bench_started = Some(std::time::Instant::now()); }\n    \tpub fn bench_submitted(&mut self, count: usize) { self.bench_submitted = count; }\n    \tpub fn bench_observe(&mut self, observer: impl FnOnce(std::time::Instant, std::time::Instant, usize, usize) + Send + Sync + 'static) { self.bench_observer = Some(Box::new(observer)); }\n    " + s[idx:]
    needle = '\tpub fn notify_sent(&mut self) {\n'
    s = s.replace(needle, needle + '\t\tif let Some(observer) = self.bench_observer.take() {\n    \t\t\tif let Some(start) = self.bench_started {\n    \t\t\t\tobserver(start, std::time::Instant::now(), self.bench_submitted * self.shard_size, self.bench_submitted);\n    \t\t\t}\n    \t\t}\n    ')
    p.write_text(s)
    p = root / 'moonshine-core/src/session/stream/video/mod.rs'
    s = p.read_text().replace('pub send: std::time::Duration,', 'pub enqueue: std::time::Duration,\n\tpub send: std::time::Duration,')
    s = s.replace('// Sends are wrapped in wrap_cancel', 'batch.bench_start();\n\t\t\t\t\t\t\t\t// Sends are wrapped in wrap_cancel')
    s = s.replace('Ok(send_stats) => {', 'Ok(send_stats) => {\n\t\t\t\t\t\t\t\t\t\tlet submitted = (send_stats.gso_sends as usize * send_stats.gso_segments_per_send + send_stats.per_shard_sends as usize).min(batch.shard_count());\n\t\t\t\t\t\t\t\t\t\tbatch.bench_submitted(submitted);', 1)
    p.write_text(s)
    p = root / 'moonshine-core/src/session/stream/video/pipeline/mod.rs'
    s = p.read_text().replace('let shards = match packetizer.packetize(', 'let mut shards = match packetizer.packetize(', 1)
    idx = s.index('\t\tif packet_tx.send(VideoPacketMessage::Batch(shards)).await.is_err() {')
    s = s[:idx] + '\t\t// Measurement only: keep original admission lifetime and packetizer.\n    \t\tlet measured_stats_tx = stats_tx.clone();\n    \t\tshards.bench_observe(move |started, finished, submitted_bytes, submitted_packets| {\n    \t\t\tlet _ = measured_stats_tx.send(FrameStats {\n    \t\t\t\tchannel_wait: frame_context.channel_wait, import: frame_context.import,\n    \t\t\t\tconvert: frame_context.convert, submit: frame_context.submit,\n    \t\t\t\tconsumer_queue: consumer_queue_dur, encode_wait: encode_wait_dur,\n    \t\t\t\tpacketize: t_packetized - t_start, enqueue: started.saturating_duration_since(t_packetized),\n    \t\t\t\tsend: finished.saturating_duration_since(started), total: finished.saturating_duration_since(frame_context.created_at),\n    \t\t\t\tencoded_bytes, wire_bytes: submitted_bytes, packet_count: submitted_packets,\n    \t\t\t\tstale_frames_dropped: 0, is_key_frame,\n    \t\t\t});\n    \t\t});\n    ' + s[idx:]
    s = s.replace('\t\tlet _ = stats_tx.send(stats);', '', 1)
    s = s.replace('\t\t\tpacketize: packetize_dur,\n\t\t\tsend:', '\t\t\tpacketize: packetize_dur,\n\t\t\tenqueue: send_dur,\n\t\t\tsend:', 1)
    s = s.replace('send: sent.duration_since(packetized),', 'enqueue: std::time::Duration::ZERO,\n\t\t\t\tsend: sent.duration_since(packetized),')
    p.write_text(s)
    # Share the current stats reader; lifecycle cycles need newer manager APIs.
    bench = (current_root / 'moonshine-tools/src/bin/bench.rs').read_text()
    bench = bench.replace('#[path = "bench/cycles.rs"]\nmod cycles;\n', '')
    bench = bench.replace('''\tif args.cycles > 0 {
\t\tcycles::run_full_cycles(&args).await
\t} else if args.reconnect_cycles > 0 {
\t\tcycles::run_reconnect_cycles(&args).await
\t} else if args.matrix''', '\tif args.matrix')
    assert 'cycles::' not in bench
    (root / 'moonshine-tools/src/bin/bench.rs').write_text(bench)
    p = root / 'moonshine-core/src/session/stream/video/diagnostics.rs'
    s = p.read_text().replace('packetize: Duration::', 'enqueue: Duration::ZERO,\n\t\t\tpacketize: Duration::')
    p.write_text(s)
