#!/usr/bin/env node
/**
 * check_profile_hpel_vectors.js — decode the Rust tag-6 (half-pixel motion)
 * profile encoder's output with the SHIPPED browser decoder (web/codec.js
 * makeProfileDecoder) and require bit-exact equality with the Rust encoder's
 * own reconstructed ("shown") BGR frames. This is the browser-side proof that
 * the half-pel interpolation math (`(A+B+1)>>1` / `(A+B+C+D+2)>>2`, integer,
 * edge-clamped) crosses the wire identically on both sides.
 *
 * Usage (after cargo test --test roundtrip_profile -- --ignored):
 *   node experiments/check_profile_hpel_vectors.js
 */
'use strict';

const fs = require('fs');
const path = require('path');

const codec = require(path.join(__dirname, '..', 'web', 'codec.js'));
const FILE = path.join(__dirname, 'vectors_profile_hpel_rust.bin');

const HEADER = 13; // PRFV(4) + version(1) + W,H(4) + n(4)

(async () => {
  if (!fs.existsSync(FILE)) {
    console.error('missing ' + FILE + ' — run: cargo test --test roundtrip_profile -- --ignored');
    process.exit(1);
  }
  const buf = fs.readFileSync(FILE);
  let off = 0;
  let cases = 0;
  let frames = 0;

  while (off + HEADER <= buf.length) {
    const magic = buf.slice(off, off + 4).toString('ascii');
    if (magic !== 'PRFV') throw new Error('bad magic at ' + off);
    if (buf[off + 4] !== 1) throw new Error('bad version at ' + off);
    const w = buf.readUInt16BE(off + 5);
    const h = buf.readUInt16BE(off + 7);
    const n = buf.readUInt32BE(off + 9);
    off += HEADER;

    const dec = codec.makeProfileDecoder();
    for (let i = 0; i < n; i++) {
      const index = buf.readUInt32BE(off); off += 4;
      const mlen = buf.readUInt32BE(off); off += 4;
      const msg = buf.slice(off, off + mlen); off += mlen;
      const slen = buf.readUInt32BE(off); off += 4;
      const shown = buf.slice(off, off + slen); off += slen;

      if (msg.length < 5 || msg[4] !== codec.TAG_PROFILE_HPEL) {
        throw new Error(`frame ${index} (${w}x${h}): not a tag-6 message (tag=${msg.length >= 5 ? msg[4] : 'short'})`);
      }
      const { frameIndex, frame } = await dec.decode(msg);
      if (frameIndex !== index) throw new Error(`index mismatch: got ${frameIndex}, want ${index}`);
      if (frame.length !== shown.length) {
        throw new Error(`frame ${index} (${w}x${h}): length ${frame.length} != ${shown.length}`);
      }
      for (let k = 0; k < frame.length; k++) {
        if (frame[k] !== shown[k]) {
          throw new Error(`frame ${index} (${w}x${h}): byte ${k} ${frame[k]} != ${shown[k]}`);
        }
      }
      frames++;
    }
    cases++;
  }

  console.log(`OK: codec.js decoded ${frames} Rust half-pel-encoded frames across ${cases} cases, all bit-exact`);
})().catch((e) => {
  console.error('FAIL:', e.message);
  process.exit(1);
});
