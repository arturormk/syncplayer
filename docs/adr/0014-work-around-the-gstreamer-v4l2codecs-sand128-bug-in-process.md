# 0014 - Work around the GStreamer v4l2codecs SAND128 bug in process

Status: Proposed
Date: 2026-11-08
Supersedes: None
Superseded by: None

## Context

On a Raspberry Pi 4 (Raspberry Pi OS trixie, GStreamer 1.26.2 with `+rpt` patches), HEVC is
decoded by `v4l2slh265dec` from the v4l2codecs plugin in gst-plugins-bad. Decoding is correct, but
the decoder keeps its V4L2 capture queue on the two-plane `Nc12` format (Y and UV planes with column
heights `H` and `H/2`) while advertising `DMA_DRM` `NV12:0x0700000000000004`, the single-plane
Broadcom SAND128 modifier with column height 0. `glimagesink` imports the DMA-BUF according to the
modifier, so frames are shown with scrambled colors.

No modifier can describe two planes with different column heights, so rewriting the caps label
cannot fix the import. Converting through `videoconvert` is correct but too slow to keep up at
1920x1200@25. A dedicated column-by-column copy to linear NV12 measured about 10 ms per frame on
the device. H.264 media avoids the problem and remains the default recommendation; the proper fix
belongs upstream.

ADR 0001 builds playback on GStreamer. The project forbids `unsafe` code; GStreamer element
subclassing through gstreamer-rs compiles under that rule.

## Decision

Syncplayer ships an opt-in workaround, named after the upstream bug so it is not mistaken for a
Syncplayer feature:

- It is enabled per player by `workaround_gst_v4l2codecs_sand_bug = true` in a `[players.<id>]`
  table, with `[players.default]` fallback, or by `--workaround-gst-v4l2codecs-sand-bug[=BOOL]`,
  which overrides the program file. It is off by default.
- When enabled, an in-process `BaseTransform` element, `syncplayersanduntile`, registered only
  inside the Syncplayer process, is placed at the head of the video output chain in both the
  `playbin3` and the separate-audio `uridecodebin3` pipelines, including `--kms-gl`.
- The element removes SAND128 modifiers from `DMA_DRM` caps, offers system-memory `NV12_128C8`
  upstream wherever downstream accepts system-memory `NV12`, and untiles `NV12_128C8` to `NV12`. It
  derives each plane's column height from its memory size. Every other format and memory type
  passes through unchanged.
- Under `--kms-gl` with the workaround enabled, a negotiated output without DMA-BUF is logged as a
  warning instead of rejected.
- Independently of the switch, Syncplayer warns when it negotiates SAND128 `DMA_DRM` caps.
- The key is part of the canonical program hash only when present, so existing hashes are
  unchanged.

## Consequences

Positive:

- HEVC media plays with correct colors on Raspberry Pi 4 hosts that enable the workaround.
- Hosts and media that do not need it are unaffected; H.264 keeps its zero-copy path.
- The workaround is isolated in one module and one switch, so it is easy to remove.

Negative / Trade-offs:

- Untiling costs CPU (about 26% of one core at 1920x1200@25, more for larger frames) and the GPU
  upload of system-memory video replaces a zero-copy import.
- Any SAND128 `DMA_DRM` path is refused while the workaround is enabled, including a correctly
  labelled one from a future fixed decoder.
- The single-memory input layout is inferred from the video meta and has not been observed on
  hardware; inconsistent geometry fails with an error rather than drawing wrong colors.

Follow-ups:

- Measure total CPU, dropped frames, and on-screen colors on a Raspberry Pi 4, with and without
  `--kms-gl`, and at 1920x3530 if HEVC is used at that size.
- Report the bug to GStreamer (v4l2codecs) and Raspberry Pi; retire this workaround through a new
  ADR once fixed packages are deployed.

## Source blueprints

- Investigation report `SYNCPLAYER-RPI-HEVC-WORKAROUND.md` (2026-11-08), sections "Findings" and
  "Proposed syncplayer workaround".

Source citations record provenance only; they do not incorporate the cited document wholesale
into this decision.
