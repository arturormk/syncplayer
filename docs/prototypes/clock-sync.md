# Shared-clock prototype

## Purpose

This prototype validates ADR 0004 and the release primitive from ADR 0010 on two physical Linux
hosts. It is not the production Syncplayer protocol, configuration model, or playlist engine.

The Master forces a monotonic `GstSystemClock` onto its pipeline and exposes that same clock through
`GstNetTimeProvider`. The Slave calibrates a `GstNetClientClock`, forces it onto its pipeline, and
reports readiness only after its local file is prerolled. Both pipelines use the same future
absolute clock timestamp as their base time.

## Test asset

Use a 60 fps, constant-frame-rate, intra-frame-friendly test video lasting at least 60 seconds.
Every frame should visibly contain its sequential frame number. Add a full-screen black/white
transition and an audio click once per second. Copy the exact file to both machines and verify its
cryptographic checksum before testing.

The prototype deliberately does not include an asset generator: codec/container choices are part
of the test matrix and generated binaries do not belong in the repository.

## Procedure

1. Record the host models, Linux versions, GStreamer versions, display refresh rates, connection
   type, media checksum, and complete commands.
2. Run Master and Slave with `RUST_LOG=syncplayer=debug` and capture their logs separately.
3. Film both displays in one high-frame-rate camera view. Do not infer display synchronization only
   from queried GStreamer positions.
4. Repeat ten cold starts and record the largest observed frame separation.
5. Run continuously for sixty minutes and inspect the initial and final numbered frames together.
6. Disconnect the TCP control path after release and verify playback continues. The implementation
   already closes this connection normally; firewall or cable testing additionally exercises the
   clock's behavior when its UDP samples disappear.

## Acceptance criteria

- Ten consecutive starts show no more than one frame of separation.
- Sixty minutes of playback show no sustained drift beyond one frame.
- Failure to synchronize the clock or preroll within the configured timeout exits clearly without
  starting unsynchronized playback.
- A malformed, oversized, truncated, incompatible, stale, or non-future control message is rejected.
- Loss of the control connection after release does not stop playback.

Pause, resume, goto, gapless transitions, playlist loops, correction policy, and Master reconnection
are explicitly subsequent experiments. `ReleasePlan` already distinguishes start, resume, and goto
so those experiments can reuse the clock-domain model without treating this lab protocol as stable.

## Results

Add dated result documents beside this file. Record raw measurements and failures; do not adjust the
acceptance criteria after observing results without explaining why.

