#!/usr/bin/env node
/**
 * fps_count.js — connect to the running asciline-server like the browser would
 * and count decoded frames for DURATION seconds. Proves the Rust server streams
 * at 60fps when asked (--fps 60), i.e. beyond the Python original's 30fps cap.
 *
 * Usage: node experiments/fps_count.js <port> [duration_s]
 */
'use strict';

const DURATION = parseFloat(process.argv[3] || '3');
const PORT = parseInt(process.argv[2] || '8000', 10);

const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?codec=adaptive`);
ws.binaryType = 'arraybuffer';

let frames = 0;
let inited = false;
const t0 = Date.now();

ws.onmessage = (e) => {
  if (typeof e.data === 'string') {
    if (e.data.startsWith('INIT:')) {
      inited = true;
      const p = e.data.split(':');
      console.log(`INIT: fps=${p[1]} mode=${p[2]} grid=${p[3]}x${p[4]}`);
    }
  } else if (inited) {
    frames++;
  }
};

setTimeout(() => {
  const elapsed = (Date.now() - t0) / 1000;
  console.log(`frames in ${elapsed.toFixed(1)}s: ${frames} → ${(frames / elapsed).toFixed(1)} fps`);
  ws.close();
  process.exit(0);
}, DURATION * 1000);

ws.onerror = () => {
  console.error('ws error — is the server running?');
  process.exit(1);
};
