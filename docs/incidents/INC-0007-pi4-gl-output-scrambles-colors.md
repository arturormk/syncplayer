# INC-0007 — Raspberry Pi 4 GL output scrambles colors

- Status: Resolved
- Date: 2026-11-08
- Affected component: Media pipeline and video output (Raspberry Pi 4, GBM/KMS)
- Severity: Installation

## Context and trigger

The Raspberry Pi 4 players run Raspberry Pi OS Lite (trixie, kernel `6.18.50+rpt-rpi-v8`)
with GStreamer 1.26.2 and Mesa 26.2.2, without X11 or Wayland. With the default sink selection
(`autovideosink` → `glimagesink`) or with `--kms-gl`, GStreamer GL renders through EGL on a GBM
surface and presents it with KMS.

Minimal reproduction, with no syncplayer and no hardware decoder involved:

```sh
GST_GL_WINDOW=gbm gst-launch-1.0 filesrc location=clip.mp4 ! qtdemux ! h264parse ! avdec_h264 ! glimagesink
```

## Symptom

Picture geometry, text and grid lines were recognizable, but colors were noisy and wrong. Smooth
gradients showed repeated color bands and fine detail turned to grey noise. Flat saturated content
such as SMPTE bars and checkerboards looked almost correct. `kmssink` showed the same media
correctly. The symptom was the same for H.264 (`v4l2h264dec`, linear `YU12`) and HEVC
(`v4l2slh265dec`, `NV12:0x0700000000000004` SAND128).

## Impact

The Pi 4 players were unusable with GPU output. The only correct path, `kmssink`, could not keep
up with 4K HEVC.

## Investigation

The decisive experiments read the live scanout buffer through `drmModeGetFB2` and a PRIME mmap,
instead of relying on photographs of the monitor:

1. Hardware-decoded frames passed through `glupload ! glcolorconvert ! gldownload` offscreen were
   pixel-identical to `avdec_h264` output. Decoding, DMA-BUF import and GL YUV→RGB conversion were
   correct.
2. A `tee` that read decoded buffers back on the CPU while `glimagesink` played showed clean frames
   at the same moment the screen was corrupt. Reference-frame corruption was ruled out. The
   corruption also did not reset at keyframes.
3. Software-decoded H.264 converted to RGBA on the CPU, and a grayscale `videotestsrc` zone plate,
   were equally corrupt through `glimagesink`. The decoder, DMA-BUF and SAND were all ruled out.
4. A checkerboard was geometrically exact in the scanout buffer, which ruled out a tiling mismatch.
5. A solid `0xff808080` frame produced the scanout word `0xE0180601` in a plane that KMS reported
   as `AR24`, modifier `0`. That word is exactly A2R10G10B10 with alpha 3 and R=G=B=513
   (0.502 × 1023). Black became `0xC0000000` and white stayed `0xFFFFFFFF`.
6. The `GST_DEBUG=glcontext:5` log showed the chosen EGL config `[10, 10, 10, 2]` with native
   visual `AR30`.
7. `GST_GL_CONFIG=gst-gl-context-config,red-size=8,green-size=8,blue-size=8,alpha-size=8` made the
   same frame `0xFF808080` and the hardware-decoded H.264 and HEVC output correct, with no dropped
   frames. Requesting `alpha-size=0` did not help, because the 10-bit config still matched.

An earlier hypothesis that the v4l2codecs SAND128 label caused the corruption was rejected. An
in-process SAND128 untiler (reverted in b2f5524) changed nothing visible, consistent with this
root cause.

## Root Cause

Mesa v3d lists an A2R10G10B10 window config first for an 8-bit RGB request. `GstGLWindowGBMEGL`
creates its `gbm_surface` in that config's native format (`AR30`). `gst_gl_gbm_drm_fb_get_from_bo`
in `gst-libs/gst/gl/gbm/gstgl_gbm_utils.c` then registers each buffer with the legacy
`drmModeAddFB(depth 32, bpp 32)`, which the kernel maps to ARGB8888. The GPU writes 2-10-10-10
pixels, and the display reads them as 8-8-8-8. Values 0 and 255 survive this mismatch, while every
intermediate value is scrambled.

Upstream fix location (inference, not submitted): `gst_gl_gbm_drm_fb_get_from_bo` should call
`drmModeAddFB2` with `gbm_bo_get_format(bo)` instead of deriving a depth/bpp pair.

## Resolution

Syncplayer installs a bus sync handler on every playback pipeline. When a GL element announces a
GBM `GstGLDisplay` (`HAVE_CONTEXT`), or when `--kms-gl` creates one, syncplayer connects to the
display's `create-context` signal. It creates the context itself after
`gst_gl_context_request_config` with an RGBA8888 config. If that context cannot be created, the
handler returns `NULL` and GStreamer uses its default. `GST_GL_CONFIG` still overrides the request,
as GStreamer documents. X11, Wayland and other display types are left alone.

## Verification

- `cargo clippy --all-targets` and `cargo test` pass. `gbm_egl_config_excludes_ten_bit_scanout_formats`
  pins the requested config.
- On a Raspberry Pi 4B player, the patched `syncplayer master` with the production arguments logged
  `created GBM GL context with 8-bit scanout config` and selected `v4l2h264dec` → `glimagesink`.
  Scanout grabs at 6.7 s, 10.8 s and 14.9 s matched CPU-decoded reference frames.
- `gst-launch-1.0` with the same config: `v4l2h264dec` (1920×1200 H.264) and `v4l2slh265dec`
  (SAND128 HEVC) showed correct colors with no dropped-frame warnings and about 5% CPU.

## Prevention and guardrails

- Judge Pi display problems from the scanout buffer, not from photographs. Grabbing the active KMS
  framebuffer separates rendering, layout and value-encoding faults in a few minutes.
- When `GST_DEBUG=glcontext:5` reports a config other than `[8, 8, 8, 8]` on a GBM display, check
  the scanout format before suspecting decoders.

## References

- Code: `src/main.rs` (`GBM_EGL_CONFIG`, `guard_gbm_scanout_format`, `request_eight_bit_gbm_contexts`)
- Related documentation: `PI4-Troubles.md`
- Commit or pull request: branch `fix/pi4-gbm-8bit-egl-config`

## Tags

- `gstreamer`, `gl`, `egl`, `gbm`, `kms`, `raspberry-pi`, `v3d`, `colors`, `hevc`, `h264`
