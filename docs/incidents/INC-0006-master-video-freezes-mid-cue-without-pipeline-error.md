# INC-0006 — Master video freezes mid-cue without pipeline error

- Status: Mitigated
- Date: 2026-10-15
- Affected component: Master media pipeline and video output
- Severity: Installation

## Context and trigger

The Master and Raspberry Pi Slave played a looping three-video program through their real playback
sinks for an all-day, public-facing projection. After approximately eight hours, the Master's image
froze during ordinary playback. Syncplayer logging was set to `WARN` at the time, so the first
occurrence did not identify the failing layer.

The visible frame showed the number 3 in a ten-second countdown. The countdown had progressed for
approximately seven seconds and had about three seconds remaining, placing the freeze within the
cue rather than near the transition to another video.

A second run on 2026-10-15 reproduced the same visible countdown frame with DEBUG application logs,
centralized diagnostics, GStreamer logs, and resource sampling. This time the Slave also stopped
because the Master's exported clock stopped. Its independent playback watchdog exited with status
70 after ten seconds without a video frame.

## Symptom

- The Master's displayed video stopped changing and remained on one countdown frame.
- The Master process remained alive without a GStreamer bus error or `CLOCK_LOST` message.
- Its pipeline stayed in `Paused` with a synchronization transition permanently in progress.
- Its selected `GstPulseSinkClock`, expected position, and actual position all stopped changing.
- The Slave's network-derived clock and media position then stopped, causing the watchdog exit.
- Master CPU fell to approximately 0-2% and RSS remained stable around 1.17 GiB, excluding CPU,
  memory, and I/O pressure as the immediate cause.

## Impact

A frozen projection can remain visible for the rest of an unattended day because the process is
still alive and therefore looks healthy to an ordinary service supervisor. For this installation,
a brief restart glitch is much less harmful than leaving a static frame on a public display for
hours.

The failure also stops the Master process from being a trustworthy presentation-health signal even
if its network control and clock-provider threads remain responsive.

## Investigation

The second run established a deterministic sequence:

1. Master position error increased by almost exactly 1 ms per playlist iteration: approximately
   221 ms at iteration 221 through 250 ms at iteration 250. The media metadata also contains a
   one-millisecond container/longest-stream endpoint difference, consistent with this slope.
2. At 21:17:37.516, three samples exceeded the 250 ms repair threshold and triggered local drift
   recovery while the countdown cue was at 4.191 seconds.
3. Recovery selected a position five seconds ahead (`8.941504 s`) and a shared-clock release at
   `24579693048000 ns`, then reset the pipeline to `NULL` and prerolled it in `PAUSED`.
4. The reset destroyed the PulseAudio sink supplying the exported `GstPulseSinkClock`. That clock
   stopped permanently at `24574693533000 ns`, about five seconds before its own release deadline.
5. The release worker waited on that stopped clock, so it could never return the pipeline to
   `PLAYING`. The watchdog was intentionally disarmed during the scheduled transition.
6. The exported clock stopped the Slave as well. Its last queried position was 4.866 seconds; its
   watchdog then reported no frames for ten seconds and exited with status 70.

The repeated visible frame is therefore expected rather than coincidental: the same accumulated
error crosses the same threshold in the same playlist iteration, and recovery prerolls the same
future part of the countdown before waiting forever.

## Root Cause

Local drift recovery used the same clock both as the wait condition for a future release and as a
resource owned by the pipeline being reset. On a Master using `GstPulseSinkClock`, moving the
pipeline to `NULL` destroyed the clock provider. The clock could no longer reach the timestamp that
would move the replacement pipeline from `PAUSED` to `PLAYING`, creating a circular wait.

The deterministic trigger was ordinary timeline error accumulating by roughly one millisecond per
playlist iteration until it crossed the hard-repair threshold. This was not a decoder, graphics,
resource-exhaustion, or watchdog failure.

## Mitigation

Syncplayer retains the independent playback watchdog on both Master and Slave pipelines and adds
three recovery safeguards:

- The Master uses bounded drift slewing by default, correcting sustained 100-250 ms error without
  stopping the pipeline or changing the exported clock.
- A hard Master correction performs an accurate flushing seek within the current cue. The Slave
  retains the reset, preroll, and future shared-clock release because its network clock is external
  to its media pipeline.
- Every scheduled release has an independent monotonic deadline. A stopped shared clock therefore
  produces a fatal error and supervisor restart instead of an unlimited wait.

The existing playback watchdog behavior remains:

- A pad probe counts decoded video buffers after the final diagnostic overlay and immediately
  before the selected video sink.
- The watchdog is armed only when both logical transport and the GStreamer pipeline are stably in
  the Playing state.
- It is disarmed while stopped, paused, preparing, seeking, recovering, or waiting for a scheduled
  release, preventing intentional inactivity from being treated as a stall.
- If no new video buffer crosses the output path for ten seconds, the watchdog emits an error with
  the player role, elapsed stall time, last frame count, last frame PTS, and last queried media
  position.
- It then force-exits with status 70. The immediate exit deliberately avoids normal GStreamer
  teardown, which may itself block when the pipeline is wedged, and allows the service supervisor
  to restart the player.

The incident remains Mitigated until the corrective behavior survives a representative real-sink
endurance run. It may then be marked Resolved.

## Verification

- Unit tests cover watchdog liveness, role-specific drift recovery, Master-default slewing, and the
  monotonic scheduled-release deadline.
- Formatting, all 70 unit tests, Clippy with warnings denied, `git diff --check`, and a release build
  pass for this change.
- A real PulseAudio Master and network-clock Slave must run for at least 300 playlist iterations.
  Master error must converge below 40 ms without a destructive reset, both clocks must remain
  monotonic, and neither watchdog may exit.
- A forced hard Master error above 250 ms must recover through an in-place seek while the Slave
  continues rendering.

## Prevention and guardrails

- Supervise Master and Slave processes and restart them on nonzero exit, including status 70.
- Treat continued process existence as insufficient proof of presentation health.
- Never schedule a Master recovery against a future timestamp on a clock provider destroyed by
  that recovery.
- Keep scheduled-transition liveness deadlines in an independent monotonic clock domain.
- Keep actual frame-flow liveness independent from clock-derived position telemetry; a queried
  position may advance even when the visible output is frozen.
- Run optional control features and real physical sinks in multi-hour loop tests before unattended
  deployment.
- Preserve high-detail logs and system diagnostics from the first recurrence rather than restarting
  before collecting evidence when operationally safe.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- User documentation: [`README.md`](../../README.md)
- Related incident: [`INC-0005`](INC-0005-drift-slew-rate-reset-races-playlist-handoff.md)
- Commit or pull request: not yet recorded

## Tags

- `gstreamer`
- `watchdog`
- `video-freeze`
- `master`
- `mid-cue`
- `real-sink`
- `raspberry-pi`
- `availability`
