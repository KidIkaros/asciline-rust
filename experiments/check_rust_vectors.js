#!/usr/bin/env node
/**
 * check_rust_vectors.js — decode the Rust encoder's output with the SHIPPED
 * browser decoder (web/codec.js, unchanged from the original project) and
 * require bit-exact equality with the expected framebuffers.
 *
 * Usage (after cargo test --test decode_python_vectors -- --ignored):
 *   node experiments/check_rust_vectors.js
 */
'use strict';

const fs = require('fs');
const path = require('path');

const codec = require(path.join(__dirname, '..', 'web', 'codec.js'));
const FILE = path.join(__dirname, 'vectors_rust.bin');

const HEADER = 18; // RSTV(4) + version(1) + cell(1) + cols,rows,n(12)

(async () => {
  if (!fs.existsSync(FILE)) {
    console.error('missing ' + FILE + ' — run: cargo test --test decode_python_vectors -- --ignored');
    process.exit(1);
  }
  const buf = fs.readFileSync(FILE);
  let off = 0;
  let cases = 0;
  let frames = 0;

  while (off + HEADER <= buf.length) {
    const magic = buf.slice(off, off + 4).toString('ascii');
    if (magic !== 'RSTV') throw new Error('bad magic at ' + off);
    const cell = buf[off + 5];
    const cols = buf.readUInt32BE(off + 6);
    const rows = buf.readUInt32BE(off + 10);
    const n = buf.readUInt32BE(off + 14);
    off += HEADER;

    const dec = codec.makeDecoder(cell);
    for (let i = 0; i < n; i++) {
      const index = buf.readUInt32BE(off); off += 4;
      const mlen = buf.readUInt32BE(off); off += 4;
      const msg = buf.slice(off, off + mlen); off += mlen;
      const plen = buf.readUInt32BE(off); off += 4;
      const plain = buf.slice(off, off + plen); off += plen;

      const { frameIndex, frame } = await dec.decode(msg);
      if (frameIndex !== index) throw new Error(`index mismatch: got ${frameIndex}, want ${index}`);
      if (frame.length !== plain.length) {
        throw new Error(`frame ${index} (${cols}x${rows}, cell=${cell}): length ${frame.length} != ${plain.length}`);
      }
      for (let k = 0; k < frame.length; k++) {
        if (frame[k] !== plain[k]) {
          throw new Error(`frame ${index} (${cols}x${rows}, cell=${cell}): byte ${k} ${frame[k]} != ${plain[k]}`);
        }
      }
      frames++;
    }
    cases++;
  }

  console.log(`OK: codec.js decoded ${frames} Rust-encoded frames across ${cases} cases, all bit-exact`);
})().catch((e) => {
  console.error('FAIL:', e.message);
  process.exit(1);
});
