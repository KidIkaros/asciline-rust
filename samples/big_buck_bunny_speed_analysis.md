# Big Buck Bunny compile speed analysis

Same source: 240 frames at 30 fps, 240 columns. Compile FPS is frames processed per wall-second; display FPS remains the source FPS.

| Format | Display FPS | Frames | Wall time | Compile FPS | Output bytes | PSNR-Y | SSIM-Y |
|---|---:|---:|---:|---:|---:|---:|---:|
| ASCII mode | 30 | 240 | 1.13 s | 212.4 | 2620569 | — | — |
| PIXEL lossless | 30 | 240 | 2.17 s | 110.6 | 9779783 | — | — |
| PROFILE QF=70 | 30 | 240 | 2.63 s | 91.3 | 472221 | 39.52 | 0.9816 |
| PROFILE QF=70 (no quality report) | 30 | 240 | 2.20 s | 109.1 | 472221 | — | — |

## Interpretation

- **Display FPS** is the source/container playback rate. It is 30 fps for all
  four outputs; the comparison video should not make one panel appear slower.
- **Compile FPS** is offline encoding throughput and is the relevant speed
  comparison for `.ascf` production.
- The no-quality profile row isolates the SSIM/quality-report cost from DCT
  encoding cost.
- GIFs are previews and may play at a browser-controlled rate. Use the MP4
  comparison and this table for timing claims.
