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

// The last choices, in this browser: { app, size ("native" or [w, h]), fps, audio, hdr, fullscreen,
// bitrate ({ kbps, width, height, fps } only when the user changed the default) }.
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
  const state = JSON.stringify([snap.server?.browser_streaming, snap.session && [snap.session.application, snap.session.width, snap.session.height, snap.session.fps], (snap.library || []).map(a => a.id), message]);
  if (state === shown) return;
  shown = state;
  render(snap);
}

// A session can start or stop elsewhere (Moonlight, the control panel): follow it.
setInterval(load, 3000);

function show(html) {
  $("#play-form").innerHTML = `<h2 id="play-title">play</h2>${html}`;
}

function render(snap) {
  const player = window.MoonshineStream;
  if (!player) { show(`<p class="err">The stream player did not load. Reload the page.</p>`); return; }
  if (snap.server?.browser_streaming === false) {
    show(`<p>Streaming in the browser is off. To turn it on, set <code>streaming = true</code> in the [ui] table of config.toml, then restart Moonshine.</p>`);
    return;
  }

  const missing = player.missingFeatures();
  const join = Boolean(snap.session);
  const session = snap.session;
  const apps = snap.library || [];
  const last = loadChoices();
  // A link with ?app= wins over the last choice.
  const wanted = new URLSearchParams(location.search).get("app") ?? (last.app != null ? String(last.app) : null);
  const dpr = window.devicePixelRatio || 1;
  // Landscape, also on a phone held upright: the player turns the device to landscape where it can.
  const long = Math.max(screen.width, screen.height), short = Math.min(screen.width, screen.height);
  const native = [Math.round(long * dpr) & ~1, Math.round(short * dpr) & ~1];
  const sizes = [native, ...RESOLUTIONS.filter(r => r[0] !== native[0] || r[1] !== native[1])];
  const speakers = player.speakerChannels();
  const defaultChannels = [2, 6, 8].includes(last.audio) ? last.audio : speakers >= 8 ? 8 : speakers >= 6 ? 6 : 2;
  // "native" follows this screen; a saved size that is not in the list falls back to it.
  const sizeIndex = Array.isArray(last.size) ? Math.max(0, sizes.findIndex(r => r[0] === last.size[0] && r[1] === last.size[1])) : 0;
  const lastFps = [30, 60, 120].includes(last.fps) ? last.fps : 60;
  // HDR needs it on this computer and an HDR display here.
  const hdrPossible = Boolean(snap.server?.hdr) && window.matchMedia?.("(dynamic-range: high)").matches;

  show(`
    ${message ? `<p class="note">${esc(message)}</p>` : ""}
    ${missing ? `<p class="err">${esc(missing)}</p>` : ""}
    ${join ? `<p class="muted">${esc(session.application || "A session")} runs at ${session.width} × ${session.height} @ ${session.fps}. This browser takes it over; a Moonlight client that streams it now disconnects.</p>` :
      `<div class="field"><label for="s-app">Application</label><select id="s-app">${apps.map(a => `<option value="${a.id}" ${String(a.id) === wanted ? "selected" : ""}>${esc(a.title)}</option>`).join("")}</select></div>
    <div class="field"><label for="s-size">Resolution</label><select id="s-size">${sizes.map((r, i) => `<option value="${i}" ${i === sizeIndex ? "selected" : ""}>${r[0]} × ${r[1]}${i === 0 ? " (this screen)" : ""}</option>`).join("")}</select></div>
    <div class="field"><label for="s-fps">Frame rate</label><select id="s-fps">${[60, 120, 30].map(f => `<option value="${f}" ${f === lastFps ? "selected" : ""}>${f}</option>`).join("")}</select></div>
    <div class="field"><label for="s-audio">Audio</label><select id="s-audio">${[[2, "Stereo"], [6, "5.1 surround"], [8, "7.1 surround"]].map(([n, l]) => `<option value="${n}" ${n === defaultChannels ? "selected" : ""}>${l}</option>`).join("")}</select><span class="help">This device has ${speakers} audio channel${speakers === 1 ? "" : "s"}. The browser mixes more channels down to fit.</span></div>
    ${hdrPossible ? `<div class="field"><label class="switch"><input type="checkbox" id="s-hdr" ${last.hdr ? "checked" : ""}><span>HDR</span></label><span class="help">10-bit HEVC or AV1. The player draws on an SDR canvas, so HDR content shows as SDR. Not tested with HDR content yet.</span></div>` : ""}`}
    <div class="field"><label class="switch"><input type="checkbox" id="s-full" ${last.fullscreen === false ? "" : "checked"}><span>Start in full screen</span></label><span class="help">On a phone or tablet, also turns the screen to landscape.</span></div>
    <div class="field"><label for="s-rate">Bitrate (Mb/s)</label><input type="number" id="s-rate" min="1" max="500" step="1"><span class="help">The whole stream, with FEC and audio. The default follows Moonlight: 20 Mb/s at 1080p60.</span></div>
    <div class="sheet-actions">
      <button type="submit" class="btn primary" id="s-go" ${missing || (!join && !apps.length) ? "disabled" : ""}>${join ? "Join the session" : "Start streaming"}</button>
    </div>`);

  const size = () => join ? [session.width, session.height] : sizes[Number($("#s-size")?.value || 0)];
  const fps = () => join ? session.fps : Number($("#s-fps")?.value || 60);
  // The default bitrate follows the size and frame rate. A bitrate that the user typed comes back
  // only with the same size and frame rate.
  const update = () => {
    const [w, h] = size(), b = last.bitrate;
    const custom = b && b.width === w && b.height === h && b.fps === fps();
    $("#s-rate").value = (custom ? b.kbps : autoBitrate(w, h, fps())) / 1000;
  };
  update();
  $("#s-size")?.addEventListener("change", update);
  $("#s-fps")?.addEventListener("change", update);

  // The player must open in the click: full screen and audio need its user gesture.
  $("#play-form").onsubmit = e => {
    e.preventDefault();
    const [width, height] = size();
    const app = apps.find(a => String(a.id) === $("#s-app")?.value);
    const fullscreen = Boolean($("#s-full")?.checked);
    const bitrateKbps = Math.round(Number($("#s-rate").value || 20) * 1000);
    const custom = bitrateKbps !== autoBitrate(width, height, fps());
    // A default bitrate removes only a typed bitrate for this size and frame rate.
    const same = last.bitrate && last.bitrate.width === width && last.bitrate.height === height && last.bitrate.fps === fps();
    const bitrate = custom ? { kbps: bitrateKbps, width, height, fps: fps() } : same ? null : last.bitrate ?? null;
    const choices = { ...last, fullscreen, bitrate };
    if (!join) {
      const index = Number($("#s-size")?.value || 0);
      Object.assign(choices, {
        app: app ? app.id : null,
        size: index === 0 ? "native" : sizes[index],
        fps: fps(),
        audio: Number($("#s-audio")?.value || 2),
      });
      if ($("#s-hdr")) choices.hdr = $("#s-hdr").checked;
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
      audioChannels: Number($("#s-audio")?.value || 2),
      hdr: Boolean($("#s-hdr")?.checked),
      fullscreen,
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
