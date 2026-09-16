# 0001 - Build the playback engine with Rust and GStreamer on Linux

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

The player needs mature codec support, timestamp-aware audio/video pipelines, network-clock
integration, hardware decoding, and Linux display/audio integration. Reimplementing decoding,
frame scheduling, and device support would dominate the project and increase reliability risk.
The initial deployment targets are Ubuntu-class x86-64 systems and 64-bit Raspberry Pi systems.

## Decision

Implement Syncplayer in Rust and use GStreamer through its Rust bindings as the media framework.
Target Linux on `x86_64` and `aarch64` initially.

Keep unsafe Rust tightly contained at platform or FFI boundaries. Use GStreamer for media decode,
timestamp propagation, clocking, and sink integration rather than implementing those facilities
inside Syncplayer.

This decision does not prescribe a particular high-level GStreamer element, pipeline topology,
plugin bundle, window system, hardware decoder, or static-linking strategy. Those choices remain
subject to prototypes and platform measurements.

## Consequences

Positive:

- The project builds on a mature, cross-platform media graph and codec ecosystem.
- Rust provides a strong type and ownership model for protocol and runtime state.
- GStreamer clocks and timestamps align with the synchronization problem.

Negative / Trade-offs:

- Deployment must manage native GStreamer libraries, plugins, and host GPU integrations.
- GLib/GStreamer and Rust async-runtime integration requires deliberate boundaries.
- Behavior and available acceleration vary by platform and plugin set.

Follow-ups:

- Prototype clock use, gapless transitions, hardware decode, and display selection before fixing
  the final pipeline architecture.
- Treat packaging as its own workstream and report the selected decoder and sinks at runtime.

## Source blueprints

- `../BLUEPRINT.md`, sections 3–5, 47, 80, and 85
