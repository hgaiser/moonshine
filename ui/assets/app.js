// The Moonshine control panel. Data arrives over a WebSocket; changes go to POST /api/action.
"use strict";

(() => {
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const esc = s => String(s ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const css = n => getComputedStyle(document.documentElement).getPropertyValue(n).trim();
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const now = () => Date.now() / 1000;

function toast(msg, ms = 4200) {
  const t = document.createElement("div");
  t.className = "toast"; t.textContent = msg;
  $("#toasts").appendChild(t);
  setTimeout(() => t.remove(), ms);
}

/* ---------- Server connection ---------- */
let snap = null;
let connected = false;

async function act(action, body = {}) {
  try {
    const res = await fetch("/api/action", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Moonshine-UI": "1" },
      body: JSON.stringify({ action, ...body })
    });
    let data = {};
    try { data = await res.json(); } catch (e) { data = { error: await res.text().catch(() => "") }; }
    if (!res.ok || data.error) { toast(data.error || `The request failed (${res.status}).`, 6000); return { ok: false, error: data.error }; }
    if (data.message) toast(data.message);
    return { ok: true, message: data.message };
  } catch (e) {
    toast("Moonshine does not answer. Check that it runs.", 6000);
    return { ok: false, error: String(e) };
  }
}

function setLive(on, text) {
  connected = on;
  $("#live").classList.toggle("off", !on);
  $("#live-text").textContent = text || (on ? "Live" : "Reconnecting…");
}

function connect() {
  const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws");
  ws.onopen = () => setLive(true);
  ws.onmessage = e => {
    let m; try { m = JSON.parse(e.data); } catch (err) { return; }
    if (m.type === "snapshot") onSnapshot(m.data);
    else if (m.type === "stats_history") { stats.stream = m.stream; stats.buckets = m.buckets || []; renderSession(); }
    else if (m.type === "stats_start") { stats.stream = m.stream; stats.buckets = []; statsFollow = true; frozen = null; renderSession(); }
    else if (m.type === "bucket") { stats.buckets.push(m.data); if (stats.buckets.length > MAX) stats.buckets.shift(); drawAll(); }
    else if (m.type === "stats_stop") { stats.stream = null; renderSession(); }
    else if (m.type === "logs") addLogs(m.lines || []);
  };
  ws.onclose = () => { setLive(false); setTimeout(connect, 2000); };
}

/* ---------- Routing ---------- */
const PAGES = ["home", "clients", "apps", "config", "logs"];
function route() {
  let p = location.hash.slice(1);
  if (!PAGES.includes(p)) p = "home";
  PAGES.forEach(x => { $("#page-" + x).hidden = x !== p; });
  $$("nav.rail a").forEach(a => { if (a.dataset.page === p) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current"); });
  if (p === "home") requestAnimationFrame(drawAll);
  if (p === "logs") renderLogs();
}
addEventListener("hashchange", route);

/* ---------- Formatting ---------- */
const fmtDur = s => { s = Math.max(0, Math.floor(s)); const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = s % 60; return (h ? h + ":" : "") + String(m).padStart(2, "0") + ":" + String(x).padStart(2, "0"); };
const rel = ts => { if (!ts) return "never"; const s = now() - ts; return s < 60 ? "just now" : s < 3600 ? Math.floor(s / 60) + " min ago" : s < 86400 ? Math.floor(s / 3600) + " h ago" : Math.floor(s / 86400) + " d ago"; };
const uptime = ts => { const s = now() - ts; const d = Math.floor(s / 86400), h = Math.floor(s / 3600) % 24, m = Math.floor(s / 60) % 60; return d ? `${d} d ${h} h` : h ? `${h} h ${m} min` : `${m} min`; };
const channels = n => ({ 2: "Stereo", 6: "5.1 surround", 8: "7.1 surround" }[n] || n + " channels");

/* ---------- Snapshot ---------- */
const prev = {};
function changedPart(key, value) { if (same(prev[key], value)) return false; prev[key] = value; return true; }

function onSnapshot(s) {
  const old = snap; snap = s;
  if (old) diffEvents(old, s);
  document.title = s.server.name + " · Moonshine";
  $("#host-name").textContent = s.server.name;
  $("#rail-foot").innerHTML = `<span>Moonshine ${esc(s.server.version)}</span><span>Up ${uptime(s.server.started)}</span>`;
  if (changedPart("session", s.session)) renderSession();
  bannerFx(!!s.session && s.session.phase === "streaming");
  if (changedPart("health", s.health)) renderHealth();
  if (changedPart("server", s.server)) renderServer();
  if (changedPart("pairing", s.pairing)) renderPair();
  if (changedPart("devices", s.devices)) renderClients();
  if (changedPart("apps", [s.applications, s.scanners, s.scanned_at])) renderApps();
  if (changedPart("settings", s.settings)) syncSettings();
  if (changedPart("restart", s.restart_needed)) renderRestart();
  setNavCounts();
  if (!old) guide.start();
}

/* ---------- Session + stats ---------- */
const STAGES = [["wait", "Capture wait"], ["import", "Import"], ["convert", "Convert"], ["encode", "Encode"], ["packetize", "Packetize"], ["send", "Send"]];
const MAX = 240;
const stats = { stream: null, buckets: [] };
let statsFollow = true, frozen = null;
const view = () => statsFollow ? stats.buckets : frozen;
const hover = { lat: null, br: null, fps: null };

function renderSession() {
  const el = $("#session");
  if (!snap) return;
  const s = snap.session;
  if (!s) {
    el.innerHTML = `
      <div class="panel-head"><h2>Stream</h2><span class="pill plain">Idle</span></div>
      <div class="empty">
        <h3 style="font-size:18px;font-weight:600;margin:0">No one is streaming</h3>
        <p class="muted" style="margin:0;max-width:56ch">Open Moonlight on a paired device and select an application${snap.server?.browser_streaming === false ? "" : ", or select Play to stream to this browser"}. The live graphs for the stream show here.</p>
      </div>
      ${stats.buckets.length ? chartsHTML(true) : ""}`;
  } else {
    const st = s.stream || stats.stream;
    const who = esc(s.device || "A device");
    const title = s.phase === "streaming" ? `${who} is streaming ${esc(s.application)}` : `${who} is starting ${esc(s.application)}`;
    const pill = s.phase === "streaming" ? '<span class="pill ok">● Live</span>' : '<span class="pill info">Starting</span>';
    el.innerHTML = `
      <div class="session-top">
        <div class="session-title"><h3>${title}</h3>${pill}</div>
        <span style="display:flex;gap:8px"><button class="btn sm" id="join-browser">Play</button><button class="btn sm danger" id="stop">Stop stream</button></span>
      </div>
      <div id="stop-confirm"></div>
      <dl class="facts">
        <div><dt>Mode</dt><dd>${s.width} × ${s.height} @ ${s.fps}</dd></div>
        <div><dt>Video</dt><dd>${st ? esc(st.codec) + " · " + (st.hdr ? "HDR10" : "SDR") : s.hdr ? "HDR10" : "SDR"}</dd></div>
        <div><dt>Target bitrate</dt><dd>${st ? (st.bitrate_kbps / 1000).toFixed(0) + " Mb/s" : "—"}</dd></div>
        <div><dt>Audio</dt><dd>${channels(s.audio_channels)}</dd></div>
        <div><dt>Duration</dt><dd id="dur">${fmtDur(now() - s.started)}</dd></div>
      </dl>
      ${chartsHTML(false)}`;
    $("#stop").onclick = () => {
      $("#stop-confirm").innerHTML = `<div class="confirm"><span>Stop the stream? The application closes and Moonlight disconnects.</span>
        <button class="btn sm" id="stop-no">Keep streaming</button><button class="btn sm danger solid" id="stop-yes">Stop stream</button></div>`;
      $("#stop-no").onclick = () => { $("#stop-confirm").innerHTML = ""; };
      $("#stop-yes").onclick = async () => { $("#stop-yes").disabled = true; await act("stop_session"); };
    };
  }
  // Play in this browser: from the banner always, and next to Stop stream to take over a session.
  const off = snap.server?.browser_streaming === false;
  $("#play").hidden = off;
  const joinButton = $("#join-browser");
  if (joinButton && off) joinButton.remove();
  else if (joinButton) joinButton.onclick = () => { location.href = "/play"; };
  bindCharts();
  drawAll();
}

/* ---------- Stream in the browser ---------- */
function chartsHTML(idle) {
  const leg = STAGES.map(([k, l]) => `<span><i style="background:var(--s-${k})"></i>${l}</span>`).join("") + `<span><i style="background:var(--fg);height:2px"></i>Slowest frame</span>`;
  return `<div class="charts-head">
      <span class="muted">${idle ? "Last stream" : "Last 60 seconds"}${!idle && !statsFollow ? ' · <span class="paused">Paused. New data still arrives.</span>' : ""}</span>
      ${idle ? "" : `<label class="switch"><input type="checkbox" id="stats-follow" ${statsFollow ? "checked" : ""}><span>Follow</span></label>`}
    </div>
    <div class="charts"${idle ? ' style="opacity:.55"' : ""}>
    <div class="chart" data-c="lat">
      <div class="chart-head"><h3>Frame latency</h3><span class="readout" id="ro-lat"></span></div>
      <canvas id="c-lat" aria-label="Time of each pipeline stage over the last 60 seconds"></canvas>
      <div class="legend">${leg}</div>
    </div>
    <div class="chart" data-c="br">
      <div class="chart-head"><h3>Bitrate</h3><span class="readout" id="ro-br"></span></div>
      <canvas id="c-br" aria-label="Bitrate over the last 60 seconds, with keyframes marked"></canvas>
      <div class="legend"><span><i style="background:var(--data)"></i>Mb/s</span><span><i style="background:var(--accent)"></i>Keyframe</span></div>
    </div>
    <div class="chart" data-c="fps">
      <div class="chart-head"><h3>Frame rate</h3><span class="readout" id="ro-fps"></span></div>
      <canvas id="c-fps" aria-label="Frames per second against the target"></canvas>
      <div class="legend"><span><i style="background:var(--data)"></i>Sent fps, over 1 s</span><span><i style="background:var(--muted);height:2px"></i>Target</span></div>
    </div>
  </div>`;
}

function bindCharts() {
  const f = $("#stats-follow");
  if (f) f.onchange = () => {
    statsFollow = f.checked; frozen = statsFollow ? null : stats.buckets.slice();
    const head = f.closest(".charts-head").querySelector(".muted");
    head.innerHTML = "Last 60 seconds" + (statsFollow ? "" : ' · <span class="paused">Paused. New data still arrives.</span>');
    drawAll();
  };
  $$(".chart").forEach(ch => {
    const c = ch.dataset.c, cv = ch.querySelector("canvas");
    // Pointer events cover mouse, touch and pen. With touch, a horizontal swipe slides over the
    // graph and a vertical swipe still scrolls the page (touch-action: pan-y in the CSS).
    const pick = e => {
      const r = cv.getBoundingClientRect(), g = geom(r.width, +cv.dataset.x0 || 38), n = view().length;
      if (!n) return;
      const i = n - 1 - Math.round((g.x1 - (e.clientX - r.left)) / g.step);
      hover[c] = Math.max(0, Math.min(n - 1, i));
      draw(c);
    };
    const clear = () => { hover[c] = null; draw(c); const tip = ch.querySelector(".tip"); if (tip) tip.remove(); };
    cv.onpointerdown = e => { if (e.pointerType !== "mouse") cv.setPointerCapture(e.pointerId); pick(e); };
    cv.onpointermove = e => { if (e.pointerType === "mouse" || cv.hasPointerCapture(e.pointerId)) pick(e); };
    cv.onpointerleave = e => { if (e.pointerType === "mouse") clear(); };
    // The browser took the gesture over to scroll the page.
    cv.onpointercancel = clear;
    clearChart[c] = clear;
  });
}

// A touch selection stays after the finger lifts, so the value can be read. A tap elsewhere ends it.
const clearChart = {};
document.addEventListener("pointerdown", e => {
  if (e.pointerType !== "mouse" && !e.target.closest(".chart canvas")) Object.values(clearChart).forEach(f => f());
});

function geom(w, x0 = 38) { const x1 = w - 6; return { x0, x1, step: (x1 - x0) / (MAX - 1) }; }
function niceMax(v, steps) { for (const s of steps) if (v <= s) return s; return Math.ceil(v / 10) * 10; }
function setupCanvas(cv) {
  const dpr = devicePixelRatio || 1, w = cv.clientWidth, h = cv.clientHeight;
  if (!w) return null;
  cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr);
  const ctx = cv.getContext("2d"); ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  return { ctx, w, h };
}
function axes(ctx, w, h, max, ticks, unit, g) {
  const y0 = h - 18, y1 = 6;
  ctx.font = "11px " + css("--font-mono"); ctx.fillStyle = css("--muted"); ctx.strokeStyle = css("--grid"); ctx.lineWidth = 1;
  // Make the left margin as wide as the widest label, so no label is cut off.
  g.x0 = Math.max(38, Math.ceil(Math.max(0, ...ticks.map(v => ctx.measureText(v + unit).width))) + 10);
  g.step = (g.x1 - g.x0) / (MAX - 1);
  ticks.forEach(v => {
    const y = Math.round(y0 - (v / max) * (y0 - y1)) + .5;
    ctx.beginPath(); ctx.moveTo(g.x0, y); ctx.lineTo(g.x1, y); ctx.stroke();
    ctx.textAlign = "right"; ctx.textBaseline = "middle"; ctx.fillText(v + unit, g.x0 - 6, y);
  });
  ctx.textAlign = "center"; ctx.textBaseline = "alphabetic";
  [[-60, 0], [-45, .25], [-30, .5], [-15, .75]].forEach(([s, f]) => ctx.fillText(s + " s", g.x0 + (g.x1 - g.x0) * f, h - 3));
  ctx.textAlign = "right"; ctx.fillText("now", g.x1, h - 3);
  ctx.canvas.dataset.x0 = g.x0;
  return { y0, y1, Y: v => y0 - (Math.min(v, max) / max) * (y0 - y1) };
}
const xOf = (i, g) => g.x1 - (view().length - 1 - i) * g.step;
function hoverLine(ctx, x, a) { ctx.strokeStyle = css("--muted"); ctx.setLineDash([3, 3]); ctx.beginPath(); ctx.moveTo(x + .5, a.y1); ctx.lineTo(x + .5, a.y0); ctx.stroke(); ctx.setLineDash([]); }
function path(ctx, B, g, f) { B.forEach((b, i) => { const x = xOf(i, g), y = f(b, i); i ? ctx.lineTo(x, y) : ctx.moveTo(x, y); }); }

function draw(c) {
  const cv = $("#c-" + c); if (!cv) return;
  const S = setupCanvas(cv); if (!S) return;
  const { ctx, w, h } = S, B = view() || [], g = geom(w), hi = hover[c];
  const ro = $("#ro-" + c);
  if (!B.length) { axes(ctx, w, h, 1, [], "", g); ro.textContent = "Waiting for frames…"; return; }
  const last = B[B.length - 1], sel = hi != null ? B[hi] : last;
  if (c === "lat") {
    const max = niceMax(Math.max(...B.map(b => b.slowest.total), 1), [2, 4, 6, 8, 12, 16, 20, 24, 32, 40, 60, 80, 100, 120, 160, 200]);
    const ticks = [0, max / 4, max / 2, max * 3 / 4, max].map(v => +v.toFixed(1));
    const a = axes(ctx, w, h, max, ticks, " ms", g);
    let base = B.map(() => 0);
    STAGES.forEach(([k]) => {
      const top = B.map((b, i) => base[i] + (b[k] || 0));
      ctx.beginPath();
      path(ctx, B, g, (b, i) => a.Y(top[i]));
      for (let i = B.length - 1; i >= 0; i--) ctx.lineTo(xOf(i, g), a.Y(base[i]));
      ctx.closePath(); ctx.fillStyle = css("--s-" + k); ctx.fill();
      base = top;
    });
    ctx.beginPath(); ctx.strokeStyle = css("--fg"); ctx.lineWidth = 1.25;
    path(ctx, B, g, b => a.Y(b.slowest.total)); ctx.stroke();
    if (hi != null) { hoverLine(ctx, xOf(hi, g), a); tipFor(cv, sel, xOf(hi, g)); }
    ro.innerHTML = `<b>${sel.total.toFixed(1)} ms</b> average · ${sel.slowest.total.toFixed(1)} ms slowest`;
  } else if (c === "br") {
    const max = niceMax(Math.max(...B.map(b => b.mbps), 1), [5, 10, 20, 40, 60, 80, 100, 120, 150, 200, 300, 500]);
    const a = axes(ctx, w, h, max, [0, max / 2, max].map(v => Math.round(v)), "", g);
    const line = new Path2D(); B.forEach((b, i) => { const x = xOf(i, g), y = a.Y(b.mbps); i ? line.lineTo(x, y) : line.moveTo(x, y); });
    ctx.beginPath(); path(ctx, B, g, b => a.Y(b.mbps));
    ctx.lineTo(xOf(B.length - 1, g), a.y0); ctx.lineTo(xOf(0, g), a.y0); ctx.closePath();
    ctx.globalAlpha = .18; ctx.fillStyle = css("--data"); ctx.fill(); ctx.globalAlpha = 1;
    ctx.strokeStyle = css("--data"); ctx.lineWidth = 1.5; ctx.stroke(line);
    ctx.fillStyle = css("--accent");
    B.forEach((b, i) => { if (b.key) { const x = Math.round(xOf(i, g)); ctx.fillRect(x - 1, a.y1, 2, 6); ctx.globalAlpha = .35; ctx.fillRect(x, a.y1 + 6, 1, a.y0 - a.y1 - 6); ctx.globalAlpha = 1; } });
    if (hi != null) hoverLine(ctx, xOf(hi, g), a);
    else { ctx.fillStyle = css("--data"); ctx.beginPath(); ctx.arc(xOf(B.length - 1, g), a.Y(last.mbps), 3, 0, 7); ctx.fill(); }
    ro.innerHTML = `<b>${sel.mbps.toFixed(1)} Mb/s</b>${sel.key ? " · keyframe" : ""}`;
  } else {
    const target = (stats.stream && stats.stream.fps) || (snap && snap.session && snap.session.fps) || 60;
    const max = niceMax(target * 1.15, [30, 70, 140, 280]);
    const a = axes(ctx, w, h, max, [0, Math.round(target / 2), target], "", g);
    ctx.strokeStyle = css("--muted"); ctx.setLineDash([4, 4]); ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(g.x0, a.Y(target)); ctx.lineTo(g.x1, a.Y(target)); ctx.stroke(); ctx.setLineDash([]);
    ctx.beginPath(); ctx.strokeStyle = css("--data"); ctx.lineWidth = 1.5;
    path(ctx, B, g, b => a.Y(b.fps)); ctx.stroke();
    if (hi != null) hoverLine(ctx, xOf(hi, g), a);
    // The server measures each value over the last second.
    ro.innerHTML = `<b>${sel.fps.toFixed(0)} fps</b> of ${target}`;
  }
}

function tipFor(cv, b, x) {
  const ch = cv.closest(".chart"); let tip = ch.querySelector(".tip");
  if (!tip) { tip = document.createElement("div"); tip.className = "tip"; ch.appendChild(tip); }
  // Each point combines 250 ms of frames: the averages, and the slowest frame on its own.
  const f = b.slowest, ms = v => (v || 0).toFixed(2);
  tip.innerHTML = `<div class="tip-head"><span>ms</span><span>Average</span><span>Slowest</span></div>` +
    STAGES.slice().reverse().map(([k, l]) => `<div><span><i style="background:var(--s-${k})"></i>${l}</span><span>${ms(b[k])}</span><span>${ms(f[k])}</span></div>`).join("") +
    `<div class="tip-sum"><span>Total</span><span>${ms(b.total)}</span><span>${ms(f.total)}</span></div>` +
    `<div class="tip-note">Slowest frame: ${f.kb.toFixed(0)} kB${f.key ? ", keyframe" : ""}</div>`;
  const cw = ch.clientWidth, tw = tip.offsetWidth || 260;
  tip.style.left = Math.max(0, Math.min(cw - tw, x + (x > cw / 2 ? -tw - 12 : 12))) + "px";
  tip.style.top = (cv.offsetTop + 4) + "px";
}

function drawAll() { if (!$("#page-home").hidden) ["lat", "br", "fps"].forEach(draw); }
addEventListener("resize", drawAll);
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", drawAll);
new MutationObserver(drawAll).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
setInterval(() => { const d = $("#dur"); if (d && snap && snap.session) d.textContent = fmtDur(now() - snap.session.started); }, 1000);

/* ---------- Health ---------- */
function renderHealth() {
  const h = snap.health, checks = h.checks || [];
  const count = s => checks.filter(c => c.status === s).length;
  const ok = count("ok"), warn = count("warn"), fail = count("fail");
  $("#health-sum").innerHTML = checks.length
    ? `<span class="pill ok">${ok} passed</span>` + (warn ? `<span class="pill warn">${warn} warning${warn > 1 ? "s" : ""}</span>` : "") + (fail ? `<span class="pill bad">${fail} failed</span>` : "") +
      `<span class="pill plain">${h.running ? "Checking…" : "Checked " + rel(h.checked_at)}</span>`
    : `<span class="pill plain">${h.running ? "Checking…" : "Not checked yet"}</span>`;
  const order = { fail: 0, warn: 1, ok: 2 };
  $("#health").innerHTML = checks.length ? checks.slice().sort((a, b) => order[a.status] - order[b.status]).map(c => `
    <div class="check">
      <span class="mark ${c.status === "fail" ? "warn" : c.status}" style="${c.status === "fail" ? "color:var(--bad)" : ""}">${c.status === "ok" ? "✓" : "!"}</span>
      <div><strong>${esc(c.name)}</strong><p>${esc(c.message)}</p></div><span></span>
    </div>`).join("") : `<p class="empty-note">Moonshine started with --no-health-check. Select Check again to run the checks.</p>`;
  $("#recheck").disabled = h.running;
  $("#recheck").textContent = h.running ? "Checking…" : "Check again";
}
$("#recheck").onclick = () => act("run_health_check");

function renderServer() {
  const s = snap.server;
  $("#server-facts").innerHTML = [
    ["GPU", s.gpu || "Unknown"],
    ["Encoders", s.codecs.join(" · ") || "None found"],
    ["HDR", s.hdr ? "Supported" : "Off"],
    ["Version", s.version],
    ["Config", s.config_path]
  ].map(([k, v]) => `<div><span class="muted">${k}</span><span>${esc(v)}</span></div>`).join("");
}

/* ---------- Clients ---------- */
const editing = new Set(), confirming = new Set();
function renderPair() {
  const el = $("#pair-panel"), p = snap.pairing;
  const toggle = `<label class="switch"><input type="checkbox" id="allow-pair" ${p.enabled ? "checked" : ""}><span>Allow new devices</span></label>`;
  let body;
  if (!p.enabled) body = `<p class="empty-note">Pairing is off. Moonlight shows an error when a new device tries to pair. Paired devices still work.</p>`;
  else if (!p.requests.length) body = `<p class="empty-note">No device is asking to pair. In Moonlight, select <strong>${esc(snap.server.name)}</strong>. The PIN field shows here by itself.</p>`;
  else body = p.requests.map((r, n) => `
    <div class="req pair" data-req="${esc(r.id)}">
      <div style="display:grid;gap:12px">
        <div class="pair-req"><span class="pill info">Waiting</span><span>A device asked to pair ${rel(r.since)}.</span></div>
        <form class="pin" autocomplete="off">
          ${[0, 1, 2, 3].map(i => `<input inputmode="numeric" maxlength="1" aria-label="PIN digit ${i + 1}" id="pin-${n}-${i}">`).join("")}
        </form>
      </div>
      <div class="pair-actions"><button class="btn primary" disabled data-pinok>Pair device</button></div>
    </div>`).join("");
  el.innerHTML = `<div class="panel-head"><h2>Pair a new device</h2>${toggle}</div>${body}`;
  $("#allow-pair").onchange = e => act("set_pairing", { enabled: e.target.checked });
  $$(".req", el).forEach(req => {
    const ins = $$(".pin input", req), ok = $("[data-pinok]", req);
    const val = () => ins.map(i => i.value).join("");
    const submit = async () => { ok.disabled = true; const r = await act("submit_pin", { id: req.dataset.req, pin: val() }); if (!r.ok) ok.disabled = false; };
    ins.forEach((inp, i) => {
      inp.oninput = () => { inp.value = inp.value.replace(/\D/g, "").slice(-1); if (inp.value && ins[i + 1]) ins[i + 1].focus(); ok.disabled = val().length !== 4; };
      inp.onkeydown = e => { if (e.key === "Backspace" && !inp.value && ins[i - 1]) ins[i - 1].focus(); if (e.key === "Enter" && val().length === 4) { e.preventDefault(); submit(); } };
    });
    ins[0].addEventListener("paste", e => { const d = (e.clipboardData.getData("text") || "").replace(/\D/g, "").slice(0, 4); if (d) { e.preventDefault(); d.split("").forEach((c, i) => ins[i].value = c); ok.disabled = d.length !== 4; } });
    $(".pin", req).onsubmit = e => e.preventDefault();
    ok.onclick = submit;
  });
  const first = $(".pin input", el); if (first && document.activeElement === document.body) first.focus();
}

const COLORS = ["#5b4cb4", "#1f9aa5", "#a26d1d", "#7465cc", "#2e5f8a", "#8f2a2a", "#3f7d3a", "#b8a21a"];
const colorOf = s => { let h = 0; for (const c of String(s)) h = (h * 31 + c.charCodeAt(0)) >>> 0; return COLORS[h % COLORS.length]; };
function initials(t) { return String(t).split(/\s+/).filter(w => w && /[A-Za-z0-9]/.test(w[0])).slice(0, 2).map(w => w[0]).join("").toUpperCase() || "?"; }

function renderClients() {
  const list = snap.devices;
  $("#client-count").textContent = list.length + (list.length === 1 ? " device" : " devices");
  $("#client-list").innerHTML = list.length ? list.map(c => `
    <div class="row" data-fp="${esc(c.fingerprint)}">
      <div class="avatar" style="background:${colorOf(c.fingerprint)}">${esc(initials(c.name))}</div>
      <div class="row-main">
        ${editing.has(c.fingerprint) ? `<form class="inline-edit" data-rename><input value="${esc(c.name)}" placeholder="e.g. Living room TV" aria-label="Device name"><button class="btn sm primary">Save</button><button type="button" class="btn sm" data-cancel>Cancel</button></form>`
          : `<div class="row-title">${esc(c.name)}${c.streaming ? '<span class="pill ok">Streaming</span>' : ""}</div>`}
        <div class="row-sub">Paired ${rel(c.paired_at)} · Last seen ${rel(c.last_seen)} · <span class="mono">${esc(c.fingerprint.slice(0, 8))}</span></div>
      </div>
      <div class="row-actions">${editing.has(c.fingerprint) ? "" : `<button class="btn sm" data-edit>Rename</button><button class="btn sm danger" data-unpair>Unpair</button>`}</div>
      ${confirming.has(c.fingerprint) ? `<div class="confirm"><span>Unpair ${esc(c.name)}? It must pair again with a new PIN to stream.${c.streaming ? " Its stream stops." : ""}</span><button class="btn sm" data-keep>Keep</button><button class="btn sm danger solid" data-yes>Unpair</button></div>` : ""}
    </div>`).join("") : `<p class="empty-note">No devices are paired yet.</p>`;
  $$("#client-list .row").forEach(row => {
    const fp = row.dataset.fp, b = s => $(s, row);
    if (b("[data-edit]")) b("[data-edit]").onclick = () => { editing.add(fp); renderClients(); const i = $(`[data-fp="${fp}"] input`); i.focus(); i.select(); };
    if (b("[data-cancel]")) b("[data-cancel]").onclick = () => { editing.delete(fp); renderClients(); };
    if (b("[data-rename]")) b("[data-rename]").onsubmit = async e => { e.preventDefault(); const r = await act("rename_device", { fingerprint: fp, name: b("input").value }); if (r.ok) { editing.delete(fp); prev.devices = null; } };
    if (b("[data-unpair]")) b("[data-unpair]").onclick = () => { confirming.add(fp); renderClients(); };
    if (b("[data-keep]")) b("[data-keep]").onclick = () => { confirming.delete(fp); renderClients(); };
    if (b("[data-yes]")) b("[data-yes]").onclick = async () => { confirming.delete(fp); await act("unpair_device", { fingerprint: fp }); };
  });
}

/* ---------- Applications ---------- */
const openScanners = new Set();
let scannersSeen = false;
function appRow(title, sub, hidden, actions) {
  return `<div class="row${hidden ? " dim" : ""}">
      <div class="avatar" style="background:${colorOf(title)}">${esc(initials(title))}</div>
      <div class="row-main">
        <div class="row-title">${esc(title)}${hidden ? '<span class="pill plain">Hidden</span>' : ""}</div>
        ${sub ? `<div class="row-sub mono">${esc(sub)}</div>` : ""}
      </div>
      <div class="row-actions">${actions}</div>
    </div>`;
}
function renderApps() {
  if (!snap) return;
  if (!scannersSeen) { snap.scanners.forEach(s => openScanners.add(s.index)); scannersSeen = true; }
  const q = $("#app-search").value.trim().toLowerCase(), showHidden = $("#show-hidden").checked;
  const hiddenN = snap.scanners.reduce((n, s) => n + s.applications.filter(a => a.hidden).length, 0);
  $("#show-hidden-lbl").textContent = `Show hidden (${hiddenN})`;
  const match = t => !q || t.toLowerCase().includes(q);
  const kinds = Object.fromEntries(snap.scanner_kinds.map(k => [k.kind, k]));
  const own = snap.applications.filter(a => match(a.form.title)).sort((a, b) => a.form.title.localeCompare(b.form.title, undefined, { sensitivity: "base" }));
  let html = `<div class="panel">
      <div class="panel-head"><div><h2>Your applications</h2><p class="muted" style="margin:2px 0 0;font-size:13px">Saved in config.toml.</p></div><button class="btn primary sm" id="add-app">Add application</button></div>
      <div class="list">${own.length ? own.map(a => appRow(a.form.title, a.form.command, false, `<button class="btn sm" data-editapp="${a.index}">Edit</button>`)).join("") : `<p class="empty-note">${q ? "No applications match the search." : "No applications yet."}</p>`}</div>
    </div>
    <div class="section-head">
      <div><h2 class="sec">Scanners</h2><p class="muted" style="margin:2px 0 0">Scanners add apps by themselves and update the list when you install or remove something.${snap.scanned_at ? " Last scan " + rel(snap.scanned_at) + "." : ""}</p></div>
      <button class="btn sm" id="add-scanner">Add scanner</button>
    </div>`;
  html += snap.scanners.length ? snap.scanners.map(s => {
    const K = kinds[s.form.kind] || { name: s.form.kind };
    const list = s.applications.filter(a => (showHidden || !a.hidden) && match(a.title));
    const hid = s.applications.filter(a => a.hidden).length;
    return `<details class="panel scanner" data-scanner="${s.index}"${openScanners.has(s.index) ? " open" : ""}>
      <summary>
        <div class="scanner-id"><span class="src">${esc(K.name)}</span><span class="mono muted scanner-path">${esc(s.form.path.split("\n").join(" · "))}</span></div>
        <div class="scanner-meta"><span class="muted num">${s.applications.length} apps${hid ? ` · ${hid} hidden` : ""}</span>
          <button class="btn sm ghost" data-editscanner="${s.index}">Edit</button></div>
      </summary>
      <div class="list">${list.length ? list.map(a => appRow(a.title, "", a.hidden, `<button class="btn sm ghost" data-hide="${esc(a.title)}" data-hidden="${a.hidden}">${a.hidden ? "Show" : "Hide"}</button>`)).join("")
        : `<p class="empty-note">${q ? "No apps match the search." : "This scanner found no apps."}</p>`}</div>
    </details>`;
  }).join("") : `<p class="empty-note">No scanners. Add one to list your Steam games or desktop apps.</p>`;
  $("#app-list").innerHTML = html;
  $$("[data-hide]").forEach(b => b.onclick = () => act("set_hidden", { title: b.dataset.hide, hidden: b.dataset.hidden !== "true" }));
  $$("[data-editapp]").forEach(b => b.onclick = () => openAppSheet(snap.applications.find(a => a.index == b.dataset.editapp)));
  $$("[data-editscanner]").forEach(b => b.onclick = e => { e.preventDefault(); openScannerSheet(snap.scanners.find(s => s.index == b.dataset.editscanner)); });
  $$("details.scanner").forEach(d => d.ontoggle = () => { const id = +d.dataset.scanner; d.open ? openScanners.add(id) : openScanners.delete(id); });
  $("#add-app").onclick = () => openAppSheet(null);
  $("#add-scanner").onclick = () => openScannerSheet(null);
}
$("#app-search").oninput = renderApps;
$("#show-hidden").onchange = renderApps;

function sheet(html, onReady) {
  const root = $("#modal-root");
  root.innerHTML = `<div class="overlay" id="ov">${html}</div>`;
  const close = () => { root.innerHTML = ""; };
  $("#ov").onclick = e => { if (e.target.id === "ov") close(); };
  root.onkeydown = e => { if (e.key === "Escape") close(); };
  onReady(close);
}
const moreOptions = (f, extra = "") => `<details class="adv"${f.pre_command || f.post_command || f.env || f.steam_integration ? " open" : ""}><summary><strong>More options</strong></summary>
      <div style="display:grid;gap:12px">
        ${extra}
        <div class="field"><label for="f-pre">Before launch</label><textarea id="f-pre" placeholder="e.g. systemctl --user start obs-replay">${esc(f.pre_command)}</textarea><span class="help">One command for each line.</span></div>
        <div class="field"><label for="f-post">After exit</label><textarea id="f-post">${esc(f.post_command)}</textarea></div>
        <div class="field"><label for="f-env">Environment</label><textarea id="f-env" placeholder="MANGOHUD=1&#10;PROTON_ENABLE_HDR=1">${esc(f.env)}</textarea><span class="help">One NAME=value for each line.</span></div>
      </div>
    </details>`;
const common = () => ({ pre_command: $("#f-pre").value, post_command: $("#f-post").value, env: $("#f-env").value });

function openAppSheet(a) {
  const f = a ? a.form : { title: "", command: "", boxart: "", working_directory: "", steam_integration: false, pre_command: "", post_command: "", env: "" };
  sheet(`<form class="sheet" id="app-form" role="dialog" aria-modal="true" aria-labelledby="sheet-title">
    <h2 id="sheet-title">${a ? "Edit application" : "Add application"}</h2>
    <div class="field"><label for="f-title">Name</label><input type="text" id="f-title" value="${esc(f.title)}" placeholder="e.g. Heroic"></div>
    <div class="field"><label for="f-cmd">Command</label><input type="text" id="f-cmd" class="mono" value="${esc(f.command)}" placeholder="/usr/bin/heroic"><span class="help">Runs as you, inside the stream's own display. Use quotes around arguments with spaces.</span></div>
    <div class="field"><label for="f-art">Box art</label><input type="text" id="f-art" class="mono" value="${esc(f.boxart)}" placeholder="~/Pictures/heroic.png"><span class="help">Path to a PNG or JPEG on this computer. Without box art, clients show an icon or the name.</span></div>
    <div class="field"><label for="f-wd">Working folder</label><input type="text" id="f-wd" class="mono" value="${esc(f.working_directory)}" placeholder="Your home folder"><span class="help">The folder the application starts in, for a game that loads its files from there. Leave empty for your home folder.</span></div>
    ${moreOptions(f, `<label class="switch"><input type="checkbox" id="f-steam" ${f.steam_integration ? "checked" : ""}><span>Steam integration</span></label><span class="help">For Steam itself (Big Picture): Steam chooses which window is shown, so its overlay and focus work. Leave it off for anything else.</span>`)}
    <span class="err" id="f-err"></span>
    <div class="sheet-actions">
      ${a ? `<button type="button" class="btn danger" id="f-del" style="margin-right:auto">Remove</button>` : ""}
      <button type="button" class="btn" id="f-cancel">Cancel</button>
      <button class="btn primary">${a ? "Save" : "Add application"}</button>
    </div>
  </form>`, close => {
    $("#f-title").focus();
    $("#f-cancel").onclick = close;
    if (a) $("#f-del").onclick = async () => { if ((await act("remove_application", { index: a.index })).ok) close(); };
    $("#app-form").onsubmit = async e => {
      e.preventDefault();
      const form = { title: $("#f-title").value, command: $("#f-cmd").value, boxart: $("#f-art").value, working_directory: $("#f-wd").value, steam_integration: $("#f-steam").checked, ...common() };
      if (!form.title.trim()) { $("#f-err").textContent = "Type a name for the application."; return; }
      if (!form.command.trim()) { $("#f-err").textContent = "Type the command that starts the application."; return; }
      const r = await act("save_application", { index: a ? a.index : null, form });
      if (r.ok) close(); else $("#f-err").textContent = r.error || "";
    };
  });
}

function openScannerSheet(s) {
  let kind = s ? s.form.kind : (snap.scanner_kinds.find(k => k.found && !snap.scanners.some(x => x.form.kind === k.kind)) || snap.scanner_kinds[0]).kind;
  const draw = () => {
    const K = snap.scanner_kinds.find(k => k.kind === kind);
    const f = s ? s.form : { path: K.default_path, include_terminal: false, include_hidden: false, pre_command: "", post_command: "", env: "" };
    sheet(`<form class="sheet" id="sc-form" role="dialog" aria-modal="true" aria-labelledby="sheet-title">
      <h2 id="sheet-title">${s ? "Edit scanner" : "Add scanner"}</h2>
      ${s ? `<p class="muted" style="margin:0">${esc(K.name)}: ${esc(K.description)}</p>` : `<div class="field"><label>What to scan</label><div class="types">${snap.scanner_kinds.map(t => `
        <label class="type${t.kind === kind ? " on" : ""}"><input type="radio" name="sc-type" value="${t.kind}" ${t.kind === kind ? "checked" : ""}>
          <strong>${esc(t.name)}</strong><span>${esc(t.description)}</span>
          <em class="${t.found ? "found" : "nf"}">${t.found ? "Found on this computer" : "Not found"}</em></label>`).join("")}</div></div>`}
      <div class="field"><label for="f-path">${esc(K.path_label)}</label>
        ${K.multiline ? `<textarea id="f-path" class="mono">${esc(f.path)}</textarea><span class="help">One folder for each line. Subfolders are included.</span>` : `<input type="text" id="f-path" class="mono" value="${esc(f.path)}">`}
      </div>
      ${kind === "desktop" ? `<label class="switch"><input type="checkbox" id="f-term" ${f.include_terminal ? "checked" : ""}><span>Include terminal apps</span></label>` : ""}
      ${kind === "faugus" ? `<label class="switch"><input type="checkbox" id="f-hidden" ${f.include_hidden ? "checked" : ""}><span>Include hidden games</span></label><span class="help">Games on Faugus' Steam runner are not listed: add a Steam scanner for those.</span>` : ""}
      ${moreOptions(f)}
      <span class="err" id="f-err"></span>
      <div class="sheet-actions">
        ${s ? `<button type="button" class="btn danger" id="f-del" style="margin-right:auto">Remove scanner</button>` : ""}
        <button type="button" class="btn" id="f-cancel">Cancel</button>
        <button class="btn primary">${s ? "Save" : "Add scanner"}</button>
      </div>
    </form>`, close => {
      $("#f-cancel").onclick = close;
      $$('input[name="sc-type"]').forEach(r => r.onchange = () => { kind = r.value; draw(); });
      if (s) $("#f-del").onclick = async () => { if ((await act("remove_scanner", { index: s.index })).ok) close(); };
      $("#sc-form").onsubmit = async e => {
        e.preventDefault();
        const form = { kind, path: $("#f-path").value, include_terminal: !!($("#f-term") && $("#f-term").checked), include_hidden: !!($("#f-hidden") && $("#f-hidden").checked), ...common() };
        if (!form.path.trim()) { $("#f-err").textContent = `Type the ${K.path_label.toLowerCase()}.`; return; }
        $("#f-err").textContent = "";
        const btn = $("#sc-form .btn.primary"); btn.disabled = true; btn.textContent = "Scanning…";
        const r = await act("save_scanner", { index: s ? s.index : null, form });
        if (r.ok) { if (!s) openScanners.add(snap.scanners.length); close(); }
        else { $("#f-err").textContent = r.error || ""; btn.disabled = false; btn.textContent = s ? "Save" : "Add scanner"; }
      };
    });
  };
  draw();
}

/* ---------- Settings ---------- */
const EFFECT = { now: "Applies now", next: "Next stream", restart: "Needs restart" };
let saved = {}, draft = {};
const changed = () => Object.keys(draft).filter(k => !same(draft[k], saved[k]));
const isLoopback = c => /^127\.|^::1(\/|$)/.test(c);

function syncSettings() {
  const fresh = Object.fromEntries(snap.settings.map(s => [s.key, s.value]));
  const dirty = changed();
  saved = fresh;
  // Keep what the user is editing; take the saved values for everything else.
  const next = structuredClone(fresh);
  dirty.forEach(k => { if (k in draft) next[k] = draft[k]; });
  draft = next;
  keepOpen(renderSettings);
}

function control(s) {
  const v = draft[s.key], id = "s-" + s.key;
  if (s.kind === "bool") return `<label class="switch"><input type="checkbox" id="${id}" ${v ? "checked" : ""} ${s.read_only ? "disabled" : ""} aria-label="${esc(s.label)}"><span>${v ? "On" : "Off"}</span></label>`;
  if (s.kind === "range") return `<div style="display:grid;gap:2px;width:100%"><div class="range"><input type="range" id="${id}" min="${s.min ?? 0}" max="${s.max ?? 100}" step="5" value="${v}" aria-label="${esc(s.label)}" aria-valuetext="${v} percent"><output for="${id}" id="o-${s.key}">${v}${esc(s.unit || "")}</output></div>
    <div class="range-scale" aria-hidden="true"><span>${s.min ?? 0}${esc(s.unit || "")}</span><span>${Math.round(((s.min ?? 0) + (s.max ?? 100)) / 2)}${esc(s.unit || "")}</span><span>${s.max ?? 100}${esc(s.unit || "")}</span></div></div>`;
  if (s.kind === "choice") return `<select id="${id}" aria-label="${esc(s.label)}">${(s.options || []).map(o => `<option value="${esc(o.value)}" ${o.value === v ? "selected" : ""}>${esc(o.label)}</option>`).join("")}</select>`;
  if (s.kind === "list") return `<div class="chips">${(v || []).map((c, n) => `<span class="chip">${esc(c)}<button type="button" data-rm="${s.key}" data-n="${n}" aria-label="Remove ${esc(c)}">×</button></span>`).join("")}
    <form class="chip-add" data-add="${s.key}"><input placeholder="192.168.1.0/24" aria-label="Add address range"><button class="btn sm">Add</button></form></div>`;
  return `<div style="display:flex;gap:8px;align-items:center;width:100%"><input type="${s.kind === "number" ? "number" : "text"}" id="${id}" value="${esc(v)}" ${s.min != null ? `min="${s.min}"` : ""} ${s.max != null ? `max="${s.max}"` : ""} placeholder="${esc(s.placeholder || "")}" aria-label="${esc(s.label)}">${s.unit ? `<span class="muted">${esc(s.unit)}</span>` : ""}</div>`;
}

function renderSettings() {
  if (!snap) return;
  const groups = [];
  snap.settings.forEach(s => { let g = groups.find(x => x.name === s.group); if (!g) groups.push(g = { name: s.group, adv: s.advanced, items: [] }); g.items.push(s); });
  $("#settings").innerHTML = groups.map(g => {
    const body = g.items.map(s => `
      <div class="setting${same(draft[s.key], saved[s.key]) ? "" : " changed"}">
        <div class="lbl"><strong>${esc(s.label)} <span class="effect ${s.effect}">${EFFECT[s.effect]}</span></strong><span>${esc(s.help)}</span>
          ${s.key === "ui_allowed_networks" && (draft.ui_allowed_networks || []).some(c => !isLoopback(c)) ? `<div class="note warn" style="margin-top:6px">Anyone in these ranges can change settings and add applications. Applications run commands as you.</div>` : ""}
        </div>
        <div class="ctl">${control(s)}</div>
      </div>`).join("");
    const id = "adv-" + g.name.replace(/\W/g, "");
    return g.adv ? `<details class="panel adv" id="${id}"><summary><h2>${esc(g.name)}</h2></summary>${body}</details>` : `<div class="panel"><h2>${esc(g.name)}</h2>${body}</div>`;
  }).join("");
  snap.settings.forEach(s => {
    const el = $("#s-" + s.key); if (!el) return;
    el.oninput = el.onchange = () => {
      draft[s.key] = s.kind === "bool" ? el.checked : (s.kind === "number" || s.kind === "range") ? (el.value === "" ? "" : Number(el.value)) : el.value;
      if (s.kind === "bool") el.nextElementSibling.textContent = el.checked ? "On" : "Off";
      if (s.kind === "range") { $("#o-" + s.key).textContent = el.value + (s.unit || ""); el.setAttribute("aria-valuetext", el.value + " percent"); }
      el.closest(".setting").classList.toggle("changed", !same(draft[s.key], saved[s.key]));
      updateSavebar();
    };
  });
  $$("[data-rm]").forEach(b => b.onclick = () => { draft[b.dataset.rm] = draft[b.dataset.rm].filter((_, i) => i !== +b.dataset.n); keepOpen(renderSettings); });
  $$("[data-add]").forEach(f => f.onsubmit = e => {
    e.preventDefault(); const v = $("input", f).value.trim();
    if (!/^[0-9a-fA-F:.]+(\/\d{1,3})?$/.test(v)) { toast("Type a range such as 192.168.1.0/24."); return; }
    draft[f.dataset.add] = [...(draft[f.dataset.add] || []), v]; keepOpen(renderSettings);
  });
  updateSavebar();
}
function keepOpen(fn) { const open = $$("details.adv[open]").map(d => d.id); fn(); open.forEach(id => { const d = $("#" + id); if (d) d.open = true; }); }
function updateSavebar() {
  const c = changed();
  $("#savebar").hidden = !c.length;
  $("#savebar-text").textContent = `${c.length} unsaved change${c.length === 1 ? "" : "s"}`;
  setNavCounts();
}
$("#discard").onclick = () => { draft = structuredClone(saved); keepOpen(renderSettings); };
$("#save").onclick = async () => {
  const values = Object.fromEntries(changed().map(k => [k, draft[k]]));
  $("#save").disabled = true;
  const r = await act("save_settings", { values });
  $("#save").disabled = false;
  if (r.ok) { Object.assign(saved, structuredClone(values)); keepOpen(renderSettings); }
};

function renderRestart() {
  const r = snap.restart_needed;
  $("#restart-slot").innerHTML = r.length ? `<div class="note warn restart-banner"><span>Restart Moonshine to apply: ${r.map(esc).join(", ")}.${snap.session ? " The stream that runs now stops." : ""}</span><button class="btn sm primary" id="restart">Restart now</button></div>` : "";
  const b = $("#restart");
  if (b) b.onclick = async () => { b.disabled = true; b.textContent = "Restarting…"; if ((await act("restart")).ok) setLive(false, "Restarting…"); else { b.disabled = false; b.textContent = "Restart now"; } };
}

/* ---------- Logs ---------- */
const LEVELS = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
let minLevel = "INFO", logs = [], logsDirty = true;
function addLogs(lines) {
  logs.push(...lines); if (logs.length > 2000) logs.splice(0, logs.length - 2000);
  logsDirty = true;
  if (!$("#page-logs").hidden) renderLogs();
}
const fmtTs = ms => new Date(ms).toLocaleTimeString([], { hour12: false });
function renderLogs() {
  if (!logsDirty && !renderLogs.force) return;
  logsDirty = false; renderLogs.force = false;
  const q = $("#log-search").value.trim().toLowerCase(), max = LEVELS.indexOf(minLevel);
  const rows = logs.filter(x => LEVELS.indexOf(x.level) <= max && (!q || (x.target + " " + x.message).toLowerCase().includes(q))).slice(-1000);
  $("#log").innerHTML = rows.map(x => `<tr><td class="muted num">${fmtTs(x.time)}</td><td class="lv ${x.level}">${x.level}</td><td class="tgt">${esc(x.target)}</td><td class="msg">${esc(x.message)}</td></tr>`).join("") ||
    `<tr><td class="msg muted" style="padding:12px">No messages match this filter.</td></tr>`;
  if ($("#log-follow").checked) { const b = $("#log-box"); b.scrollTop = b.scrollHeight; }
}
$("#log-search").oninput = () => { renderLogs.force = true; renderLogs(); };
$$("#log-level button").forEach(b => b.onclick = () => { minLevel = b.dataset.l; $$("#log-level button").forEach(x => x.setAttribute("aria-pressed", x === b)); renderLogs.force = true; renderLogs(); });

/* ---------- Notifications ---------- */
// Conditions stay until they are resolved; events happened once and live while the page is open.
const events = [];
const seen = new Set();
function notify(sev, text) { events.unshift({ id: Math.random(), sev, text, ts: now(), read: false }); if (events.length > 50) events.pop(); renderBell(); }
function diffEvents(a, b) {
  if (!a.session && b.session) notify("ok", `${b.session.device || "A device"} started ${b.session.application}.`);
  if (a.session && !b.session) notify("plain", `The stream of ${a.session.application} ended.`);
  const known = new Set(a.devices.map(d => d.fingerprint));
  b.devices.filter(d => !known.has(d.fingerprint)).forEach(d => notify("ok", `${d.name} is paired. Give it a name on the Clients page.`));
  const before = new Map(a.scanners.map(s => [s.index + s.form.kind, s.applications.length]));
  b.scanners.forEach(s => { const n = before.get(s.index + s.form.kind); if (n != null && s.applications.length > n) { const k = b.scanner_kinds.find(k => k.kind === s.form.kind); notify("ok", `${k ? k.name : s.form.kind} scanner found ${s.applications.length - n} new app${s.applications.length - n === 1 ? "" : "s"}.`); } });
}
function conditions() {
  if (!snap) return [];
  const out = [];
  if (snap.pairing.enabled) snap.pairing.requests.forEach(r => out.push({ key: "pair:" + r.id + r.since, sev: "info", title: "A device asks to pair", text: "Type the PIN that Moonlight shows.", link: ["Type the PIN", "#clients"] }));
  (snap.health.checks || []).filter(c => c.status !== "ok").forEach(c => out.push({ key: "health:" + c.name + c.message, sev: "warn", title: c.name, text: c.message, link: ["View on Home", "#home"] }));
  if (snap.restart_needed.length) out.push({ key: "restart:" + snap.restart_needed.join(), sev: "warn", title: "Restart needed", text: "Saved settings take effect after a restart.", link: ["Open Settings", "#config"] });
  return out;
}
function renderBell() {
  const cs = conditions();
  const unread = cs.filter(c => !seen.has(c.key)).length + events.filter(e => !e.read).length;
  const bc = $("#bell-count"); bc.textContent = unread; bc.hidden = !unread;
  $("#bell").setAttribute("aria-label", unread ? `Notifications, ${unread} unread` : "Notifications");
  const panel = $("#notif"); if (panel.hidden) return;
  panel.innerHTML = `<div class="notif-head"><h2>Notifications</h2><button class="btn sm ghost" id="mark-read" ${unread ? "" : "disabled"}>Mark all as read</button></div>
    ${cs.map(c => `<div class="nitem${seen.has(c.key) ? "" : " unread"}"><span class="dot ${c.sev}"></span><div><strong>${esc(c.title)}</strong><p>${esc(c.text)}</p>
      <div class="meta"><a href="${c.link[1]}" data-go>${esc(c.link[0])}</a><span>Until resolved</span></div></div></div>`).join("")}
    ${events.map(e => `<div class="nitem${e.read ? "" : " unread"}"><span class="dot ${e.sev}"></span><div><p style="color:var(--fg)">${esc(e.text)}</p><div class="meta"><span>${rel(e.ts)}</span></div></div></div>`).join("")}
    ${!cs.length && !events.length ? `<p class="muted" style="padding:14px;margin:0">Nothing new.</p>` : ""}`;
  $("#mark-read").onclick = () => { cs.forEach(c => seen.add(c.key)); events.forEach(e => e.read = true); renderBell(); };
  $$("[data-go]", panel).forEach(a => a.onclick = () => toggleBell(false));
}
function toggleBell(open) { const p = $("#notif"); p.hidden = !open; $("#bell").setAttribute("aria-expanded", open); if (open) renderBell(); }
$("#bell").onclick = e => { e.stopPropagation(); toggleBell($("#notif").hidden); };
document.addEventListener("click", e => { if (!$("#notif").hidden && !e.target.closest("#notif")) toggleBell(false); });
document.addEventListener("keydown", e => { if (e.key === "Escape" && !$("#notif").hidden) { toggleBell(false); $("#bell").focus(); } });

function setNavCounts() {
  if (!snap) return;
  const w = (snap.health.checks || []).filter(c => c.status !== "ok").length, nh = $("#nav-health"); nh.textContent = w; nh.hidden = !w;
  const p = snap.pairing.enabled ? snap.pairing.requests.length : 0, np = $("#nav-pair"); np.textContent = p; np.hidden = !p;
  const u = changed().length, nu = $("#nav-unsaved"); nu.textContent = u; nu.hidden = !u;
  renderBell();
}

/* ---------- Banner ---------- */
// While a client streams, moonlight flows down the beam into the staff's crystal and the crystal pulses.
// One canvas pixel is one banner pixel. The geometry mirrors assets/banner-src/banner.py: the UI banner is
// render(1280, 120, 250) with the top 8 rows cut off, shown at 2x.
const bannerFx = (() => {
  const cv = $("#mast-fx"), img = $(".mast img"), g = cv.getContext("2d");
  const X0 = 240, Y0 = 8, W = cv.width, H = cv.height;        // canvas origin in render() pixels
  const MX = 370 - X0, MY = 40 - Y0, MR = 25;                  // moon
  const CX = 263 - X0, CY = 15 - Y0;                           // crystal on the staff
  const BL = Math.hypot(CX - MX, CY - MY), UX = (CX - MX) / BL, UY = (CY - MY) / BL;
  const bw = t => MR * .9 * Math.max(0, 1 - t) ** .75 + 2.5;  // beam half-width, as in banner.py
  const B4 = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
  const bayer = (x, y) => (B4[((y + Y0) & 3) * 4 + ((x + X0) & 3)] + .5) / 16;
  const LIGHT = [236, 240, 255], MID = [170, 180, 245];
  const rgb = c => `rgb(${c})`;
  const T0 = (MR + 1) / BL;                                    // the beam leaves the moon here

  // Beam pixels: position along the beam (0 = moon, 1 = crystal) and closeness to the beam's center line.
  const beam = [], halo = [];
  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    const t = ((x - MX) * UX + (y - MY) * UY) / BL, sd = (x - MX) * -UY + (y - MY) * UX, w = bw(t);
    if (t >= 0 && t <= 1.02 && Math.abs(sd) < w && Math.hypot(x + .5 - MX, y + .5 - MY) > MR + .5)
      beam.push({ i: (y * W + x) * 4, t, e: 1 - Math.abs(sd) / w, b: bayer(x, y) });
    const d = Math.hypot(x - CX, y - CY);
    if (d > 6.5 && d < 13) halo.push({ x, y, f: 1 - (d - 6.5) / 6.5, b: bayer(x, y) });
  }
  const data = g.createImageData(W, H);

  // Motes of light that drift down the beam and funnel into the crystal.
  const gauss = () => (Math.random() + Math.random() + Math.random() - 1.5) / 1.5;
  const mote = t => ({ t, s: gauss() * .8, v: .05 + Math.random() * .04, ph: Math.random() * 6.3, kind: Math.random() });
  const motes = Array.from({ length: 36 }, () => mote(T0 + Math.random() * (1 - T0)));

  let on = false, raf = 0, last = 0, time = 0, offAt = 0, energy = 0;
  const still = matchMedia("(prefers-reduced-motion: reduce)");

  function step(dt) {
    energy *= Math.exp(-dt * 1.5);
    for (const m of motes) {
      m.t += m.v * (.6 + .8 * m.t) * dt;
      if (m.t >= .97) { energy = Math.min(.4, energy + .08); Object.assign(m, mote(T0 + Math.random() * .05)); }
    }
  }

  function draw() {
    // Bands of brighter light that travel along the beam, dithered like the banner.
    const d = data.data; d.fill(0);
    for (const p of beam) {
      const k = (.5 + .5 * Math.cos(2 * Math.PI * (p.t - time * .05) * 3)) ** 2.5 * p.e ** .7 * (.5 + .5 * p.t);
      if (k * .9 <= p.b) continue;
      const c = k > .45 ? LIGHT : MID;
      d[p.i] = c[0]; d[p.i + 1] = c[1]; d[p.i + 2] = c[2]; d[p.i + 3] = c === LIGHT ? 176 : 136;
    }
    g.putImageData(data, 0, 0);

    // The crystal: a slow pulse, lifted by each mote that arrives.
    const pulse = Math.min(1, .25 + .5 * (.5 - .5 * Math.cos(2 * Math.PI * time / 4.5)) + energy);
    g.fillStyle = rgb(MID); g.globalAlpha = .6;
    for (const h of halo) if (pulse * h.f * .6 > h.b) g.fillRect(h.x, h.y, 1, 1);
    g.fillStyle = rgb(LIGHT);
    const ext = Math.round(pulse * 3);
    for (let a = 0; a < 8; a++) {
      const L = a % 2 ? 8 : 15, ca = Math.cos(a * Math.PI / 4), sa = Math.sin(a * Math.PI / 4);
      for (let r = L; r < L + ext; r++) {
        g.globalAlpha = .75 * (1 - (r - L) / 4);
        g.fillRect(CX + Math.round(ca * r), CY + Math.round(sa * r), 1, 1);
      }
    }
    g.globalAlpha = Math.max(0, pulse - .5);                   // the cyan half of the crystal charges to white
    for (let dy = -6; dy <= 6; dy++) g.fillRect(CX + 1, CY + dy, 3 - (Math.abs(dy) >> 1), 1);

    for (const m of motes) {
      const lat = m.s * bw(m.t) + Math.sin(time * 1.3 + m.ph) * .6;
      const x = Math.round(MX + UX * BL * m.t - UY * lat), y = Math.round(MY + UY * BL * m.t + UX * lat);
      const a = Math.ceil(Math.min(1, (m.t - T0) / .08) * (.55 + .45 * m.t) * 3) / 3;
      if (a <= 0) continue;
      g.fillStyle = rgb(MID);
      if (m.kind < .4) {                                       // a streak with a fading tail
        for (let k = 1; k <= 3; k++) { g.globalAlpha = a * (1 - k / 4); g.fillRect(Math.round(x - UX * k), Math.round(y - UY * k), 1, 1); }
      } else if (m.kind < .65 && Math.sin(time * 4 + m.ph) > 0) { // a twinkling four-point star
        g.globalAlpha = a; g.fillRect(x - 1, y, 3, 1); g.fillRect(x, y - 1, 1, 3);
      }
      g.fillStyle = rgb(LIGHT); g.globalAlpha = a;
      g.fillRect(x, y, 1, 1);
    }
    g.globalAlpha = 1;
  }

  function frame(ts) {
    raf = 0;
    if (still.matches || (!on && ts - offAt > 1600)) { g.clearRect(0, 0, W, H); return; }
    raf = requestAnimationFrame(frame);
    if (ts - last < 1000 / 30 - 2) return;                     // 30 fps is enough for slow light
    const dt = Math.min(.1, (ts - last) / 1000); last = ts;
    time += dt; step(dt); draw();
  }

  // The image is object-fit: cover, anchored left; follow its scale so the canvas stays on the beam.
  function place() {
    const s = Math.max(img.clientWidth / 2560, img.clientHeight / 214), px = 2 * s;
    cv.style.left = X0 * px + "px";
    cv.style.top = (img.clientHeight - 214 * s) / 2 + "px";
    cv.style.width = W * px + "px"; cv.style.height = H * px + "px";
  }
  new ResizeObserver(place).observe(img);

  return function set(v) {
    if (v === on) return;
    on = v;
    cv.classList.toggle("on", on);
    if (!on) offAt = performance.now();
    else if (!raf && !still.matches) { last = performance.now(); raf = requestAnimationFrame(frame); }
  };
})();

/* ---------- Guide ---------- */
// A tour for new users: the page goes dark except one part, and a card tells what that part does.
// It opens by itself until the user turns it off. The guide button in the masthead opens it again.
const guide = (() => {
  const KEY = "moonshine-guide";
  const STEPS = [
    { page: "home", sel: "nav.rail", title: "Pages", text: "The control panel has five pages. Select a page here to open it." },
    { page: "home", sel: "#session", title: "Stream", text: "This panel shows which device streams which application. While a device streams, live graphs show the latency, bitrate and frame rate. You can also stop the stream here." },
    { page: "home", sel: "#page-home .panel:has(#health)", title: "Health", text: "When Moonshine starts, it checks the GPU, the encoders and the ports. A warning tells you what to repair. After a repair, select Check again." },
    { page: "home", sel: "#bell", title: "Notifications", text: "The bell shows pairing requests, health warnings and stream events. The number shows how many you did not read." },
    { page: "clients", sel: "#pair-panel", title: "Pair a device", text: "To add a device, select this computer in Moonlight. Moonlight shows a PIN. Type that PIN here. Turn off Allow new devices to refuse all new devices." },
    { page: "clients", sel: "#page-clients .panel:has(#client-list)", title: "Paired devices", text: "These devices can stream from this computer. Rename a device to find it easily. Unpair a device to remove its access." },
    { page: "apps", sel: "#app-list > .panel:first-child", title: "Your applications", text: "Add an application with a name and a command. Clients show it in their app list." },
    { page: "apps", sel: "#app-list > .section-head", title: "Scanners", text: "A scanner finds your Steam games or desktop apps. It updates the list when you install or remove something. Hide an app that you do not want to see in Moonlight." },
    { page: "config", sel: "#settings > :first-child", title: "Settings", text: "Each setting shows when a change takes effect: now, at the next stream, or after a restart. Select Save to write your changes to config.toml." },
    { page: "logs", sel: "#report", title: "Logs", text: "This page shows the latest messages from Moonshine. If you find a bug, download the report and attach it to the issue." },
    { page: null, sel: "#guide-btn", title: "Open the guide again", text: "Select guide to see this tour again." }
  ];
  const get = () => { try { return localStorage.getItem(KEY); } catch (e) { return null; } };
  const put = v => { try { localStorage.setItem(KEY, v); } catch (e) { /* storage blocked */ } };
  let ui = null, i = 0, raf = 0, last = "", from = "", inerted = [], focusBack = null;

  // The part to light up. If it is not on the page (for example, no settings yet), light up the page heading.
  function target(s) {
    const t = $(s.sel);
    if (t && t.getClientRects().length) return t;
    return s.page ? $(`#page-${s.page} .page-head`) : null;
  }

  function show(n) {
    i = n; last = "";
    const s = STEPS[i];
    if (s.page && location.hash.slice(1) !== s.page) { location.hash = s.page; route(); }
    const t = target(s);
    if (t) t.scrollIntoView({ block: t.offsetHeight < innerHeight * .6 ? "center" : "start" });
    ui.card.innerHTML = `
      <div class="guide-step">${i + 1} / ${STEPS.length}</div>
      <h2 id="guide-title">${esc(s.title)}</h2>
      <p id="guide-text">${esc(s.text)}</p>
      <label class="switch guide-off"><input type="checkbox" id="guide-off" ${ui.off ? "checked" : ""}><span>Do not show the guide at start</span></label>
      <div class="guide-actions">
        <button class="btn sm ghost" data-g="close">Close</button><span class="grow"></span>
        ${i ? '<button class="btn sm" data-g="back">Back</button>' : ""}
        <button class="btn sm primary" data-g="next">${i === STEPS.length - 1 ? "Done" : "Next"}</button>
      </div>`;
    $("#guide-off").onchange = e => { ui.off = e.target.checked; };
    $$("[data-g]", ui.card).forEach(b => b.onclick = () => ({ close, back: () => show(i - 1), next })[b.dataset.g]());
    $('[data-g="next"]', ui.card).focus();
  }
  const next = () => i < STEPS.length - 1 ? show(i + 1) : close();

  // Follow the part every frame: the page renders again when new data arrives, and it can scroll or resize.
  function track() {
    raf = requestAnimationFrame(track);
    const t = target(STEPS[i]); if (!t) return;
    const r = t.getBoundingClientRect(), vw = document.documentElement.clientWidth, vh = innerHeight, pad = 6, g = 12;
    const top = Math.max(4, r.top - pad), bot = Math.min(vh - 4, r.bottom + pad), left = Math.max(4, r.left - pad), right = Math.min(vw - 4, r.right + pad);
    const cw = ui.card.offsetWidth, ch = ui.card.offsetHeight;
    const key = [top, bot, left, right, vw, vh, cw, ch].join();
    if (key === last) return; last = key;
    Object.assign(ui.hole.style, { top: top + "px", left: left + "px", width: Math.max(0, right - left) + "px", height: Math.max(0, bot - top) + "px" });
    // Below the part, then above, then at its right or left, and else over its bottom.
    const clampX = x => Math.max(g, Math.min(vw - cw - g, x)), clampY = y => Math.max(g, Math.min(vh - ch - g, y));
    let x, y;
    if (bot + g + ch <= vh - g) { x = clampX(left); y = bot + g; }
    else if (top - g - ch >= g) { x = clampX(left); y = top - g - ch; }
    else if (right + g + cw <= vw - g) { x = right + g; y = clampY(top); }
    else if (left - g - cw >= g) { x = left - g - cw; y = clampY(top); }
    else { x = clampX(left); y = vh - ch - g; }
    ui.card.style.left = x + "px"; ui.card.style.top = y + "px";
  }

  function onKey(e) {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(); }
    else if (e.key === "ArrowRight") { e.preventDefault(); next(); }
    else if (e.key === "ArrowLeft" && i) { e.preventDefault(); show(i - 1); }
  }

  function open() {
    if (ui) return;
    toggleBell(false);
    $("#modal-root").innerHTML = "";
    from = location.hash || "#home";
    focusBack = document.activeElement;
    const root = document.createElement("div");
    root.innerHTML = `<div class="guide-block"></div><div class="guide-hole"></div>
      <div class="guide-card" role="dialog" aria-modal="true" aria-labelledby="guide-title" aria-describedby="guide-text"></div>`;
    // Everything behind the guide is out of reach for the mouse and the keyboard.
    inerted = [...document.body.children].filter(el => !el.inert);
    inerted.forEach(el => { el.inert = true; });
    document.body.appendChild(root);
    ui = { root, hole: $(".guide-hole", root), card: $(".guide-card", root), off: get() !== "on" };
    document.addEventListener("keydown", onKey, true);
    show(0);
    raf = requestAnimationFrame(track);
  }

  function close() {
    if (!ui) return;
    put(ui.off ? "off" : "on");
    cancelAnimationFrame(raf);
    document.removeEventListener("keydown", onKey, true);
    ui.root.remove(); ui = null;
    inerted.forEach(el => { el.inert = false; }); inerted = [];
    if (location.hash !== from) { location.hash = from; route(); }
    if (focusBack && focusBack.focus) focusBack.focus();
  }

  $("#guide-btn").onclick = e => { e.stopPropagation(); open(); };
  // At start, wait for the first data so the panels have their size.
  return { start() { if (get() !== "off") requestAnimationFrame(open); } };
})();

/* ---------- Theme ---------- */
const THEMES = ["auto", "light", "dark"];
let theme = "auto";
try { theme = localStorage.getItem("moonshine-theme") || "auto"; } catch (e) { /* storage blocked */ }
function applyTheme() {
  if (theme === "auto") document.documentElement.removeAttribute("data-theme"); else document.documentElement.setAttribute("data-theme", theme);
  $("#theme-btn").textContent = theme; $("#theme-btn").title = "Color theme: " + theme + ". Click to change.";
}
$("#theme-btn").onclick = () => { theme = THEMES[(THEMES.indexOf(theme) + 1) % 3]; try { localStorage.setItem("moonshine-theme", theme); } catch (e) { /* storage blocked */ } applyTheme(); };
applyTheme();

route();
connect();
})();
