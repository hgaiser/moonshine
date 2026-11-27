// The stream page (/play): choose what to stream, then the player (stream.js) runs on this page.
"use strict";

const $ = s => document.querySelector(s);
const esc = s => String(s ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const RESOLUTIONS = [[1280, 720], [1920, 1080], [2560, 1440], [3840, 2160]];

/// Moonlight's default bitrate (20 Mb/s at 1080p60), scaled with the pixel rate.
function autoBitrate(w, h, fps) {
  return Math.max(5000, Math.min(150000, Math.round(20000 * (w * h * fps) / (1920 * 1080 * 60) / 1000) * 1000));
}

function toast(message, ms = 4200) {
  const el = document.createElement("div");
  el.className = "toast";
  el.textContent = message;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), ms);
}

// The color theme that the control panel chose.
try {
  const theme = localStorage.getItem("moonshine-theme") || "auto";
  if (theme !== "auto") document.documentElement.setAttribute("data-theme", theme);
} catch { /* Storage blocked. */ }

// The last choices, in this browser: { app, size ("native" or [w, h]), fps, audioMode ("auto", 2, 6
// or 8; "audio" was the old fixed choice and is ignored), hdrMode ("auto"
// or "off"), codec ("auto", "hevc", "h264" or "av1"), fullscreen, stats, bitrate ({ kbps, width, height, fps } only when the user changed
// the default), mouse ("raw" or "accelerated"), apps ({ [app id]: the advanced options to use for
// that application }) }.
const CHOICES = "moonshine-stream-choices";
function loadChoices() {
  try { return JSON.parse(localStorage.getItem(CHOICES)) || {}; } catch { return {}; }
}
function saveChoices(choices) {
  try { localStorage.setItem(CHOICES, JSON.stringify(choices)); } catch { /* Storage blocked or full. */ }
}

let message = null;
let playing = false;
let shown = null; // What the form shows: redrawn only when this changes, to keep the user's choices.
let picked = null; // The application that the user selected in the form.

async function load() {
  if (playing) return;
  let snap;
  try {
    snap = await (await fetch("/api/snapshot")).json();
  } catch (e) {
    shown = null;
    show(`<p class="err">The control panel did not answer (${esc(e.message)}). It tries again.</p>`);
    return;
  }
  const state = JSON.stringify([snap.server?.browser_streaming, snap.session && [snap.session.application, snap.session.device, snap.session.phase, snap.session.browser_rejoin, snap.session.width, snap.session.height, snap.session.fps], (snap.library || []).map(a => a.id), message]);
  if (state === shown) return;
  shown = state;
  render(snap, picked);
}

// A session can start or stop elsewhere (Moonlight, the control panel): follow it.
setInterval(load, 3000);

function show(html) {
  $("#play-form").innerHTML = `<h2 id="play-title">play</h2>${html}`;
}

/// Draw the form. `pick` is the application that the user selected; `keep` holds what the form
/// keeps when it is drawn again for another application.
function render(snap, pick = null, keep = null) {
  const player = window.MoonshineStream;
  if (!player) { show(`<p class="err">The stream player did not load. Reload the page.</p>`); return; }
  if (snap.server?.browser_streaming === false) {
    show(`<p>Streaming in the browser is off. To turn it on, set <code>streaming = true</code> in the [ui] table of config.toml, then restart Moonshine.</p>`);
    return;
  }

  const missing = player.missingFeatures();
  // A browser that lost its stream can attach again to its session. Any other session has to stop
  // first.
  const rejoin = Boolean(snap.session?.browser_rejoin);
  if (snap.session && !rejoin) {
    const s = snap.session;
    show(`
      ${message ? `<p class="note">${esc(message)}</p>` : ""}
      <p>${esc(s.device || "Another device")} ${s.phase === "streaming" ? "streams" : "starts"} ${esc(s.application || "an application")} now. To start a stream here, stop that stream first. The application closes.</p>
      <div class="sheet-actions"><button type="button" class="btn danger" id="s-stop">Stop the stream</button></div>`);
    $("#s-stop").onclick = async () => {
      const button = $("#s-stop");
      if (!button.dataset.sure) {
        button.dataset.sure = "1";
        button.textContent = "Stop the stream: the application closes";
        return;
      }
      button.disabled = true;
      try {
        const res = await fetch("/api/action", {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-Moonshine-UI": "1" },
          body: JSON.stringify({ action: "stop_session" }),
        });
        const data = await res.json().catch(() => ({}));
        if (!res.ok || data.error) throw new Error(data.error || `The request failed (${res.status}).`);
      } catch (e) {
        toast(e.message, 6000);
        button.disabled = false;
      }
      shown = null;
      load();
    };
    return;
  }
  const session = snap.session;
  const apps = snap.library || [];
  const saved = loadChoices();
  // A link with ?app= wins over the last choice.
  let wanted = pick ?? new URLSearchParams(location.search).get("app") ?? (saved.app != null ? String(saved.app) : null);
  if (!apps.some(a => String(a.id) === wanted)) wanted = apps.length ? String(apps[0].id) : null;
  // The options saved for this application, else the options of the last stream.
  const own = rejoin ? null : saved.apps?.[wanted] ?? null;
  const last = own ? { ...saved, ...own } : saved;
  const fullscreenOn = keep ? keep.fullscreen : saved.fullscreen !== false;
  const dpr = window.devicePixelRatio || 1;
  // Landscape, also on a phone held upright: the player turns the device to landscape where it can.
  const long = Math.max(screen.width, screen.height), short = Math.min(screen.width, screen.height);
  const native = [Math.round(long * dpr) & ~1, Math.round(short * dpr) & ~1];
  const sizes = [native, ...RESOLUTIONS.filter(r => r[0] !== native[0] || r[1] !== native[1])];
  const speakers = player.speakerChannels();
  // Automatic audio: the most channels that this device's speakers take.
  const autoChannels = speakers >= 8 ? 8 : speakers >= 6 ? 6 : 2;
  const audioMode = [2, 6, 8].includes(last.audioMode) ? last.audioMode : "auto";
  const AUDIO = [[2, "Stereo"], [6, "5.1 surround"], [8, "7.1 surround"]];
  const audioName = n => AUDIO.find(a => a[0] === n)[1];
  // "native" follows this screen; a saved size that is not in the list falls back to it.
  const sizeIndex = Array.isArray(last.size) ? Math.max(0, sizes.findIndex(r => r[0] === last.size[0] && r[1] === last.size[1])) : 0;
  const lastFps = [30, 60, 120].includes(last.fps) ? last.fps : 60;
  const hdrMode = last.hdrMode === "off" ? "off" : "auto";
  const lastCodec = ["hevc", "h264", "av1"].includes(last.codec) ? last.codec : "auto";
  const mouse = last.mouse === "accelerated" ? "accelerated" : "raw";

  show(`
    ${message ? `<p class="note">${esc(message)}</p>` : ""}
    ${missing ? `<p class="err">${esc(missing)}</p>` : ""}
    ${rejoin ? `<p class="muted">${esc(session.application || "A session")} runs at ${session.width} × ${session.height} @ ${session.fps}. A browser streamed it and lost the connection. Reconnect to continue the stream here.</p>` :
      `<div class="field"><label for="s-app">Application</label><select id="s-app">${apps.map(a => `<option value="${a.id}" ${String(a.id) === wanted ? "selected" : ""}>${esc(a.title)}</option>`).join("")}</select></div>
`}
    <div class="field"><label class="switch"><input type="checkbox" id="s-full" ${fullscreenOn ? "checked" : ""}><span>Start in full screen</span></label><span class="help">On a phone or tablet, also turns the screen to landscape.</span></div>
    <details class="adv" ${keep?.open ? "open" : ""}><summary><strong>Advanced options</strong><span class="adv-sum muted" id="s-adv-sum"></span></summary><div style="display:grid;gap:14px">
      ${rejoin ? "" : `<div class="field"><label for="s-size">Resolution</label><select id="s-size">${sizes.map((r, i) => `<option value="${i}" ${i === sizeIndex ? "selected" : ""}>${r[0]} × ${r[1]}${i === 0 ? " (this screen)" : ""}</option>`).join("")}</select></div>`}
      ${rejoin ? "" : `<div class="field"><label for="s-fps">Frame rate</label><select id="s-fps">${[60, 120, 30].map(f => `<option value="${f}" ${f === lastFps ? "selected" : ""}>${f}</option>`).join("")}</select></div>`}
      <div class="field"><label for="s-rate">Bitrate (Mb/s)</label><input type="number" id="s-rate" min="1" max="500" step="1"><span class="help">The whole stream, with FEC and audio. The default follows Moonlight: 20 Mb/s at 1080p60.</span></div>
      ${rejoin ? "" : `<div class="field"><label for="s-hdr">HDR</label><select id="s-hdr"><option value="auto" ${hdrMode === "auto" ? "selected" : ""}>Automatic</option><option value="off" ${hdrMode === "off" ? "selected" : ""}>Off</option></select><span class="help" id="s-hdr-help"></span></div>`}
      ${rejoin ? "" : `<div class="field"><label for="s-codec">Video codec</label><select id="s-codec"><option value="auto">Automatic</option></select><span class="help" id="s-codec-help"></span></div>`}
      ${rejoin ? "" : `<div class="field"><label for="s-audio">Audio</label><select id="s-audio"><option value="auto">Automatic (${audioName(autoChannels)})</option>${AUDIO.map(([n, l]) => `<option value="${n}" ${n === audioMode ? "selected" : ""}>${l}</option>`).join("")}</select><span class="help">This device has ${speakers} audio channel${speakers === 1 ? "" : "s"}. Automatic selects the most channels that it takes. The browser mixes more channels down to fit.</span></div>`}
      <div class="field"><label for="s-mouse">Captured mouse</label><select id="s-mouse"><option value="raw" ${mouse === "raw" ? "selected" : ""}>Raw (for games)</option><option value="accelerated" ${mouse === "accelerated" ? "selected" : ""}>Accelerated (as on this device)</option></select><span class="help">The first click in the stream captures the mouse. Raw motion has no acceleration, for exact aim in games. Accelerated motion moves as the pointer of this device, for desktop work.</span></div>
      <div class="field"><label class="switch"><input type="checkbox" id="s-stats" ${last.stats ? "checked" : ""}><span>Show statistics</span></label><span class="help">Statistics on top of the stream: frame rate, bitrate, decode time, round trip and loss. Ctrl+Alt+Shift+S shows or hides them during the stream.</span></div>
      ${rejoin ? "" : `<div class="field"><label class="switch"><input type="checkbox" id="s-own" ${own ? "checked" : ""}><span>Remember these options for this application</span></label><span class="help">The next stream of this application uses these options again. Without this, it uses the options of the last stream.</span></div>`}
    </div></details>
    <div class="sheet-actions">
      <button type="submit" class="btn primary" id="s-go" ${missing || (!rejoin && !apps.length) ? "disabled" : ""}>${rejoin ? "Reconnect" : "Start streaming"}</button>
    </div>`);

  const size = () => rejoin ? [session.width, session.height] : sizes[Number($("#s-size")?.value || 0)];
  const fps = () => rejoin ? session.fps : Number($("#s-fps")?.value || 60);
  const channels = () => { const v = $("#s-audio")?.value; return !v || v === "auto" ? autoChannels : Number(v); };
  // The default bitrate follows the size and frame rate. A bitrate that the user typed comes back
  // only with the same size and frame rate.
  const update = () => {
    const [w, h] = size(), b = last.bitrate;
    const custom = b && b.width === w && b.height === h && b.fps === fps();
    $("#s-rate").value = (custom ? b.kbps : autoBitrate(w, h, fps())) / 1000;
    summary();
  };
  // The closed advanced options still show the stream's size, rate and bitrate.
  const summary = () => {
    const [w, h] = size();
    $("#s-adv-sum").textContent = `${w} × ${h} · ${fps()} fps · ${Number($("#s-rate").value) || 0} Mb/s`;
  };
  $("#s-rate").addEventListener("input", summary);
  update();
  $("#s-size")?.addEventListener("change", update);
  $("#s-fps")?.addEventListener("change", update);

  // The codecs that both this browser and this computer support. Automatic lets Moonshine select:
  // HEVC, then H.264, then AV1 (HEVC, then AV1 for HDR).
  const names = player.CODEC_NAMES;
  const codec = () => { const c = $("#s-codec")?.value; return c && c !== "auto" ? c : null; };
  const codecs = (async () => {
    const server = (snap.server?.codecs || []).map(n => Object.keys(names).find(c => names[c] === n));
    const [all, tenBit] = await Promise.all([player.decodableCodecs(), player.decodableCodecs(true)]);
    const select = $("#s-codec");
    if (select) {
      for (const c of ["hevc", "h264", "av1"].filter(c => all.includes(c) && server.includes(c))) {
        select.add(new Option(names[c], c, false, c === lastCodec));
      }
    }
    return tenBit;
  })();

  // Automatic HDR: an HDR stream when this computer, this browser and the codec can do it, else SDR.
  let hdr = false;
  const hdrHelp = async () => {
    const help = $("#s-hdr-help");
    if (!help) return;
    const tenBit = await codecs;
    const c = codec();
    const problem = !snap.server?.hdr ? "HDR is off on this computer (hdr in the [compositor] table of config.toml)"
      : c === "h264" ? "H.264 has no HDR"
      : c && !tenBit.includes(c) ? `this browser cannot decode 10-bit ${names[c]}`
      : await player.hdrProblem();
    hdr = !problem && $("#s-hdr").value === "auto";
    help.textContent = $("#s-hdr").value === "off" ? "The stream is SDR."
      : problem ? `The stream is SDR: ${problem}.`
      : `The stream is HDR (10-bit ${c ? names[c] : "HEVC or AV1"}).`;
  };
  const codecHelp = async () => {
    const help = $("#s-codec-help");
    if (!help) return;
    await codecs;
    const count = $("#s-codec").options.length - 1;
    help.textContent = codec() ? `The stream uses ${names[codec()]}.`
      : count ? "Moonshine selects the first codec that this browser and this computer support: HEVC, then H.264, then AV1."
      : "This browser and this computer have no codec in common.";
  };
  codecHelp();
  hdrHelp();
  $("#s-hdr")?.addEventListener("change", hdrHelp);
  $("#s-codec")?.addEventListener("change", () => { codecHelp(); hdrHelp(); });

  // Another application: draw the form again with its saved options.
  $("#s-app")?.addEventListener("change", () => render(snap, picked = $("#s-app").value, { open: $("details.adv").open, fullscreen: $("#s-full").checked }));

  // The player must open in the click: full screen and audio need its user gesture.
  $("#play-form").onsubmit = e => {
    e.preventDefault();
    const [width, height] = size();
    const app = apps.find(a => String(a.id) === $("#s-app")?.value);
    const fullscreen = Boolean($("#s-full")?.checked);
    const stats = Boolean($("#s-stats")?.checked);
    const mouseMode = $("#s-mouse")?.value === "accelerated" ? "accelerated" : "raw";
    const bitrateKbps = Math.round(Number($("#s-rate").value || 20) * 1000);
    const custom = bitrateKbps !== autoBitrate(width, height, fps());
    // A default bitrate removes only a typed bitrate for this size and frame rate.
    const same = last.bitrate && last.bitrate.width === width && last.bitrate.height === height && last.bitrate.fps === fps();
    const bitrate = custom ? { kbps: bitrateKbps, width, height, fps: fps() } : same ? null : last.bitrate ?? null;
    const choices = { ...saved, fullscreen };
    delete choices.hdr; // The old HDR switch.
    delete choices.audio; // The old fixed audio choice.
    if (rejoin) {
      Object.assign(choices, { stats, bitrate, mouse: mouseMode });
    } else {
      const index = Number($("#s-size")?.value || 0);
      const options = {
        size: index === 0 ? "native" : sizes[index],
        fps: fps(),
        bitrate,
        hdrMode: $("#s-hdr")?.value === "off" ? "off" : "auto",
        codec: codec() || "auto",
        audioMode: $("#s-audio")?.value === "auto" ? "auto" : Number($("#s-audio")?.value),
        stats,
        mouse: mouseMode,
      };
      choices.app = app ? app.id : null;
      // Options for this application only, or the options for the next stream of any application.
      const own = { ...saved.apps };
      if (app && $("#s-own")?.checked) {
        own[app.id] = options;
      } else {
        if (app) delete own[app.id];
        Object.assign(choices, options);
      }
      if (Object.keys(own).length) choices.apps = own; else delete choices.apps;
    }
    saveChoices(choices);
    message = null;
    playing = true;
    player.open({
      applicationId: app ? app.id : 0,
      title: app ? app.title : session?.application,
      width, height,
      fps: fps(),
      bitrateKbps,
      audioChannels: channels(),
      hdr,
      codec: rejoin ? null : codec(),
      rawMouse: mouseMode === "raw",
      fullscreen,
      stats,
      onClose: (text, failed) => {
        message = text || null;
        playing = false;
        shown = null;
        if (text && failed) toast(text, 9000);
        load();
      },
    });
  };
}

load();
