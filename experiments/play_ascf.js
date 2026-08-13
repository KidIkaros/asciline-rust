#!/usr/bin/env node
/**
 * play_ascf.js — decode compiled .ascf containers with the SHIPPED browser
 * decoder (web/codec.js makeDecoder), exactly as web/app.js feeds WebSocket
 * frames: one stateful decoder, every record in order, keyframe resync and
 * all. This is the end-to-end proof that the exact .ascf bytes a user
 * downloads play in the browser decoder — container header, record framing,
 * tag dispatch (raw/zlib/delta/RLE-full/profile tag 4 / profile tag 5 AQ),
 * and per-frame size + monotonic-index validation.
 *
 * Usage:
 *   node experiments/play_ascf.js samples/big_buck_bunny_profile.ascf samples/drone_profile.ascf ...
 *
 * Prints a per-clip summary; exits non-zero on any decode failure, size
 * mismatch, or frame-index regression.
 */
'use strict';

const fs = require('fs');
const path = require('path');

const codec = require(path.join(__dirname, '..', 'web', 'codec.js'));

function parseHeader(buf) {
  if (buf.length < 14) throw new Error('ascf header too short');
  const magic = buf.slice(0, 4).toString('ascii');
  if (magic === 'ASC2') {
    if (buf.length < 18) throw new Error('ASC2 header truncated');
    const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    return {
      isV2: true,
      fps: dv.getFloat32(4, false),
      mode: buf[8],
      pixel: buf[9] === 1,
      cols: dv.getUint16(10, false),
      rows: dv.getUint16(12, false),
      totalFrames: dv.getUint32(14, false),
      headerLen: 18,
    };
  }
  if (magic === 'ASCF') {
    const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    return {
      isV2: false,
      fps: dv.getFloat32(4, false),
      mode: buf[8],
      pixel: buf[9] === 1,
      cols: dv.getUint16(10, false),
      rows: dv.getUint16(12, false),
      totalFrames: 0,
      headerLen: 14,
    };
  }
  throw new Error('invalid ascf magic: ' + magic);
}

async function playFile(file) {
  const buf = fs.readFileSync(file);
  const h = parseHeader(buf.subarray(0, 18));
  const dec = codec.makeDecoder(h.pixel ? 3 : 4); // the browser player's decoder

  let off = h.headerLen;
  let frames = 0;
  let fullFrames = 0; // non-delta records (raw/zlib/RLE-full/profile)
  let prevIndex = -1;

  while (off + 4 <= buf.length) {
    const len = buf.readUInt32BE(off);
    off += 4;
    if (len === 0) throw new Error(`${file}: zero-length record at ${off - 4}`);
    if (off + len > buf.length) throw new Error(`${file}: record ${len}B overruns EOF at ${off}`);
    const msg = buf.slice(off, off + len);
    off += len;

    const tag = msg.length >= 5 ? msg[4] : -1;
    const isProfile = tag === codec.TAG_PROFILE || tag === codec.TAG_PROFILE_AQ;
    const expect = isProfile
      ? h.cols * h.rows * 3 // profile frames decode to BGR pixels
      : h.cols * h.rows * (h.pixel ? 3 : 4);

    const { frameIndex, frame } = await dec.decode(msg);
    if (frameIndex < prevIndex) {
      throw new Error(`${file}: frame index regression ${prevIndex} -> ${frameIndex}`);
    }
    prevIndex = frameIndex;
    if (frame.length !== expect) {
      throw new Error(
        `${file}: frame ${frameIndex} (tag ${tag}) decoded ${frame.length} B, ` +
          `expected ${expect} (${h.cols}x${h.rows}, pixel=${h.pixel})`
      );
    }
    frames++;
    if (isProfile || tag !== codec.TAG_DELTA) fullFrames++;
  }

  return { file, h, frames, fullFrames };
}

(async () => {
  const files = process.argv.slice(2);
  if (files.length === 0) {
    console.error('usage: node experiments/play_ascf.js clip.ascf [more.ascf ...]');
    process.exit(1);
  }
  for (const f of files) {
    const r = await playFile(f);
    console.log(
      `OK ${path.basename(f)}: ${r.h.cols}x${r.h.rows} ${r.h.pixel ? 'pixel' : 'ascii'} ` +
        `mode=${r.h.mode} fps=${r.h.fps} ${r.frames} frames ` +
        `(${r.fullFrames} full, ${r.h.totalFrames || 'n/a'} declared) — browser decoder clean`
    );
  }
})().catch((e) => {
  console.error('FAIL:', e.message);
  process.exit(1);
});
