// Checks the browser's FEC decoder (ui/assets/fec.js) against fec-rs on the server.
// FEC_VECTORS=/tmp/fec.json cargo test -p moonshine-ui fec_vectors && node ui/tests/fec.mjs /tmp/fec.json
import { readFileSync } from "node:fs";
import { FrameShards, parseVideoDatagram } from "../assets/fec.js";

const fromHex = h => Uint8Array.from(h.match(/../g) ?? [], b => parseInt(b, 16));
const equal = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
let seed = 12345;
const random = n => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };

const vectors = JSON.parse(readFileSync(process.argv[2], "utf8"));
let checks = 0, failures = 0;
for (const v of vectors) {
  const data = fromHex(v.data);
  const datagrams = v.datagrams.map(d => parseVideoDatagram(fromHex(d)));
  const blocks = new Map();
  for (const d of datagrams) blocks.set(d.block, [...(blocks.get(d.block) ?? []), d]);
  for (let trial = 0; trial < 50; trial++) {
    // Drop up to `parity` shards of each block (trial 0: none; trial 1: exactly `parity`).
    const dropped = new Set();
    for (const list of blocks.values()) {
      const parity = list[0].parity;
      const n = trial === 0 ? 0 : trial === 1 ? parity : random(parity + 1);
      while ([...dropped].filter(d => list.includes(d)).length < n) dropped.add(list[random(list.length)]);
    }
    const frame = new FrameShards(datagrams[0]);
    for (const d of [...datagrams].sort(() => random(3) - 1)) if (!dropped.has(d)) frame.add(d);
    checks++;
    if (!frame.complete || !equal(frame.assemble(), data)) {
      failures++;
      console.log(`FAIL: ${data.length} bytes, ${v.percent}%, dropped ${dropped.size}, complete ${frame.complete}`);
    }
  }
  // One shard more than the parity in a block cannot be rebuilt.
  const first = blocks.get(0);
  const frame = new FrameShards(datagrams[0]);
  for (const d of datagrams.filter(d => d.block !== 0 || d.index > first[0].parity)) frame.add(d);
  checks++;
  if (frame.complete) { failures++; console.log(`FAIL: ${data.length} bytes completed without enough shards`); }
  console.log(`${data.length} bytes, ${v.percent}%: ${datagrams.length} datagrams in ${blocks.size} block(s), parity ${[...blocks.values()].map(l => l[0].parity).join("/")}`);
}
console.log(`${checks - failures}/${checks} checks passed`);
process.exit(failures ? 1 : 0);
