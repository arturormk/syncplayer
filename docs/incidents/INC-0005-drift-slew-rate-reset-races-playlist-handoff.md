# INC-0005 — Drift-slew rate reset races playlist handoff

- Status: Resolved
- Date: 2026-10-15
- Affected component: Slave media pipeline and drift correction
- Severity: Playback disruption

## Context and trigger

A Raspberry Pi Slave played a looping three-video program with embedded audio through the
`playbin3` engine. The Slave used `--drift-slew`, which corrects sustained 100–250 ms position
error by sending non-flushing GStreamer instant-rate-change seeks through the video sink.

The failure was timing-dependent. The same program ran without crashes when `--drift-slew` was
disabled, but with slewing enabled it eventually failed after approximately one to two hours of
repeated cue handoffs.

## Symptom

The Slave received an error from the `qtdemux` belonging to a newly queued play item:

```text
GStreamer error ... GstQTDemux:qtdemux218
Internal data stream error.
streaming stopped, reason error (-5)
Error: playback pipeline failed: Internal data stream error.
```

After Syncplayer returned the fatal pipeline error, process teardown ended with signal 11 and the
test launcher reported exit status 139. The failure was observed with the Raspberry Pi's real
playback pipeline, not with a test sink.

## Impact

The Slave process terminated and stopped presenting its display until restarted by an operator or
service supervisor. The long and variable reproduction time made short transition tests appear
healthy and reduced confidence in unattended installations using drift slewing.

## Investigation

1. Repeated playback without `--drift-slew` no longer crashed, isolating the remaining failure to
   rate control rather than ordinary decoding or gapless playlist playback.
2. Instant rate changes are sent as seek events from a persistent sink. For `playbin3`, the event
   travels upstream to the shared demuxer and then back downstream to all streams.
3. `about-to-finish` runs on a GStreamer streaming thread and setting the next URI begins preparing
   and later replacing the current play item. GStreamer seek handling is asynchronous, so accepting
   a seek does not prove that all related processing completed before a concurrent handoff.
4. Syncplayer already suppressed drift samples when the playlist cursor advanced ahead of the
   authoritative cue. However, suppression of an active slew immediately sent a second instant-rate
   seek at 1.0x. That restoration command was therefore deliberately issued during the interval in
   which the old demuxer was being retired and the new demuxer was being installed.
5. The failing element number matched the continuing sequence of demuxers constructed for queued
   loop items. There was no corresponding failure when rate changes were absent.

The exact native path from the GStreamer stream error to the final segmentation fault was not
captured with a debugger. The rate-reset/handoff race is established by the application control
flow, the affected demuxer, the option-dependent reproduction, and the successful endurance test
after removing that overlap.

## Root Cause

Syncplayer treated drift-slew suppression as permission to restore normal speed immediately. At a
playlist boundary, “suppressed” actually meant that the media graph was unstable. Sending the 1.0x
instant-rate seek from that path could target a `qtdemux` while `playbin3` was tearing down one play
item and preparing the next.

The transition predicate also described only the ambiguous boundary sample. It did not explicitly
cover the full period from `about-to-finish`, when the next URI is queued, until telemetry confirms
that the replacement item is stable.

## Resolution

- Mark a handoff in progress before an `about-to-finish` callback changes the queued URI.
- Keep rate-control samples suppressed from that point until the expected cue, playlist cursor,
  pipeline state, and pending state all show a stable replacement item.
- For external-audio programs, require both video and audio cursors to reach the expected item.
- When an active slew is suppressed, clear its sampling state but retain a pending normal-rate
  reset instead of sending the reset during the handoff.
- Apply the deferred 1.0x reset on the first subsequent safe observation, before collecting new
  samples for another slew.

This preserves gradual drift correction while ensuring the correction path does not intentionally
send a rate command into a queued or crossing playlist transition.

## Verification

- A controller regression test verifies that suppressing an active slew produces no immediate
  reset and that the next safe observation requests the deferred 1.0x reset.
- All 65 unit tests pass.
- Formatting, Clippy with warnings denied, and `git diff --check` pass.
- The release binary was rebuilt before deployment.
- On 2026-10-15, the Raspberry Pi Slave ran the same looping three-video workload with
  `--drift-slew` and its real playback sinks for nearly six hours without a pipeline error or
  process crash. The previous failure normally appeared after one to two hours.
- On 2026-10-16/17, three Raspberry Pi Slaves used `--drift-slew` for a 17.62-hour observed,
  645-iteration four-Pi soak; copied logs cover 17.16 hours and 629 iterations. After successful
  startup, none reported a pipeline error, process exit, corrective recovery, or steady-state
  disconnect. This substantially exceeds the original one-to-two-hour
  reproduction window. An unexplained SIGSEGV on player04's first launch occurred before the
  successful soak process and left no diagnostic evidence tying it to this handoff race. See the
  [`soak report`](../reports/2026-10-17-four-raspberry-pi-soak-test.md).

## Prevention and guardrails

- Treat `about-to-finish` through confirmed new-item stability as one transition phase for any
  operation that sends seek or segment events through the media graph.
- Suppression must be side-effect-free while the graph is unstable; defer restorative commands
  until a stable sample rather than assuming that “return to normal” is harmless.
- Exercise optional control features during multi-hour, multi-item loop tests. Testing the base
  playback path does not validate a feature that injects additional GStreamer events.
- Record whether endurance tests use real sinks, because gapless and rate-control behavior can
  differ from `fakesink` runs.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- User documentation: [`README.md`](../../README.md)
- Related incidents: [`INC-0002`](INC-0002-current-uri-race-panics-during-cue-handoff.md),
  [`INC-0003`](INC-0003-independent-decode-branches-break-preroll-and-handoff.md)
- GStreamer: [`playbin3`](https://gstreamer.freedesktop.org/documentation/playback/playbin3.html),
  [seeking design](https://gstreamer.freedesktop.org/documentation/additional/design/seeking.html)
- Fix: commit `4377057f0dfb73b3efaa6e52407d10fe765e982a`

## Tags

- `gstreamer`
- `drift-slew`
- `instant-rate-change`
- `playbin3`
- `qtdemux`
- `playlist`
- `race-condition`
- `raspberry-pi`
