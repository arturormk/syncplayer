# INC-0001 — Forced system clock stalls master playback after delayed startup

- Status: Resolved
- Date: 2026-10-12
- Affected component: Master media pipeline and audio-clock policy
- Severity: Development

## Context and trigger

The shared-clock prototype prerolled the Master's `playbin3` pipeline in `PAUSED`, forced a
`GstSystemClock` onto it, and exposed that clock through `GstNetTimeProvider`. The
`--startup-delay` option held the first frame before releasing playback at an absolute clock time.

The failure was reproduced with:

```console
RUST_LOG=syncplayer=debug \
  target/x86_64-unknown-linux-gnu/release/syncplayer master \
  --media ~/Vídeos/earth-HD.mp4 \
  --bind 192.168.1.21:46000 \
  --clock-port 46001 \
  --loop \
  --startup-delay 4
```

No Slave needed to be connected. This was significant: synchronization traffic and network-clock
calibration were not required to trigger the problem.

## Symptom

After the four-second initial hold, video and audio played smoothly for about one second. The image
then froze for several seconds while audio continued, after which normal synchronized playback
resumed.

The duration depended on `--startup-delay`:

| Startup delay | Result |
| ---: | --- |
| 0 seconds | Smooth playback |
| 1 second | Smooth playback |
| 2 seconds | A pause of about one second |
| 4 seconds | A longer pause, still beginning after about one second of smooth video |

Position telemetry captured the stall. After a timely release, the queried media position advanced
to approximately 1.52 seconds, remained fixed while the expected clock-derived position passed 2.11
and 3.12 seconds, and then caught up:

```text
release lateness:                         32,880 ns
expected=1,110,765,514 actual=1,110,757,351 error=     -8,163 ns
expected=2,112,975,026 actual=1,520,000,000 error=-592,975,026 ns
expected=3,115,375,770 actual=1,520,000,000 error=-1,595,375,770 ns
expected=4,118,093,511 actual=3,065,034,014 error=-1,053,059,497 ns
expected=5,119,591,166 actual=5,119,557,101 error=    -34,065 ns
```

The console emitted no GStreamer error during the frozen interval.

## Impact

Delayed startup was visibly broken on the Master even though its shared-clock release occurred on
time. The behavior also undermined confidence in the synchronization prototype because it resembled
a release-timing or video-clock defect while actually originating in physical audio-clock handling.

## Investigation

Several experiments narrowed the failure:

1. **Future pipeline base time was removed.** The pipeline was kept in `PAUSED`, and a one-shot
   clock wait changed it to `PLAYING` at the release deadline. Release lateness fell well below one
   millisecond, but the later freeze remained. This rejected the release callback and future base
   time as the primary cause.
2. **Preroll was refreshed one second before release.** A flushing accurate seek completed before
   the deadline, but the media position still froze around 1.52 seconds. This rejected stale decoder
   preroll as the cause; the extra seek was subsequently removed.
3. **Clock-derived expectation was compared with queried pipeline position.** The shared clock
   advanced continuously while the pipeline position plateaued. This showed that the scheduler and
   clock were healthy and that the media pipeline itself was being blocked.
4. **The audio sink was replaced with `fakesink`.** Video remained smooth with the same four-second
   delay. This isolated the failure to interaction with the physical audio sink rather than video
   decoding, rendering, media timestamps, or the release worker.
5. **Clock policy was compared with the design.** The Master was unnecessarily forced to follow a
   system clock even though it is the synchronization authority. GStreamer audio sinks have their
   own device-backed clocks; when another clock is forced on the pipeline, the sink must reconcile
   its internal audio clock with that external clock.

The timing pattern was consistent with the audio sink's default discontinuous `skew` correction:
the sink adjusted its playout position after the long paused interval, applying backpressure that
eventually stopped video. The exact internal correction sequence is inferred from the timing and
sink behavior; the decisive evidence is that bypassing the real audio sink removed the failure and
using the pipeline-selected clock corrected it.

## Root cause

The Master forced `GstSystemClock` onto a pipeline containing a physical audio sink. This inverted
the correct authority relationship: the Master should export the clock naturally driving its own
presentation, normally the audio-device clock, rather than make its audio hardware follow an
unrelated system clock.

Because audio and video share one pipeline, audio-sink clock correction and backpressure can stall
video even while the external system clock and scheduled release continue advancing normally.
Consequently, accurate release telemetry alone did not imply healthy presentation.

## Resolution

The playback clock policy and release mechanism were changed:

- The Master now lets the prerolled pipeline select its natural clock, pins that exact clock to the
  pipeline, and exposes it through `GstNetTimeProvider`.
- Slaves still use the Master's network-derived clock. Audio sinks in externally clocked pipelines
  are configured for continuous `resample` slaving instead of the default discontinuous `skew`
  adjustment.
- Pipelines remain `PAUSED` until a one-shot clock ID reaches the release timestamp, then transition
  to `PLAYING`. A future base time is no longer used as a waiting mechanism.
- The ineffective pre-release flushing seek was removed.
- Startup logs now identify the selected clock type and whether the pipeline is externally driven.

This restores the intended hierarchy:

```text
Master audio/pipeline clock -> network clock provider -> Slave network clocks
                                                   -> Slave audio resampling
```

## Verification

- The original real-sink reproduction with `--startup-delay 4` now plays smoothly.
- The diagnostic `--audio-sink fakesink` run was smooth and served as the isolation experiment, not
  as final presentation verification.
- Unit tests pass with `cargo +stable test`.
- The code passes `cargo +stable clippy --all-targets --all-features -- -D warnings`.
- The x86-64 release binary builds successfully.

The manual startup-delay matrix should continue to cover 0, 1, 2, and 4 seconds with a real audio
sink because the original defect appeared only once the paused interval crossed a threshold.

## Prevention and guardrails

- Do not force a system clock onto the Master by default. Export the exact clock assigned to its
  playback pipeline.
- Treat an audio sink as part of the synchronization design even when investigating a video freeze.
- Configure real audio sinks for continuous resampling whenever the pipeline must follow an external
  clock.
- Do not use `fakesink` success as evidence of correct physical presentation. Test real audio and
  video sinks across multiple startup delays.
- Log clock identity and external-clock policy at startup.
- Retain expected-versus-actual position telemetry; a growing error with an advancing clock
  distinguishes a stalled pipeline from a late release or stopped clock.
- Prefer one-shot clock scheduling followed by a state transition over encoding a waiting period as
  a future pipeline base time.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- Design: [`docs/BLUEPRINT.md`](../BLUEPRINT.md), sections 18–20 on Master clock selection
- Prototype procedure: [`docs/prototypes/clock-sync.md`](../prototypes/clock-sync.md)
- Related ADR: [`ADR 0010`](../adr/0010-schedule-discontinuous-transport-operations-against-the-shared-clock.md)
- Commit or pull request: not yet recorded

## Tags

- `audio`
- `clock`
- `gstreamer`
- `master`
- `startup-delay`
- `synchronization`
