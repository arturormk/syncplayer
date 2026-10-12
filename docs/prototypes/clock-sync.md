# Shared-clock prototype

## Purpose

This prototype validates ADR 0004, the release primitive from ADR 0010, and basic multi-Slave
joining and reconnection on physical Linux hosts. It is not the production Syncplayer protocol,
configuration model, or playlist engine.

The Master forces a monotonic `GstSystemClock` onto its pipeline and exposes that same clock through
`GstNetTimeProvider`. The Master starts without waiting for a Slave. Each Slave calibrates a
`GstNetClientClock`, forces it onto its pipeline, and reports readiness after its local file is
prerolled. The Master then supplies its projected media position at a future absolute clock
timestamp. This lets a late Slave join without interrupting existing playback.

## Test asset

Use a constant-frame-rate, intra-frame-friendly test video lasting at least 60 seconds: 60 fps for
the general experiment, or 30 fps when using the Raspberry Pi compatibility profile below. Every
frame should visibly contain its sequential frame number. Add a full-screen black/white transition
and an audio click once per second. Copy the exact file to both machines and verify its cryptographic
checksum before testing.

If either test host is a Raspberry Pi, prepare the asset with
[`tools/prepare-video-for-rpi.sh`](../../tools/prepare-video-for-rpi.sh), including `--audio` when
the test asset has the required click track. Use that same prepared output on every host in this
prototype so decoder or rendition differences do not contaminate the clock measurement. See
[`docs/media-preparation.md`](../media-preparation.md) for the production rendition guidance.

The prototype deliberately does not include an asset generator: codec/container choices are part
of the test matrix and generated binaries do not belong in the repository.

## Procedure

1. Record the host models, Linux versions, GStreamer versions, display refresh rates, connection
   type, media checksum, and complete commands.
2. Run the Master alone and verify that it starts immediately. Start the Slaves later with
   `RUST_LOG=syncplayer=debug` and capture every host's logs separately.
3. Film both displays in one high-frame-rate camera view. Do not infer display synchronization only
   from queried GStreamer positions.
4. Repeat ten cold starts and record the largest observed frame separation.
5. Run continuously for sixty minutes and inspect the initial and final numbered frames together.
6. Disconnect the Master or its network after release and verify that every Slave continues. Bring
   the Master back and verify that Slaves reconnect, pause, seek, and rejoin its new timeline.
7. Repeat with several Slaves, removing and restarting one at a time, and verify that neither the
   Master nor the other Slaves are interrupted.
8. Start the Master with `--startup-delay 30`, start several Slaves during that grace period, and
   verify that all ready players leave their prerolled first frame together. Start another Slave too
   late and verify that the deadline is not postponed and that it catches up afterward.

## Acceptance criteria

- Ten consecutive starts show no more than one frame of separation.
- Sixty minutes of playback show no sustained drift beyond one frame.
- Failure to synchronize the clock or preroll within the configured timeout exits clearly without
  starting unsynchronized playback.
- A malformed, oversized, truncated, incompatible, stale, or non-future control message is rejected.
- Loss of the control connection after release does not stop playback.
- A late or restarted Slave joins without pausing the Master or other Slaves.
- Slaves free-run through Master loss and resynchronize when it returns.
- A configured startup delay uses one shared deadline and never waits beyond it for missing Slaves.

Operator-driven pause, resume and goto, correction thresholds, and session identity are explicitly
subsequent experiments. Reconnection currently always performs an authoritative pause/seek/rejoin.
`ReleasePlan` distinguishes start, resume, and goto so later experiments can reuse the clock-domain
model without treating this lab protocol as stable.

## Results

Add dated result documents beside this file. Record raw measurements and failures; do not adjust the
acceptance criteria after observing results without explaining why.

