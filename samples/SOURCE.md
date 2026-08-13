# Evidence source and attribution

The visual evidence uses an 8-second excerpt of **Big Buck Bunny**, the Blender
Foundation animated short, rather than a synthetic test pattern.

- Official project: <https://peach.blender.org/>
- Official downloads: <https://download.blender.org/peach/bigbuckbunny_movies/>
- License: Creative Commons Attribution 3.0 Unported (CC BY 3.0)
- Attribution: Big Buck Bunny © Blender Foundation / peach.blender.org
- Excerpt: approximately 00:01:00–00:01:08 from the official 640×360 release;
  the 60-fps excerpt is approximately 00:01:00–00:01:04 from the official
  1080p 60-fps release, resized to 640×360.

The committed source excerpts are lossless H.264 intermediates created from the
official downloads. Their hashes are checked by `experiments/make_samples.sh`:

```text
8f113ef593688f47ec8d8b0d093fb955cb04bc350826c775d2e9ca451870856e  big_buck_bunny_excerpt_30fps.mp4
1cf8e47cdef1c3acb4cab994a463a0ca6dabe1532bc89f09f90873dae45e98e8  big_buck_bunny_excerpt_60fps.mp4
```
