# Evidence source and attribution

The visual evidence uses two committed, pinned, Creative-Commons-licensed
sources rather than synthetic test patterns.

## 1. Big Buck Bunny (visual quality, 30/60 fps cartoon)

An 8-second excerpt of **Big Buck Bunny**, the Blender Foundation animated
short.

- Official project: <https://peach.blender.org/>
- Official downloads: <https://download.blender.org/peach/bigbuckbunny_movies/>
- License: Creative Commons Attribution 3.0 Unported (CC BY 3.0)
- Attribution: Big Buck Bunny © Blender Foundation / peach.blender.org
- Excerpt: approximately 00:01:00–00:01:08 from the official 640×360 release;
  the 60-fps excerpt is approximately 00:01:00–00:01:04 from the official
  1080p 60-fps release, resized to 640×360.

## 2. Drone flight (real 60 fps aerial footage)

An 8-second 720p60 excerpt of **Family Christmas Drone Flight 4k 60FPS
Nashville, Michigan**, a genuine 59.94/60 fps aerial clip used to demonstrate
that real-world content above 30 fps is compiled and displayed at native rate.

- File page: <https://commons.wikimedia.org/wiki/File:Family_Christmas_Drone_Flight_4k_60FPS_Nashville,_Michigan.webm>
- License: Creative Commons Attribution 3.0 Unported (CC BY 3.0)
- Attribution: Joseph Challender (drone footage)
- Excerpt: approximately 00:01:30–00:01:38 from the original 4K 59.94 fps
  source, trimmed and resized to 1280×720 at 60 fps with a near-lossless
  H.264 intermediate. `framemd5` confirms ~482 of 480 decoded frames are
  unique, i.e. genuine motion rather than duplicated frames.

The committed source excerpts are lossless/near-lossless H.264 intermediates
created from the official downloads. Their hashes are checked by
`experiments/make_samples.sh`:

```text
8f113ef593688f47ec8d8b0d093fb955cb04bc350826c775d2e9ca451870856e  big_buck_bunny_excerpt_30fps.mp4
1cf8e47cdef1c3acb4cab994a463a0ca6dabe1532bc89f09f90873dae45e98e8  big_buck_bunny_excerpt_60fps.mp4
0799500294387e7c60f9fe611cdddc611c6c30d9171d63c9d3f399385781d208  drone_excerpt_720p60.mp4
```
