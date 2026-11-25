// The browser stream: a Moonshine client in the control panel.
//
// WebTransport carries the stream (see ui/src/stream.rs for the protocol): video and audio as
// datagrams, control messages and input on one bidirectional stream. WebCodecs decodes the video
// (drawn on a canvas) and the audio (played through an AudioWorklet). Input goes to the server as
// Moonlight input packets, so the server handles it as it handles Moonlight's.
//
// app.js opens a stream with `window.MoonshineStream.open(...)`.

import { FrameShards, parseVideoDatagram } from "/fec.js";

const CLIENT = { start: 1, input: 2, keyFrame: 3, stop: 4, ping: 5 };
const SERVER = { ready: 1, feedback: 2, ended: 3, pong: 4, error: 5 };
const DATAGRAM = { video: 1, audio: 2 };

// A frame that is still incomplete this long after a newer frame arrived is lost.
const LOSS_TIMEOUT_MS = 100;
// How often to ask again for a key frame while the stream waits for one.
const KEY_FRAME_RETRY_MS = 250;
// The audio buffer target.
const AUDIO_TARGET_S = 0.03;

// Moonlight input packet types.
const INPUT = {
  keyDown: 0x03, keyUp: 0x04, mouseAbsolute: 0x05, mouseRelative: 0x07, buttonDown: 0x08, buttonUp: 0x09,
  scroll: 0x0a, hscroll: 0x55000001, touch: 0x55000002, gamepadArrival: 0x55000004, gamepadUpdate: 0x0c,
};
const RUMBLE_DATA = 0x010b;

// Trackpad mode for touch screens (as in Moonlight): one finger moves the mouse relative to where
// it is, a tap clicks, a two-finger tap right-clicks, two fingers scroll, and a tap followed by a
// touch that stays down drags.
const TAP_MS = 250; // A touch shorter than this, that does not move, is a tap.
const TAP_SLOP_PX = 10; // How far a tap can move.
const DRAG_GAP_MS = 300; // A touch this soon after a tap starts a drag.
const SCROLL_SCALE = 2; // Scroll units per CSS pixel of two-finger movement.

const isTouchDevice = () => navigator.maxTouchPoints > 0 && Boolean(window.matchMedia?.("(pointer: coarse)").matches);

// KeyboardEvent.code to Windows virtual-key code (what Moonlight sends).
const VK = (() => {
  const map = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0d, NumpadEnter: 0x0d, Pause: 0x13, CapsLock: 0x14, Escape: 0x1b, Space: 0x20,
    PageUp: 0x21, PageDown: 0x22, End: 0x23, Home: 0x24, ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
    PrintScreen: 0x2c, Insert: 0x2d, Delete: 0x2e, MetaLeft: 0x5b, MetaRight: 0x5c, NumpadMultiply: 0x6a, NumpadAdd: 0x6b,
    NumpadComma: 0x6c, NumpadSubtract: 0x6d, NumpadDecimal: 0x6e, NumpadDivide: 0x6f, NumLock: 0x90, ScrollLock: 0x91,
    ShiftLeft: 0xa0, ShiftRight: 0xa1, ControlLeft: 0xa2, ControlRight: 0xa3, AltLeft: 0xa4, AltRight: 0xa5,
    Semicolon: 0xba, Equal: 0xbb, Comma: 0xbc, Minus: 0xbd, Period: 0xbe, Slash: 0xbf, Backquote: 0xc0,
    BracketLeft: 0xdb, Backslash: 0xdc, BracketRight: 0xdd, Quote: 0xde, IntlBackslash: 0xe2,
  };
  for (let i = 0; i < 26; i++) map["Key" + String.fromCharCode(65 + i)] = 0x41 + i;
  for (let i = 0; i < 10; i++) { map["Digit" + i] = 0x30 + i; map["Numpad" + i] = 0x60 + i; }
  for (let i = 1; i <= 24; i++) map["F" + i] = 0x6f + i;
  return map;
})();

// Gamepad API "standard" mapping to Moonlight button flags (bit 16 and up go in the second field).
const PAD_BUTTONS = [
  0x1000, 0x2000, 0x4000, 0x8000, // A B X Y
  0x0100, 0x0200, 0, 0, // LB RB (LT and RT are analog)
  0x0020, 0x0010, 0x0040, 0x0080, // back start LS RS
  0x0001, 0x0002, 0x0004, 0x0008, // dpad up down left right
  0x0400, 0x100000, // home, touchpad
];

const CODEC_TESTS = {
  hevc: "hvc1.1.6.L153.B0",
  h264: "avc1.640033",
  av1: "av01.0.13M.08",
};

// 10-bit (HDR) profiles: HEVC Main 10, AV1 Main at 10 bits.
const CODEC_TESTS_10BIT = {
  hevc: "hvc1.2.4.L153.B0",
  av1: "av01.0.13M.10",
};

/// The codecs this browser decodes (in 10 bits with `tenBit`), best (hardware) first.
async function decodableCodecs(tenBit = false) {
  if (typeof VideoDecoder === "undefined") return [];
  const found = [];
  for (const acceleration of ["prefer-hardware", "no-preference"]) {
    for (const [name, codec] of Object.entries(tenBit ? CODEC_TESTS_10BIT : CODEC_TESTS)) {
      if (found.includes(name)) continue;
      try {
        const { supported } = await VideoDecoder.isConfigSupported({ codec, hardwareAcceleration: acceleration, optimizeForLatency: true });
        if (supported) found.push(name);
      } catch { /* Not supported. */ }
    }
  }
  return found;
}

/// What is missing for streaming in this browser, or null.
function missingFeatures() {
  if (!window.isSecureContext) return "Open the control panel over HTTPS to stream. Browsers allow streaming only on secure pages.";
  if (typeof WebTransport === "undefined") return "This browser has no WebTransport. Use a recent Chrome, Edge, Firefox or Safari.";
  if (typeof VideoDecoder === "undefined") return "This browser has no WebCodecs video decoder. Use a recent Chrome, Edge, Firefox or Safari.";
  return null;
}

// ---------- Bitstream helpers ----------

/// The NAL units of an Annex B bitstream: [type byte offset, end offset].
function* nalUnits(data) {
  let i = 0, start = -1;
  while (i + 3 <= data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && (data[i + 2] === 1 || (data[i + 2] === 0 && data[i + 3] === 1))) {
      const header = i + (data[i + 2] === 1 ? 3 : 4);
      if (start >= 0) yield [start, i];
      start = header;
      i = header;
    } else i++;
  }
  if (start >= 0) yield [start, data.length];
}

/// Remove emulation prevention bytes (00 00 03).
function unescape(nal) {
  const out = [];
  for (let i = 0; i < nal.length; i++) {
    if (i >= 2 && nal[i] === 3 && nal[i - 1] === 0 && nal[i - 2] === 0) continue;
    out.push(nal[i]);
  }
  return out;
}

const hex = (n, width = 2) => n.toString(16).toUpperCase().padStart(width, "0");

/// The WebCodecs codec string for a key frame, from its parameter sets.
function codecString(codec, data, tenBit) {
  if (codec === "h264") {
    for (const [s, e] of nalUnits(data)) {
      if ((data[s] & 0x1f) === 7 && e - s >= 4) return `avc1.${hex(data[s + 1])}${hex(data[s + 2])}${hex(data[s + 3])}`.toLowerCase();
    }
    return CODEC_TESTS.h264;
  }
  if (codec === "hevc") {
    for (const [s, e] of nalUnits(data)) {
      if (((data[s] >> 1) & 0x3f) !== 33) continue; // SPS
      const sps = unescape(data.subarray(s + 2, Math.min(e, s + 40)));
      if (sps.length < 13) break;
      const profile = sps[1] & 0x1f, tier = (sps[1] >> 5) & 1, level = sps[12];
      return `hvc1.${profile}.${profile === 2 ? 4 : 6}.${tier ? "H" : "L"}${level}.B0`;
    }
    return tenBit ? CODEC_TESTS_10BIT.hevc : CODEC_TESTS.hevc;
  }
  return tenBit ? CODEC_TESTS_10BIT.av1 : CODEC_TESTS.av1;
}

// ---------- Moonlight input packets ----------

function packet(type, size) {
  const buffer = new ArrayBuffer(4 + size);
  const view = new DataView(buffer);
  view.setUint32(0, type >>> 0, true);
  return { buffer, view };
}

function keyPacket(down, vk, modifiers) {
  const { buffer, view } = packet(down ? INPUT.keyDown : INPUT.keyUp, 6);
  view.setUint16(5, vk, true);
  view.setUint8(7, modifiers);
  return buffer;
}

function modifiersOf(e) {
  return (e.shiftKey ? 0x01 : 0) | (e.ctrlKey ? 0x02 : 0) | (e.altKey ? 0x04 : 0) | (e.metaKey ? 0x08 : 0);
}

// For each channel in Vorbis order (what an OpusHead mapping uses), its index in WAVE order (what
// the server's mapping uses, and what WebAudio and the decoders output). Tested in Chromium and
// Firefox: with this, channel n of the server comes out as channel n.
const WAVE_OF_VORBIS = { 6: [0, 2, 1, 4, 5, 3], 8: [0, 2, 1, 6, 7, 4, 5, 3] };

/// The OpusHead (RFC 7845) for a multistream layout, mapping family 1.
function opusHead(params) {
  const channels = params.audio_channels;
  const head = new Uint8Array(21 + channels), view = new DataView(head.buffer);
  head.set([..."OpusHead"].map(c => c.charCodeAt(0)));
  head[8] = 1; // version
  head[9] = channels;
  view.setUint16(10, 0, true); // pre-skip
  view.setUint32(12, params.audio_sample_rate, true);
  view.setInt16(16, 0, true); // output gain
  head[18] = 1; // mapping family: Vorbis order
  head[19] = params.audio_streams;
  head[20] = params.audio_coupled_streams;
  head.set(WAVE_OF_VORBIS[channels].map(w => params.audio_mapping[w]), 21);
  return head;
}

const clamp16 = v => Math.max(-32768, Math.min(32767, Math.round(v)));

// ---------- Renderers ----------

/// Draws frames on a 2D canvas. The browser converts each frame from its color space (in the
/// bitstream) to the canvas's sRGB: an HDR (PQ) frame is tone-mapped to SDR. An HDR renderer
/// (WebGPU with extended range, or a <video> element) can replace it with the same interface.
class CanvasRenderer {
  constructor(canvas) {
    this.canvas = canvas;
    this.context = canvas.getContext("2d", { alpha: false, desynchronized: true });
  }

  draw(frame) {
    this.context.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
  }
}

// ---------- The player ----------

class Player {
  constructor(options) {
    this.options = options;
    this.closed = false;
    this.held = new Set();
    this.buttons = new Set();
    this.pads = new Map(); // gamepad.index -> { slot, state }
    this.stats = { frames: 0, lost: 0, recovered: 0, bytes: 0, decodeMs: [], rtt: null, since: performance.now(), keyRequests: 0 };
    this.totals = { frames: 0, audioPackets: 0 };
    this.frames = new Map();
    this.next = null;
    this.waitingKey = true;
    this.lastKeyRequest = 0;
    this.decodeStart = new Map();
    this.audioSequence = null;
    this.listeners = [];
    this.touchMode = localStorage.getItem("stream-touch-mode") === "direct" ? "direct" : "trackpad";
    this.touches = new Map(); // pointerId -> { x, y } (CSS pixels), for trackpad mode
    this.gesture = null;
    this.lastTap = null;
    this.rest = { x: 0, y: 0, scroll: 0 };
  }

  // -- UI --

  buildUi() {
    const root = document.createElement("div");
    root.className = "stream-root";
    root.innerHTML = `
      <canvas class="stream-video" tabindex="0" aria-label="The stream"></canvas>
      <div class="stream-edge" aria-hidden="true"></div>
      <div class="stream-bar" role="toolbar" aria-label="Stream">
        <span class="stream-title"></span>
        <button class="btn sm" data-act="fullscreen" title="Ctrl+Alt+Shift+X">Full screen</button>
        <button class="btn sm" data-act="touch" hidden></button>
        <button class="btn sm" data-act="stats">Statistics</button>
        <button class="btn sm" data-act="disconnect" title="Ctrl+Alt+Shift+Q">Disconnect</button>
        <button class="btn sm danger" data-act="stop">Stop session</button>
      </div>
      <div class="stream-status" role="status"></div>
      <pre class="stream-stats" hidden></pre>
      <p class="stream-hint" hidden></p>`;
    document.body.appendChild(root);
    document.documentElement.classList.add("streaming");
    this.root = root;
    this.canvas = root.querySelector(".stream-video");
    this.status = root.querySelector(".stream-status");
    this.statsBox = root.querySelector(".stream-stats");
    root.querySelector(".stream-title").textContent = this.options.title || "Stream";
    // Touch screens have no hover: a tap on the top edge shows the bar, a tap on the picture hides it.
    root.querySelector(".stream-edge").addEventListener("pointerdown", () => root.classList.add("bar-open"));
    this.canvas.addEventListener("pointerdown", () => root.classList.remove("bar-open"), true);
    root.querySelector(".stream-bar").addEventListener("click", e => {
      const action = e.target.closest("button")?.dataset.act;
      if (action === "fullscreen") this.toggleFullscreen();
      if (action === "stats") this.statsBox.hidden = !this.statsBox.hidden;
      if (action === "touch") this.setTouchMode(this.touchMode === "trackpad" ? "direct" : "trackpad");
      if (action === "disconnect") this.close("You disconnected. The session keeps running for a while; join it again from the control panel.");
      if (action === "stop") { this.send(CLIENT.stop); this.close("The session stopped."); }
    });
    this.setStatus("Connecting…");
    if (isTouchDevice()) this.root.querySelector('[data-act="touch"]').hidden = false;
    this.setTouchMode(this.touchMode);
  }

  /// "trackpad": touch moves the mouse like a laptop's touchpad. "direct": touches go to the
  /// computer as touches, where they are on the picture.
  setTouchMode(mode) {
    this.endGesture(false);
    this.touchMode = mode;
    localStorage.setItem("stream-touch-mode", mode);
    const button = this.root.querySelector('[data-act="touch"]');
    button.textContent = mode === "trackpad" ? "Touch: trackpad" : "Touch: direct";
    button.title = mode === "trackpad"
      ? "Touch moves the mouse like a touchpad. Tap to click, tap with two fingers to right-click, drag two fingers to scroll, tap and then hold to drag."
      : "Touches go to the computer where they are on the picture.";
  }

  setStatus(text) {
    this.status.textContent = text || "";
    this.status.hidden = !text;
  }

  hint(text) {
    const el = this.root.querySelector(".stream-hint");
    el.textContent = text;
    el.hidden = !text;
  }

  async toggleFullscreen() {
    if (document.fullscreenElement) { await document.exitFullscreen(); return; }
    await this.enterFullscreen(false);
  }

  /// Full screen, and on a phone or tablet also landscape for a wide stream. Must run in a user
  /// gesture. `quiet`: no hint when it fails (the automatic attempt when the stream opens).
  async enterFullscreen(quiet) {
    if (!this.root.requestFullscreen) {
      if (!quiet) this.hint("This browser cannot show the stream in full screen. On an iPhone, you can try to add the control panel to the home screen and open it from there (not tested).");
      return;
    }
    try {
      await this.root.requestFullscreen({ navigationUI: "hide" });
    } catch (e) {
      if (!quiet) this.hint(`Full screen failed: ${e.message}`);
      return;
    }
    // Chromium: capture system keys (Alt+Tab, Escape) while in full screen.
    await navigator.keyboard?.lock?.().catch(() => {});
    // Browsers lock the orientation only in full screen, and only on devices that rotate.
    const wide = (this.params?.width ?? this.options.width ?? 16) >= (this.params?.height ?? this.options.height ?? 9);
    if (!isTouchDevice() || !wide) return;
    let locked = false;
    try {
      await screen.orientation.lock("landscape");
      locked = true;
    } catch { /* Safari and desktop browsers cannot lock it. */ }
    if (!locked && window.matchMedia?.("(orientation: portrait)").matches) {
      this.hint("Turn the device sideways for a larger picture.");
      const clear = () => { if (!window.matchMedia("(orientation: portrait)").matches) this.hint(""); };
      this.on(window, "resize", clear);
    }
  }

  // -- Connection --

  async open() {
    this.buildUi();
    // An AudioContext and full screen must start in a user gesture: start them now, before the
    // first await.
    try {
      this.audioContext = new AudioContext({ latencyHint: "interactive", sampleRate: 48000 });
    } catch {
      this.audioContext = null;
    }
    if (this.options.fullscreen) this.enterFullscreen(true);

    const missing = missingFeatures();
    if (missing) return this.fail(missing);

    const codecs = await decodableCodecs();
    const codecs10 = await decodableCodecs(true);
    if (!codecs.length) return this.fail("This browser can decode none of the video codecs. Check that hardware decoding is on.");

    let token;
    try {
      const response = await fetch("/api/stream/token", { method: "POST", headers: { "X-Moonshine-UI": "1" } });
      token = await response.json();
      if (!response.ok) throw new Error(token.error || `The request failed (${response.status}).`);
    } catch (e) {
      return this.fail(`Moonshine refused the stream: ${e.message}`);
    }

    const options = {};
    if (token.certificate_hash) {
      options.serverCertificateHashes = [{ algorithm: "sha-256", value: new Uint8Array(token.certificate_hash) }];
    }
    options.congestionControl = "low-latency";
    // Behind a reverse proxy, the stream does not go through the proxy: Moonshine names its own address.
    let host = token.host || location.hostname;
    if (host.includes(":") && !host.startsWith("[")) host = `[${host}]`;
    try {
      this.transport = new WebTransport(`https://${host}:${token.port}/stream?token=${token.token}`, options);
      await this.transport.ready;
    } catch (e) {
      const message = e.message || String(e);
      if (e.name === "SecurityError" || /Content Security Policy/i.test(message)) {
        return this.fail(`The page cannot connect to ${host}:${token.port} (${message}). Set stream_host in the [ui] table of config.toml.`);
      }
      return this.fail(`The stream connection to ${host}:${token.port} failed (${message}). Make sure that this device can reach that address, and that no firewall blocks UDP port ${token.port}. To use another address, set stream_host in the [ui] table of config.toml.`);
    }
    this.transport.closed.then(() => this.close("The connection closed.")).catch(() => this.close("The connection was lost."));

    const control = await this.transport.createBidirectionalStream();
    this.writer = control.writable.getWriter();
    this.readControl(control.readable.getReader());

    try { this.transport.datagrams.incomingHighWaterMark = 4096; } catch { /* Not in every browser. */ }
    this.readDatagrams(this.transport.datagrams.readable.getReader());

    this.setStatus("Starting the application…");
    this.send(CLIENT.start, new TextEncoder().encode(JSON.stringify({
      application_id: this.options.applicationId ?? 0,
      width: this.options.width,
      height: this.options.height,
      fps: this.options.fps,
      bitrate_kbps: this.options.bitrateKbps,
      audio_channels: this.options.audioChannels ?? 2,
      hdr: Boolean(this.options.hdr),
      codecs,
      codecs_10bit: codecs10,
    })));
  }

  async onReady(params) {
    this.params = params;
    this.canvas.width = params.width;
    this.canvas.height = params.height;
    this.renderer = new CanvasRenderer(this.canvas);
    this.root.querySelector(".stream-title").textContent =
      `${this.options.title || "Stream"} · ${params.width}×${params.height} @ ${params.fps} · ${params.codec.toUpperCase()}`;
    await this.setupAudio(params).catch(e => this.hint(`No audio: ${e.message}`));
    this.bindInput();
    this.timers = [
      setInterval(() => this.ping(), 1000),
      setInterval(() => this.checkLoss(), 20),
      setInterval(() => this.renderStats(), 500),
    ];
    this.pollGamepads();
    this.setStatus("Waiting for the first frame…");
    this.send(CLIENT.start);
    this.canvas.focus();
  }

  send(kind, payload = new Uint8Array(0)) {
    if (!this.writer || this.closed) return;
    const bytes = payload instanceof ArrayBuffer ? new Uint8Array(payload) : payload;
    const message = new Uint8Array(5 + bytes.length);
    new DataView(message.buffer).setUint32(0, bytes.length + 1, true);
    message[4] = kind;
    message.set(bytes, 5);
    this.writer.write(message).catch(() => {});
  }

  sendInput(buffer) { this.send(CLIENT.input, buffer); }

  ping() {
    const payload = new Uint8Array(8);
    new DataView(payload.buffer).setFloat64(0, performance.now(), true);
    this.send(CLIENT.ping, payload);
  }

  async readControl(reader) {
    let pending = new Uint8Array(0);
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        const joined = new Uint8Array(pending.length + value.length);
        joined.set(pending); joined.set(value, pending.length);
        pending = joined;
        while (pending.length >= 4) {
          const length = new DataView(pending.buffer, pending.byteOffset).getUint32(0, true);
          if (pending.length < 4 + length) break;
          this.onControl(pending[4], pending.subarray(5, 4 + length));
          pending = pending.slice(4 + length);
        }
      }
    } catch { /* The connection closed. */ }
  }

  onControl(kind, payload) {
    const text = () => new TextDecoder().decode(payload);
    if (kind === SERVER.ready) this.onReady(JSON.parse(text()));
    else if (kind === SERVER.error) this.fail(text());
    else if (kind === SERVER.ended) this.close(text());
    else if (kind === SERVER.pong) {
      const sent = new DataView(payload.buffer, payload.byteOffset).getFloat64(0, true);
      this.stats.rtt = performance.now() - sent;
    } else if (kind === SERVER.feedback) this.onFeedback(payload);
  }

  async readDatagrams(reader) {
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        // For tests: drop a share of the video datagrams, as a bad network would.
        if (value[0] === DATAGRAM.video && this.options.testLoss && Math.random() < this.options.testLoss) continue;
        if (value[0] === DATAGRAM.video) this.onVideoChunk(value);
        else if (value[0] === DATAGRAM.audio) this.onAudio(value);
      }
    } catch { /* The connection closed. */ }
  }

  // -- Video --

  onVideoChunk(d) {
    if (!this.params) return;
    const h = parseVideoDatagram(d);
    const number = h.number;
    this.stats.bytes += d.byteLength;

    // The server reset the stream (another client attached before): frame numbers restart at 1.
    if (this.next !== null && number + 64 < this.next && (h.flags & 1)) {
      this.frames.clear();
      this.next = null;
    }
    if (this.next !== null && number < this.next) return;

    let frame = this.frames.get(number);
    if (!frame) {
      frame = { number, shards: new FrameShards(h), key: (h.flags & 1) !== 0, timestamp: h.timestamp, first: performance.now() };
      this.frames.set(number, frame);
    }
    if (frame.complete) return;
    frame.last = performance.now();
    if (frame.shards.add(h)) {
      frame.complete = true;
      this.pump();
    }
  }

  /// Decode the complete frames in order.
  pump() {
    for (;;) {
      if (this.waitingKey) {
        // Start again at the newest complete key frame.
        let key = null;
        for (const f of this.frames.values()) if (f.complete && f.key && (!key || f.number > key.number)) key = f;
        if (!key) return;
        for (const n of [...this.frames.keys()]) if (n < key.number) this.frames.delete(n);
        this.next = key.number;
        this.waitingKey = false;
      }
      const frame = this.frames.get(this.next);
      if (!frame || !frame.complete) return;
      this.frames.delete(this.next);
      this.next++;
      this.decode(frame);
    }
  }

  /// Declare lost the frames that block decoding for too long.
  checkLoss() {
    if (this.waitingKey) {
      if (performance.now() - this.lastKeyRequest > KEY_FRAME_RETRY_MS) this.requestKeyFrame();
      return;
    }
    if (this.next === null) return;
    const blocking = this.frames.get(this.next);
    let newer = null;
    for (const f of this.frames.values()) if (f.number > this.next && (!newer || f.first < newer.first)) newer = f;
    if (!newer) return;
    // A newer frame arrived, and the next one is still not complete.
    if (performance.now() - newer.first > LOSS_TIMEOUT_MS && !(blocking && blocking.complete)) {
      this.stats.lost++;
      this.waitingKey = true;
      this.requestKeyFrame();
      this.pump();
    }
  }

  requestKeyFrame() {
    this.lastKeyRequest = performance.now();
    this.stats.keyRequests++;
    this.send(CLIENT.keyFrame);
  }

  decode(frame) {
    let data;
    try {
      data = frame.shards.assemble();
    } catch (e) {
      console.warn("A frame could not be put together:", e);
      this.waitingKey = true;
      this.requestKeyFrame();
      return;
    }
    this.stats.recovered += frame.shards.recovered;

    if (!this.decoder || this.decoder.state === "closed") {
      if (!frame.key) { this.waitingKey = true; return; }
      this.createDecoder(codecString(this.params.codec, data, this.params.hdr));
    }
    this.decodeStart.set(frame.timestamp, performance.now());
    try {
      this.decoder.decode(new EncodedVideoChunk({ type: frame.key ? "key" : "delta", timestamp: frame.timestamp, data }));
    } catch (e) {
      console.warn("Decoding failed:", e);
      this.resetDecoder();
    }
  }

  createDecoder(codec) {
    this.decoder = new VideoDecoder({
      output: videoFrame => this.render(videoFrame),
      error: e => { console.warn("The video decoder failed:", e); this.resetDecoder(); },
    });
    this.decoder.configure({ codec, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
    this.codec = codec;
  }

  resetDecoder() {
    try { this.decoder?.close(); } catch { /* Closed already. */ }
    this.decoder = null;
    this.waitingKey = true;
    this.requestKeyFrame();
  }

  render(videoFrame) {
    const started = this.decodeStart.get(videoFrame.timestamp);
    if (started !== undefined) {
      this.decodeStart.delete(videoFrame.timestamp);
      this.stats.decodeMs.push(performance.now() - started);
    }
    if (this.decodeStart.size > 64) this.decodeStart.clear();
    if (!this.closed) {
      const c = videoFrame.colorSpace;
      this.color = c ? `${c.primaries ?? "?"} ${c.transfer ?? "?"}${c.fullRange ? " full" : ""}` : null;
      this.renderer.draw(videoFrame);
      this.stats.frames++;
      if (++this.totals.frames === 1) this.setStatus("");
    }
    videoFrame.close();
  }

  // -- Audio --

  async setupAudio(params) {
    if (!this.audioContext) throw new Error("this browser cannot play audio here");
    if (typeof AudioDecoder === "undefined") throw new Error("this browser has no WebCodecs audio decoder");
    await this.audioContext.audioWorklet.addModule("/stream-audio.js");
    const destination = this.audioContext.destination;
    // WebAudio down-mixes 5.1 (and stereo) to fewer channels, but drops what it cannot place of
    // 7.1 (tested in Chromium and Firefox). So 7.1 on a smaller device is folded into 5.1 here:
    // the side channels go into the rear channels.
    const fold71 = params.audio_channels === 8 && (destination.maxChannelCount || 2) < 8;
    const channels = fold71 ? 6 : params.audio_channels;
    this.audioNode = new AudioWorkletNode(this.audioContext, "stream-audio", {
      numberOfInputs: 0,
      outputChannelCount: [channels],
      processorOptions: { channels, target: AUDIO_TARGET_S },
    });
    // More channels than the device has: the WebAudio down-mix ("speakers") folds them in.
    destination.channelCount = Math.min(channels, destination.maxChannelCount || 2);
    destination.channelInterpretation = "speakers";
    this.audioNode.connect(destination);
    this.audioDecoder = new AudioDecoder({
      output: data => {
        const planes = [];
        for (let c = 0; c < data.numberOfChannels; c++) {
          const plane = new Float32Array(data.numberOfFrames);
          data.copyTo(plane, { planeIndex: c, format: "f32-planar" });
          planes.push(plane);
        }
        data.close();
        if (fold71 && planes.length === 8) {
          const [rl, rr, sl, sr] = [planes[4], planes[5], planes[6], planes[7]];
          for (let i = 0; i < rl.length; i++) { rl[i] += Math.SQRT1_2 * sl[i]; rr[i] += Math.SQRT1_2 * sr[i]; }
          planes.length = 6;
        }
        this.audioNode.port.postMessage(planes, planes.map(p => p.buffer));
      },
      error: e => this.hint(`The audio decoder failed: ${e.message}`),
    });
    const config = { codec: "opus", sampleRate: params.audio_sample_rate, numberOfChannels: params.audio_channels };
    if (params.audio_channels > 2) config.description = opusHead(params);
    if (!(await AudioDecoder.isConfigSupported(config)).supported) {
      throw new Error(`this browser cannot decode ${params.audio_channels}-channel audio`);
    }
    this.audioDecoder.configure(config);
    if (this.audioContext.state === "suspended") {
      // Some browsers (Safari) start audio only in a gesture: resume on the next one.
      const resume = () => this.audioContext.resume();
      this.audioContext.resume().catch(() => {});
      this.on(this.root, "pointerdown", resume);
      this.on(document, "keydown", resume);
    }
  }

  onAudio(d) {
    if (!this.audioDecoder || this.audioDecoder.state !== "configured") return;
    const view = new DataView(d.buffer, d.byteOffset, d.byteLength);
    const timestamp = Number(view.getBigUint64(5, true));
    this.totals.audioPackets++;
    try {
      this.audioDecoder.decode(new EncodedAudioChunk({ type: "key", timestamp, data: d.subarray(13) }));
    } catch { /* A bad packet: skip it. */ }
  }

  // -- Input --

  on(target, type, handler, options) {
    target.addEventListener(type, handler, options);
    this.listeners.push([target, type, handler, options]);
  }

  bindInput() {
    const canvas = this.canvas;
    this.on(document, "keydown", e => this.onKey(e, true), true);
    this.on(document, "keyup", e => this.onKey(e, false), true);
    this.on(window, "blur", () => this.releaseAll());
    this.on(document, "visibilitychange", () => { if (document.hidden) this.releaseAll(); });

    this.on(canvas, "pointerdown", e => this.onPointer(e, "down"));
    this.on(canvas, "pointerup", e => this.onPointer(e, "up"));
    this.on(canvas, "pointercancel", e => this.onPointer(e, "cancel"));
    this.on(canvas, "pointermove", e => this.onPointer(e, "move"));
    this.on(canvas, "contextmenu", e => e.preventDefault());
    this.on(canvas, "wheel", e => this.onWheel(e), { passive: false });
    this.on(document, "pointerlockchange", () => {
      if (document.pointerLockElement !== canvas) this.releaseButtons();
    });
  }

  onKey(e, down) {
    // Stream shortcuts, as in Moonlight.
    if (down && e.ctrlKey && e.altKey && e.shiftKey) {
      if (e.code === "KeyQ") { e.preventDefault(); this.close("You disconnected."); return; }
      if (e.code === "KeyZ") { e.preventDefault(); document.exitPointerLock?.(); return; }
      if (e.code === "KeyS") { e.preventDefault(); this.statsBox.hidden = !this.statsBox.hidden; return; }
      if (e.code === "KeyX") { e.preventDefault(); this.toggleFullscreen(); return; }
    }
    const vk = VK[e.code];
    if (vk === undefined) return;
    e.preventDefault();
    e.stopPropagation();
    if (down) {
      if (e.repeat && this.held.has(vk)) return;
      this.held.add(vk);
    } else {
      if (!this.held.delete(vk)) return;
    }
    this.sendInput(keyPacket(down, vk, modifiersOf(e)));
  }

  releaseAll() {
    for (const vk of this.held) this.sendInput(keyPacket(false, vk, 0));
    this.held.clear();
    this.releaseButtons();
  }

  releaseButtons() {
    for (const button of this.buttons) {
      const { buffer, view } = packet(INPUT.buttonUp, 1);
      view.setUint8(4, button);
      this.sendInput(buffer);
    }
    this.buttons.clear();
  }

  /// The pointer position in stream pixels, or null outside the picture.
  streamPosition(e) {
    const rect = this.canvas.getBoundingClientRect();
    const scale = Math.min(rect.width / this.canvas.width, rect.height / this.canvas.height);
    const width = this.canvas.width * scale, height = this.canvas.height * scale;
    const x = (e.clientX - rect.left - (rect.width - width) / 2) / scale;
    const y = (e.clientY - rect.top - (rect.height - height) / 2) / scale;
    return { x: Math.max(0, Math.min(this.canvas.width - 1, x)), y: Math.max(0, Math.min(this.canvas.height - 1, y)) };
  }

  onPointer(e, phase) {
    if (!this.params) return;
    if (e.pointerType === "touch" && this.touchMode === "trackpad") return this.onTrackpad(e, phase);
    if (e.pointerType === "touch" || e.pointerType === "pen") return this.onTouch(e, phase);
    const locked = document.pointerLockElement === this.canvas;

    if (phase === "move") {
      if (locked) {
        const { buffer, view } = packet(INPUT.mouseRelative, 4);
        view.setInt16(4, clamp16(e.movementX), false);
        view.setInt16(6, clamp16(e.movementY), false);
        this.sendInput(buffer);
      } else {
        this.sendAbsolute(e);
      }
      return;
    }

    if (phase === "down" && !locked && e.button === 0 && this.canvas.requestPointerLock) {
      // The first click captures the mouse; games need relative motion.
      this.sendAbsolute(e);
      const lock = this.canvas.requestPointerLock({ unadjustedMovement: true });
      if (lock?.catch) lock.catch(() => this.canvas.requestPointerLock());
    }
    const button = [1, 2, 3, 4, 5][e.button];
    if (button === undefined || phase === "cancel") return;
    if (phase === "down") this.buttons.add(button); else if (!this.buttons.delete(button)) return;
    const { buffer, view } = packet(phase === "down" ? INPUT.buttonDown : INPUT.buttonUp, 1);
    view.setUint8(4, button);
    this.sendInput(buffer);
    e.preventDefault();
  }

  sendAbsolute(e) {
    const { x, y } = this.streamPosition(e);
    const { buffer, view } = packet(INPUT.mouseAbsolute, 10);
    view.setInt16(4, Math.round(x), false);
    view.setInt16(6, Math.round(y), false);
    view.setInt16(10, this.canvas.width, false);
    view.setInt16(12, this.canvas.height, false);
    this.sendInput(buffer);
  }

  onWheel(e) {
    e.preventDefault();
    const unit = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1.2;
    if (e.deltaY) {
      const { buffer, view } = packet(INPUT.scroll, 2);
      view.setInt16(4, clamp16(-e.deltaY * unit), false);
      this.sendInput(buffer);
    }
    if (e.deltaX) {
      const { buffer, view } = packet(INPUT.hscroll, 2);
      view.setInt16(4, clamp16(e.deltaX * unit), false);
      this.sendInput(buffer);
    }
  }

  // -- Trackpad mode --

  mouseButton(down, button) {
    if (down) this.buttons.add(button); else if (!this.buttons.delete(button)) return;
    const { buffer, view } = packet(down ? INPUT.buttonDown : INPUT.buttonUp, 1);
    view.setUint8(4, button);
    this.sendInput(buffer);
  }

  click(button) {
    this.mouseButton(true, button);
    this.mouseButton(false, button);
  }

  /// Stream pixels per CSS pixel of the picture on screen: a swipe across the picture moves the
  /// mouse across the stream.
  streamScale() {
    const rect = this.canvas.getBoundingClientRect();
    return 1 / Math.min(rect.width / this.canvas.width, rect.height / this.canvas.height);
  }

  onTrackpad(e, phase) {
    e.preventDefault();
    const now = performance.now();
    if (phase === "down") {
      this.canvas.setPointerCapture?.(e.pointerId);
      this.touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (!this.gesture) {
        // A touch soon after a tap, near it, drags (the left button stays down).
        const tap = this.lastTap;
        const drag = tap && now - tap.at < DRAG_GAP_MS && Math.hypot(e.clientX - tap.x, e.clientY - tap.y) < 4 * TAP_SLOP_PX;
        this.gesture = { start: now, x: e.clientX, y: e.clientY, fingers: 1, moved: false, drag };
        this.lastTap = null;
        if (drag) this.mouseButton(true, 1);
      } else {
        this.gesture.fingers = Math.max(this.gesture.fingers, this.touches.size);
      }
      return;
    }

    const touch = this.touches.get(e.pointerId);
    if (!touch || !this.gesture) return;
    if (phase === "move") {
      const dx = e.clientX - touch.x, dy = e.clientY - touch.y;
      touch.x = e.clientX;
      touch.y = e.clientY;
      if (Math.hypot(e.clientX - this.gesture.x, e.clientY - this.gesture.y) > TAP_SLOP_PX) this.gesture.moved = true;
      if (this.touches.size === 1) {
        const scale = this.streamScale();
        this.rest.x += dx * scale;
        this.rest.y += dy * scale;
        const mx = Math.trunc(this.rest.x), my = Math.trunc(this.rest.y);
        if (mx || my) {
          this.rest.x -= mx;
          this.rest.y -= my;
          const { buffer, view } = packet(INPUT.mouseRelative, 4);
          view.setInt16(4, clamp16(mx), false);
          view.setInt16(6, clamp16(my), false);
          this.sendInput(buffer);
        }
      } else if (this.touches.size === 2) {
        // Two fingers scroll, as on a touchpad: the content follows the fingers.
        this.rest.scroll += (dy / 2) * SCROLL_SCALE;
        const amount = Math.trunc(this.rest.scroll);
        if (amount) {
          this.rest.scroll -= amount;
          const { buffer, view } = packet(INPUT.scroll, 2);
          view.setInt16(4, clamp16(amount), false);
          this.sendInput(buffer);
        }
      }
      return;
    }

    // Up or cancel.
    this.touches.delete(e.pointerId);
    if (this.touches.size) return;
    this.endGesture(phase === "up");
  }

  /// The last finger lifted: a short touch that did not move is a click.
  endGesture(clicks) {
    const gesture = this.gesture;
    this.gesture = null;
    this.touches.clear();
    this.rest = { x: 0, y: 0, scroll: 0 };
    if (!gesture) return;
    if (gesture.drag) { this.mouseButton(false, 1); return; }
    if (!clicks || gesture.moved || performance.now() - gesture.start > TAP_MS) return;
    if (gesture.fingers === 1) {
      this.click(1);
      this.lastTap = { at: performance.now(), x: gesture.x, y: gesture.y };
    } else if (gesture.fingers === 2) {
      this.click(3);
    }
  }

  onTouch(e, phase) {
    e.preventDefault();
    const kind = { down: 0x01, up: 0x02, move: 0x03, cancel: 0x04 }[phase];
    const { x, y } = this.streamPosition(e);
    const { buffer, view } = packet(INPUT.touch, 28);
    view.setUint8(4, kind);
    view.setUint32(8, e.pointerId >>> 0, true);
    view.setFloat32(12, x / this.canvas.width, true);
    view.setFloat32(16, y / this.canvas.height, true);
    view.setFloat32(20, e.pressure || 0.5, true);
    this.sendInput(buffer);
  }

  // -- Gamepads --

  pollGamepads() {
    if (this.closed) return;
    const pads = navigator.getGamepads ? [...navigator.getGamepads()] : [];
    const seen = new Set();
    for (const pad of pads) {
      if (!pad || !pad.connected) continue;
      seen.add(pad.index);
      let entry = this.pads.get(pad.index);
      if (!entry) {
        const used = new Set([...this.pads.values()].map(p => p.slot));
        let slot = 0;
        while (used.has(slot)) slot++;
        if (slot >= 4) continue;
        entry = { slot, state: "" };
        this.pads.set(pad.index, entry);
        this.sendArrival(pad, slot);
      }
      this.sendPadState(pad, entry);
    }
    for (const [index, entry] of this.pads) {
      if (!seen.has(index)) {
        this.pads.delete(index);
        this.sendPadUpdate(entry.slot, 0, 0, 0, [0, 0, 0, 0]);
      }
    }
    requestAnimationFrame(() => this.pollGamepads());
  }

  activeMask() {
    let mask = 0;
    for (const entry of this.pads.values()) mask |= 1 << entry.slot;
    return mask;
  }

  sendArrival(pad, slot) {
    const id = pad.id.toLowerCase();
    const kind = /054c|playstation|dualsense|dualshock/.test(id) ? 2 : /057e|nintendo|pro controller/.test(id) ? 3 : 1;
    const { buffer, view } = packet(INPUT.gamepadArrival, 8);
    view.setUint8(4, slot);
    view.setUint8(5, kind);
    view.setUint16(6, 0x01 | (pad.vibrationActuator ? 0x02 : 0), true); // analog triggers, rumble
    view.setUint32(8, 0x0017ffff, true);
    this.sendInput(buffer);
  }

  sendPadState(pad, entry) {
    if (pad.mapping !== "standard") return;
    let flags = 0;
    pad.buttons.forEach((b, i) => { if (b.pressed && PAD_BUTTONS[i]) flags |= PAD_BUTTONS[i]; });
    const lt = Math.round((pad.buttons[6]?.value || 0) * 255), rt = Math.round((pad.buttons[7]?.value || 0) * 255);
    const axes = [pad.axes[0] || 0, -(pad.axes[1] || 0), pad.axes[2] || 0, -(pad.axes[3] || 0)].map(v => clamp16(v * 32767));
    const state = `${flags},${lt},${rt},${axes}`;
    if (state === entry.state) return;
    entry.state = state;
    this.sendPadUpdate(entry.slot, flags, lt, rt, axes);
  }

  sendPadUpdate(slot, flags, lt, rt, axes) {
    const { buffer, view } = packet(INPUT.gamepadUpdate, 26);
    view.setUint16(4, 0x001a, true);
    view.setUint16(6, slot, true);
    view.setUint16(8, this.activeMask(), true);
    view.setUint16(10, 0x0014, true);
    view.setUint16(12, flags & 0xffff, true);
    view.setUint8(14, lt);
    view.setUint8(15, rt);
    axes.forEach((v, i) => view.setInt16(16 + i * 2, v, true));
    view.setUint16(24, 0x009c, true);
    view.setUint16(26, (flags >>> 16) & 0xffff, true);
    view.setUint16(28, 0x0055, true);
    this.sendInput(buffer);
  }

  onFeedback(payload) {
    const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
    if (payload.byteLength < 14 || view.getUint16(0, true) !== RUMBLE_DATA) return;
    const slot = view.getUint16(8, true), low = view.getUint16(10, true), high = view.getUint16(12, true);
    for (const [index, entry] of this.pads) {
      if (entry.slot !== slot) continue;
      const actuator = navigator.getGamepads()[index]?.vibrationActuator;
      if (!actuator) return;
      if (!low && !high) actuator.reset?.();
      else actuator.playEffect?.("dual-rumble", { duration: 5000, strongMagnitude: low / 65535, weakMagnitude: high / 65535 }).catch(() => {});
    }
  }

  // -- Statistics and end --

  renderStats() {
    if (this.statsBox.hidden) { this.resetStats(); return; }
    const seconds = (performance.now() - this.stats.since) / 1000;
    const decode = this.stats.decodeMs.length ? this.stats.decodeMs.reduce((a, b) => a + b, 0) / this.stats.decodeMs.length : NaN;
    this.statsBox.textContent = [
      `Video     ${this.params.width}×${this.params.height} ${this.codec || this.params.codec}${this.params.hdr ? " 10-bit" : ""}`,
      `Color     ${this.color || "–"}`,
      `Frames    ${(this.stats.frames / seconds).toFixed(1)} fps of ${this.params.fps}`,
      `Bitrate   ${(this.stats.bytes * 8 / seconds / 1e6).toFixed(1)} Mb/s`,
      `Decode    ${Number.isFinite(decode) ? decode.toFixed(1) + " ms" : "–"}`,
      `Round trip ${this.stats.rtt !== null ? this.stats.rtt.toFixed(1) + " ms" : "–"}`,
      `Lost      ${this.stats.lost} frames, ${this.stats.keyRequests} key frame requests`,
      `FEC       ${this.stats.recovered} lost packets rebuilt`,
    ].join("\n");
    this.resetStats();
  }

  resetStats() {
    Object.assign(this.stats, { frames: 0, bytes: 0, decodeMs: [], since: performance.now() });
  }

  fail(message) {
    this.close(message, true);
  }

  close(message, failed = false) {
    if (this.closed) return;
    this.closed = true;
    try { this.releaseAll(); } catch { /* The connection is gone. */ }
    for (const [target, type, handler, options] of this.listeners) target.removeEventListener(type, handler, options);
    for (const timer of this.timers || []) clearInterval(timer);
    this.writer?.close().catch(() => {});
    try { this.transport?.close(); } catch { /* Closed already. */ }
    try { this.decoder?.close(); } catch { /* Closed already. */ }
    try { this.audioDecoder?.close(); } catch { /* Closed already. */ }
    this.audioContext?.close().catch(() => {});
    if (document.pointerLockElement) document.exitPointerLock();
    navigator.keyboard?.unlock?.();
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    try { screen.orientation?.unlock?.(); } catch { /* Not locked. */ }
    this.root?.remove();
    document.documentElement.classList.remove("streaming");
    this.options.onClose?.(message, failed);
  }
}

let current = null;
let speakers = null;

/// How many audio channels the output device takes (2 when it is not known).
function speakerChannels() {
  if (speakers === null) {
    try {
      const context = new AudioContext();
      speakers = context.destination.maxChannelCount || 2;
      context.close();
    } catch {
      speakers = 2;
    }
  }
  return speakers;
}

window.MoonshineStream = {
  missingFeatures,
  decodableCodecs,
  speakerChannels,
  /// Open a stream. Options: applicationId, title, width, height, fps, bitrateKbps, audioChannels,
  /// hdr, fullscreen (try full screen at once: call `open` in a user gesture), onClose(message, failed).
  open(options) {
    current?.close("Another stream started.");
    current = new Player(options);
    current.open().catch(e => current.fail(String(e.message || e)));
    return current;
  },
};
