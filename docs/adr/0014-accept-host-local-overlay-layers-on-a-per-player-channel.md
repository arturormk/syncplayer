# 0014 - Accept host-local overlay layers on a per-player channel

Status: Proposed
Date: 2026-11-08
Supersedes: None
Superseded by: None

## Context

Syncplayer plays pre-rendered per-projector videos. Installations that map detail onto physical
objects (for example dozens of flowers rising from the floor) need content placed precisely on
each object. Doing that by re-rendering the per-projector videos in an editing tool for every
adjustment is slow, and some content is not known when the media is rendered: alignment aids,
masks that stop light spilling around objects, labels with the projector name, and similar.

Operators want to send images to a running player and have them drawn over its video: a full
1920x1200 image with transparency or, more usefully, only the bounding rectangle of the content
(a sprite placed and warped on an object, or a text label). An external tool may later let a user
move, scale, rotate, and skew such sprites interactively and push the result to each projector.

The playback pipeline already contains an overlay point. The diagnostic OSD is a `timeoverlay`
element, and with `--kms-gl` startup requires the negotiated caps to carry
`meta:GstVideoOverlayComposition`. The OSD then travels as composition metadata and `glimagesink`
blends it on the GPU while decoded frames stay zero-copy DMA-BUF. A composition is a list of
independently positioned and scaled RGBA rectangles, which matches the rectangle-shaped content
operators want to send.

The synchronization protocols (ADRs 0004 and 0006) are bounded for timing traffic: 16 KiB control
frames and 1 KiB state datagrams. Image payloads do not fit them, and mixing bulk transfers into
timing traffic would risk the playback guarantees.

Alternatives considered:

- Full-frame images only. Simple, but every update moves and uploads about 9 MB, and the CPU path
  must blend a full frame per video frame.
- Warping on the player (sprite plus a projective transform in a GL shader). Smooth interactive
  manipulation, and the same machinery could warp the video itself, but it is a large change to
  the GL output path that INC-0007 showed to be fragile.
- Distributing layers through the Master. Layers are inherently per projector, and routing them
  through the authority would couple an optional tool to synchronization.

## Decision

A player may accept **overlay layers** on its own, optional TCP listener, enabled only by an
explicit `--overlay-bind IP:PORT` (suggested port 46002). The listener is independent of the
Master/Slave control, clock, and state channels and never influences transport state.

Binding choices:

- **Layers are named rectangles.** Each layer has an id (1-64 ASCII letters, digits, `.`, `_`,
  `-`), a render rectangle `x, y, w, h` in output-frame pixels, a signed stacking order `z`, a
  whole-layer opacity, and RGBA pixels sent as PNG. The image is scaled to its render rectangle.
  Placing a layer means the client chooses the rectangle; full-screen is the rectangle
  `0,0,1920,1200`. A zero render size means "the image's own size".
- **Operations** are `PUT` (create or replace from PNG), `MOVE` (change rectangle, `z`, and opacity
  without new pixels), `DELETE`, `CLEAR`, and `LIST`. Every request receives `OK`, `ERROR`, or a
  layer listing.
- **The wire format** is a versioned binary envelope with magic `SYNO`, mirroring the existing
  protocol style, described in [`../overlay-protocol.md`](../overlay-protocol.md). An optional
  shared token (`--overlay-token`) is carried in every request.
- **Limits** bound resource use: 16 MiB requests, 4096x4096 decoded images, 32 layers, a budget of
  32 Mi decoded pixels per player, and 4 concurrent connections. PNG decoding happens on the
  connection thread, never on a streaming thread.
- **Rendering** attaches layers around the OSD element so the OSD always stays on top. When the
  sink composites overlay metadata (the `--kms-gl` path), layers are merged into the OSD's
  composition on its source pad, keeping a single composition meta per buffer. Otherwise layers are
  blended into the frame on the OSD's sink pad. Every change publishes one new immutable
  composition; unchanged layers keep their rectangle objects so GPU sinks can keep their textures.
- **Layers are host-local and ephemeral.** They are not part of the shared program or its hash
  (ADR 0008), are not synchronized between players, carry no timing, and are lost on restart.
  A Slave's listener starts before it reaches its Master, and layers received meanwhile appear
  when video starts.
- **Layers are static images.** Animated overlays and pre-rendered overlay videos are out of scope
  and would be a separate decision.

Current preferences, not binding: layers are best used with `--kms-gl` on Raspberry Pi, where
blending costs one texture upload per change. On CPU sinks small rectangles are cheap, but large
layers cost a per-frame blend proportional to their area. Opacity is baked into a faded copy of
the pixels instead of using rectangle global alpha, because `GStreamer` applies global alpha by
rewriting the pixels of shared rectangles in place.

## Consequences

Positive:

- Operators and external tools can place, align, and remove content on each projector at run time
  without re-rendering media or restarting playback.
- Rectangle-shaped updates keep network, decode, and upload costs proportional to the content.
- The synchronization protocols and their limits are unchanged; overlay traffic cannot delay
  timing traffic.
- The `--kms-gl` path keeps zero-copy video and GPU blending.

Negative / Trade-offs:

- A player with the listener enabled can be drawn on by anyone who can reach the port and knows
  the token. The token is a shared secret sent in clear text, and is visible in the process list
  when passed on the command line. Bind to a trusted interface.
- Arbitrary rotation, skew, and perspective must be rendered by the client into the rectangle's
  pixels; every manipulation step resends a PNG. This suits alignment work at a few updates per
  second, not smooth 60 fps manipulation.
- Layers do not survive restarts and are not recorded in the program.
- Large layers on CPU sinks can cost noticeable per-frame CPU time.

Follow-ups:

- Measure upload and blend cost on Raspberry Pi 4 with `--kms-gl`.
- Build the external sprite/warp tool that renders rectangles per projector and pushes them.
- Consider persisting a player's layers in the program or a host-local file once that tool exists.
- Consider a player-side GPU warp (sprite plus projective transform) if interactive smoothness or
  warping the video itself becomes necessary.
- Consider a separate feature for synchronized overlay videos.

## Source blueprints

- `../BLUEPRINT.md`, section 61 (OSD as a runtime overlay that is not part of the program media)

Source citations record provenance only; they do not incorporate the cited document wholesale
into this decision.

When this decision is replaced, create and accept a new ADR that names this ADR in `Supersedes`.
Then change this record to `Status: Superseded` and set `Superseded by` to the new ADR's linked
name and acceptance date. Do not rewrite the historical decision.
