# Shared-clock prototype

## Purpose

This prototype validates ADR 0004, the release primitive from ADR 0010, application-level
liveness, periodic authoritative state, sustained-drift recovery, and multi-Slave reconnection on
physical Linux hosts. Its transport is still experimental, but it now consumes the shared program
configuration and uses the production subscription and participant-registry model.

The Master lets its pipeline select its natural monotonic clock—normally an audio-device clock when
real audio is active—and exposes that same clock through `GstNetTimeProvider`. The Master starts
without waiting for a Slave. Each Slave calibrates a fresh `GstNetClientClock`, forces it onto its
pipeline, receives the Master's projected media position at a future absolute clock timestamp, and
acknowledges after its local pipeline has prepared that release. This lets a late Slave join without
interrupting existing playback.

The bounded binary protocol is currently version 8. Before clock calibration, a Slave
subscribes with its player identity, program identity and hash, software version, capabilities,
state endpoint, and probed cue durations. The Master rejects incompatible or duplicate active
participants and returns its authoritative cue durations on success. A Master session UUID
identifies one process run; the transport generation remains stable when Slaves join and changes for
goto, stop, pause/release, debug-loop changes, and program loads. Master and Slave binaries must
have the same protocol version.

## Runtime recovery behavior

- The Master sends complete authoritative UDP state snapshots to each registered Slave at 10 Hz by
  default. Loss, duplication, and reordering are handled by complete snapshots and sequence checks.
- Each Slave sends reliable TCP keepalives at a configurable cadence. A missing acknowledgement
  closes the old control socket even if TCP still reports it as established.
- Playback free-runs during the outage. Reconnection retries once per second, creates a new network
  clock, calibrates it, and uses the normal prepared pause/seek/future-release path.
- Snapshots contain session context, program identity/hash, transport generation and state,
  operation, sequence-position and clock anchors, cue index/ID/occurrence/duration, item-local
  position, playlist iteration, and optional debug-loop state. Each host maps that cue-local anchor
  through its own rendition durations, avoiding false identity failures at playlist boundaries.
  Unchanged state does not seek; stale state is ignored; a newer generation requires a prepared
  rejoin.
- Snapshots come from the Master's logical plan and authoritative clock, not its renderer's queried
  position. A Master renderer stall is therefore repaired locally instead of being published as a
  timeline for Slaves to chase.
- Every player samples drift once per second. Three valid samples above 100 ms produce a warning;
  three valid samples above 250 ms perform one prepared local repair. A repair starts a ten-second
  cooldown.
- Slaves report periodic observations over their existing reliable control sessions. The Master
  aggregates current/minimum/maximum/RMS error, health and recovery counters, and error slope in
  parts per million; it can append the complete sample stream to CSV for later physical analysis.
- Drift samples are suppressed during pause, preroll, seek, scheduled release, ambiguous playlist
  handoff, recovery, or unhealthy-clock periods. Loop and cue-boundary errors use sequence-aware
  positions, including absolute playlist occurrence.
- Reconnect and local-drift recovery share one serialized coordinator. Starting a newer recovery
  cancels its predecessor's scheduled release, and attempt IDs prevent late workers from starting
  playback.
- A Slave's local EOS is held and treated as diagnostic rather than logical cue authority. Master
  cue boundaries and playlist wraps are projected from the exported clock, and `CLOCK_LOST` on the
  Master pauses, re-pins that clock, resumes, and re-anchors the logical transport.

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
6. Disconnect the Master or its network after release and verify that every Slave detects missing
   keepalive acknowledgements within eight seconds and continues. Bring the Master back and verify
   that Slaves
   create a new network clock, pause, seek, and rejoin its timeline once.
7. Repeat with several Slaves, removing and restarting one at a time, and verify that neither the
   Master nor the other Slaves are interrupted.
8. Start the Master with `--startup-delay 30`, start several Slaves during that grace period, and
   verify that all ready players leave their prerolled first frame together. Start another Slave too
   late and verify that the deadline is not postponed and that it catches up afterward.
9. Suspend the Master for longer than eight seconds without first closing its sockets. On resume,
   verify that the stale connection is not reused and every Slave performs one prepared rejoin.
10. Temporarily block TCP control traffic while leaving UDP clock traffic available, then reverse
    the test. Record the different free-running and clock-calibration behavior.
11. Apply CPU and I/O pressure to the Master. Verify that a stalled Master renderer repairs locally
    and does not make healthy Slaves jump backward.
12. Inject or otherwise reproduce more than 250 ms of sustained local position error. Verify that
    three valid samples trigger one correction and that cooldown prevents one seek per second.
13. Lock the Master desktop without suspend and run for at least 30 minutes. Then run the complete
    system for 60 minutes with real audio and video sinks.

## Acceptance criteria

- Ten consecutive starts show no more than one frame of separation.
- Sixty minutes of playback show no sustained drift beyond one frame.
- Failure to synchronize the clock or preroll within the configured timeout exits clearly without
  starting unsynchronized playback.
- A malformed, oversized, truncated, incompatible, stale, or non-future control message is rejected.
- A Slave with a mismatched program identity or hash, missing capability, colliding player ID, or
  duplicate active player ID is rejected before clock calibration.
- Cue-duration differences obey the configured tolerance and warn/reject policy.
- Loss of application control keepalive is detected within eight seconds and does not immediately stop or
  blank Slave playback.
- A late or restarted Slave joins without pausing the Master or other Slaves.
- Slaves free-run through Master loss, close the stale connection, replace the network clock during
  reconnection, and perform at most one visible prepared correction when the Master returns.
- A configured startup delay uses one shared deadline and never waits beyond it for missing Slaves.
- Adding another Slave does not change the transport generation seen by existing Slaves.
- Routine authoritative snapshots do not seek or interrupt playback.
- After recovery, filmed numbered frames return to no more than one frame of separation.
- A healthy 60-minute run performs no drift-triggered correction.
- A stalled Master renderer is corrected locally without forcing healthy Slaves backward.

Operator-driven pause, resume and goto remain subsequent experiments. The current correction
thresholds are provisional until this procedure has been completed on physical hosts.
`ReleasePlan` distinguishes start, resume, and goto so later experiments can reuse the clock-domain
model without treating this lab protocol as stable. Automatic Master `CLOCK_LOST` recovery also
remains pending a focused physical-audio-sink experiment; the current implementation marks the
clock unhealthy and reports an explicit unrecovered fault.

## Results

Add dated result documents beside this file. Record raw measurements and failures; do not adjust the
acceptance criteria after observing results without explaining why.

