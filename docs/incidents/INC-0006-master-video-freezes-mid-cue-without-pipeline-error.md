# INC-0006 — Master video freezes mid-cue without pipeline error

- Status: Mitigated
- Date: 2026-10-15
- Affected component: Master media pipeline and video output
- Severity: Installation

## Context and trigger

The Master and Raspberry Pi Slave played a looping three-video program through their real playback
sinks for an all-day, public-facing projection. After approximately eight hours, the Master's image
froze during ordinary playback. Syncplayer logging was set to `WARN` at the time.

The visible frame showed the number 3 in a ten-second countdown. The countdown had progressed for
approximately seven seconds and had about three seconds remaining, placing the freeze within the
cue rather than near the transition to another video.

The Raspberry Pi Slave did not freeze and did not need to be restarted. It continued operating
through the Master interruption and recovery. This also provides further real-system evidence that
the separate `--drift-slew` handoff failure recorded in INC-0005 remains resolved.

## Symptom

- The Master's displayed video stopped changing and remained on one countdown frame.
- The process remained alive instead of reporting a fatal pipeline error or exiting.
- The `WARN`-level application log contained nothing unusual before or during the freeze.
- No GStreamer bus error, clock-lost message, EOS diagnosis, or other decisive cause was captured.
- The freeze was noticed visually after several hours; its exact onset time and the state of audio
  playback were not recorded.

The next endurance run uses `DEBUG` logging to retain per-second position telemetry and cue-handoff
messages if the failure recurs.

## Impact

A frozen projection can remain visible for the rest of an unattended day because the process is
still alive and therefore looks healthy to an ordinary service supervisor. For this installation,
a brief restart glitch is much less harmful than leaving a static frame on a public display for
hours.

The failure also stops the Master process from being a trustworthy presentation-health signal even
if its network control and clock-provider threads remain responsive.

## Investigation

The current evidence establishes the following:

1. The failure occurred well inside the countdown cue, not at a known playlist boundary.
2. It affected the Master while the Slave continued, so it was not a simultaneous failure of the
   shared program or both hosts' media files.
3. Syncplayer received no fatal GStreamer bus message at `WARN` level.
4. The process did not terminate on its own.
5. The available log does not show whether the pipeline position, selected playback clock, audio
   stream, decoder, video sink, compositor, or physical display output continued advancing.

These facts do not identify whether the stall began in decoding, scheduling, a sink or device
clock, the graphics stack, or the display compositor. The mid-cue location makes the previously
resolved drift-slew/playlist-handoff race an implausible explanation: drift slewing is Slave-only,
and no handoff was in progress on the frozen Master.

## Root Cause

Unknown. There is not yet enough diagnostic evidence to distinguish a stalled GStreamer streaming
thread from a decoder, video-sink, clock, graphics-driver, compositor, or hardware-output failure.

The absence of a warning is not evidence that GStreamer remained healthy; it only shows that the
failure did not produce one of the bus messages or application warnings monitored at that logging
level. This report must be updated if a future reproduction supplies enough evidence to identify
the failing layer.

## Mitigation

Syncplayer now installs an independent playback watchdog on both Master and Slave pipelines:

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

This is operational containment, not a root-cause fix. It limits a future frozen image to roughly
ten seconds plus supervisor restart time, but it does not prevent the underlying stall.

## Verification

- A deterministic controller test verifies that new frame counts refresh the deadline, repeated
  counts trigger the ten-second stall condition, and disarming suppresses detection.
- All 66 unit tests pass.
- Formatting, Clippy with warnings denied, and `git diff --check` pass.
- The release binary builds successfully.
- The watchdog has not yet been validated against a reproduction of the original eight-hour
  Master freeze. Real-sink endurance testing remains required.

## Evidence needed for resolution

If the freeze recurs, preserve and correlate:

- the watchdog's final error record, especially last frame PTS and queried media position;
- Syncplayer `DEBUG` logs covering at least the last several telemetry samples;
- the complete GStreamer log from process start through watchdog exit;
- the Master service journal and supervisor exit/restart record;
- kernel, DRM/KMS, GPU, display-server, and audio-device messages near the failure time;
- whether audio, OSD time, remote control, state publication, and the Slave continued advancing;
- GStreamer, kernel, graphics-driver, sink, decoder, and hardware versions;
- if practical, native thread backtraces captured while the Master is frozen and before it is
  terminated.

Once this evidence identifies the failed layer, update this report's investigation, root cause,
corrective change, verification matrix, and status. Mark it Resolved only after the underlying fix,
not merely the watchdog, survives representative real-sink endurance testing.

## Prevention and guardrails

- Supervise Master and Slave processes and restart them on nonzero exit, including status 70.
- Treat continued process existence as insufficient proof of presentation health.
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
