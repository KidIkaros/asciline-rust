# Big Buck Bunny compile speed analysis

Same source: 240 frames at 30 fps, 240 columns. Compile FPS is frames processed per wall-second; display FPS remains the source FPS.

| Format | Display FPS | Frames | Wall time | Compile FPS | Output bytes | PSNR-Y | SSIM-Y |
|---|---:|---:|---:|---:|---:|---:|---:|
| ASCII mode | 30 | 240 | 1.11 s | 216.2 | 2620569 | — | — |
| PIXEL lossless | 30 | 240 | 2.16 s | 111.1 | 9779783 | — | — |
| PROFILE QF=70 | 30 | 240 | 3.36 s | 71.4 | 337023 | 36.57 | 0.9686 |
| PROFILE QF=70 (no quality report) | 30 | 240 | 2.26 s | 106.2 | 337023 | — | — |

## Interpretation

- **Display FPS** is the source/container playback rate. It is 30 fps for all
  four outputs; the comparison video should not make one panel appear slower.
- **Compile FPS** is offline encoding throughput and is the relevant speed
  comparison for `.ascf` production.
- The no-quality profile row isolates the SSIM/quality-report cost from DCT
  encoding cost.
- The default ±7 motion search (225 candidates) is ~4.6× the SAD work of
  codec.py's ±3 (49 candidates), so profile compile is ~2.7× slower than a
  ±3 baseline; pass `--r-search 3` to trade back the size/quality gain for
  the original speed.
- GIFs are previews and may play at a browser-controlled rate. Use the MP4
  comparison and this table for timing claims.
