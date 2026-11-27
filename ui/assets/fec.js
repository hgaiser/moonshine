// Reed-Solomon erasure decoding for the browser stream's video, the same code as `fec-rs` on the
// server (Moonlight's): GF(2^8) with the polynomial 0x11D, data rows of the identity matrix, and
// parity row j, column i = 1 / ((parity + i) XOR j), a Cauchy matrix.
"use strict";

const EXP = new Uint8Array(510);
const LOG = new Uint8Array(256);
{
  let x = 1;
  for (let i = 0; i < 255; i++) {
    EXP[i] = x;
    LOG[x] = i;
    x <<= 1;
    if (x & 0x100) x ^= 0x11d;
  }
  for (let i = 255; i < 510; i++) EXP[i] = EXP[i - 255];
}

/// MUL[a * 256 + b] = a * b in GF(2^8).
const MUL = new Uint8Array(256 * 256);
for (let a = 1; a < 256; a++) {
  for (let b = 1; b < 256; b++) MUL[a * 256 + b] = EXP[LOG[a] + LOG[b]];
}

const inverse = a => EXP[255 - LOG[a]];

/// The row of the encoding matrix for shard `index` of a block.
function row(index, data, parity) {
  const r = new Uint8Array(data);
  if (index < data) r[index] = 1;
  else for (let i = 0; i < data; i++) r[i] = inverse((parity + i) ^ (index - data));
  return r;
}

/// Invert a square matrix (rows of Uint8Array) in place by Gauss-Jordan elimination.
function invert(rows) {
  const n = rows.length;
  const out = rows.map((_, i) => { const r = new Uint8Array(n); r[i] = 1; return r; });
  for (let c = 0; c < n; c++) {
    let p = c;
    while (p < n && rows[p][c] === 0) p++;
    if (p === n) throw new Error("the shards do not make a solvable system");
    [rows[c], rows[p]] = [rows[p], rows[c]];
    [out[c], out[p]] = [out[p], out[c]];
    const scale = inverse(rows[c][c]) * 256;
    for (let k = 0; k < n; k++) { rows[c][k] = MUL[scale + rows[c][k]]; out[c][k] = MUL[scale + out[c][k]]; }
    for (let r = 0; r < n; r++) {
      const f = rows[r][c];
      if (r === c || f === 0) continue;
      const m = f * 256;
      for (let k = 0; k < n; k++) { rows[r][k] ^= MUL[m + rows[c][k]]; out[r][k] ^= MUL[m + out[c][k]]; }
    }
  }
  return out;
}

/// Rebuild the missing data shards of a block. `shards` has `data + parity` entries: a
/// Uint8Array of `size` bytes for each received shard, undefined for the others. At least `data`
/// of them must be there. Returns the `data` data shards.
export function reconstruct(shards, data, parity, size) {
  const missing = [];
  for (let i = 0; i < data; i++) if (!shards[i]) missing.push(i);
  if (!missing.length) return shards.slice(0, data);

  // Any `data` received shards, data shards first.
  const used = [];
  for (let i = 0; i < data + parity && used.length < data; i++) if (shards[i]) used.push(i);
  if (used.length < data) throw new Error("too few shards");

  const decode = invert(used.map(i => row(i, data, parity)));
  const out = shards.slice(0, data);
  for (const m of missing) {
    const result = new Uint8Array(size);
    const coefficients = decode[m];
    for (let k = 0; k < data; k++) {
      const c = coefficients[k];
      if (c === 0) continue;
      const src = shards[used[k]], base = c * 256;
      if (c === 1) for (let x = 0; x < size; x++) result[x] ^= src[x];
      else for (let x = 0; x < size; x++) result[x] ^= MUL[base + src[x]];
    }
    out[m] = result;
  }
  return out;
}

/// The header of a video datagram (see `ui/src/stream.rs`).
export const VIDEO_HEADER = 26;

export function parseVideoDatagram(d) {
  const view = new DataView(d.buffer, d.byteOffset, d.byteLength);
  return {
    flags: d[1],
    number: view.getUint32(2, true),
    block: d[6],
    blocks: d[7],
    index: d[8],
    data: d[9],
    parity: d[10],
    first: view.getUint16(12, true),
    length: view.getUint32(14, true),
    timestamp: Number(view.getBigUint64(18, true)),
    shard: d.subarray(VIDEO_HEADER),
  };
}

/// The shards of one video frame, until it can be put together.
export class FrameShards {
  constructor(h) {
    this.length = h.length;
    this.blocks = new Array(h.blocks);
    this.done = 0;
    this.received = 0;
  }

  /// Add a parsed datagram. Returns true when the frame is complete.
  add(h) {
    let block = this.blocks[h.block];
    if (!block) block = this.blocks[h.block] = { data: h.data, parity: h.parity, first: h.first, shards: new Array(h.data + h.parity), got: 0, done: false };
    if (block.done || block.shards[h.index] || h.index >= block.shards.length) return this.complete;
    block.shards[h.index] = h.shard;
    block.got++;
    this.received++;
    if (block.got >= block.data) { block.done = true; this.done++; }
    return this.complete;
  }

  get complete() { return this.done === this.blocks.length; }

  /// How many lost shards the parity rebuilt.
  get recovered() {
    let n = 0;
    for (const b of this.blocks) if (b) for (let i = 0; i < b.data; i++) if (!b.shards[i]) n++;
    return n;
  }

  /// The frame's bytes. Call only when complete.
  assemble() {
    const out = new Uint8Array(this.length);
    for (const b of this.blocks) {
      const size = b.shards.find(s => s).length;
      const data = reconstruct(b.shards, b.data, b.parity, size);
      for (let i = 0; i < b.data; i++) {
        const offset = (b.first + i) * size;
        if (offset >= this.length) break;
        out.set(data[i].subarray(0, Math.min(size, this.length - offset)), offset);
      }
    }
    return out;
  }
}
