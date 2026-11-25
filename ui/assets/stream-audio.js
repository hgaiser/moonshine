// AudioWorklet for the browser stream: a ring buffer between the Opus decoder and the speakers.
// It keeps the buffer near a small target, so audio stays in sync with the low-latency video:
// when too much is buffered it drops the oldest samples, and when it runs empty it plays silence.
"use strict";

class StreamAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { channels, target } = options.processorOptions;
    this.channels = channels;
    this.capacity = Math.ceil(sampleRate * 0.5);
    this.target = Math.ceil(sampleRate * target);
    this.max = this.target * 4;
    this.buffers = Array.from({ length: channels }, () => new Float32Array(this.capacity));
    this.read = 0;
    this.write = 0;
    this.filled = 0;
    this.started = false;
    this.underruns = 0;
    this.port.onmessage = e => this.push(e.data);
  }

  push(planes) {
    const frames = planes[0].length;
    for (let c = 0; c < this.channels; c++) {
      const src = planes[Math.min(c, planes.length - 1)], dst = this.buffers[c];
      for (let i = 0; i < frames; i++) dst[(this.write + i) % this.capacity] = src[i];
    }
    this.write = (this.write + frames) % this.capacity;
    this.filled = Math.min(this.capacity, this.filled + frames);
    if (this.filled > this.max) {
      // Too far behind: jump to the target.
      const drop = this.filled - this.target;
      this.read = (this.read + drop) % this.capacity;
      this.filled -= drop;
    }
  }

  process(_inputs, outputs) {
    const out = outputs[0], frames = out[0].length;
    if (!this.started && this.filled >= this.target) this.started = true;
    if (!this.started || this.filled < frames) {
      for (const channel of out) channel.fill(0);
      if (this.started) { this.started = false; this.underruns++; this.port.postMessage({ underruns: this.underruns }); }
      return true;
    }
    for (let c = 0; c < out.length; c++) {
      const src = this.buffers[Math.min(c, this.channels - 1)], dst = out[c];
      for (let i = 0; i < frames; i++) dst[i] = src[(this.read + i) % this.capacity];
    }
    this.read = (this.read + frames) % this.capacity;
    this.filled -= frames;
    return true;
  }
}

registerProcessor("stream-audio", StreamAudio);
