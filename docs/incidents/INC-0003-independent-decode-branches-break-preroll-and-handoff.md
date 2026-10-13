# INC-0003 — Independent decode branches break preroll and cue handoff

- Status: Mitigated
- Date: 2026-10-13
- Affected component: Master and Slave local media pipelines
- Severity: Development

## Context and trigger

The first P1 playback implementation replaced `playbin3` with two independent `uridecodebin3`
branches, one for video and one for audio. This made external audio possible and let each branch
prepare its next URI through `about-to-finish`, but it also made two independent stream coordinators
responsible for one logical cue transition.

The issue was reproduced with `__videos/program.toml`, which only uses embedded audio. The laptop
Master played the first cue, while the Raspberry Pi Slave attempted to join the same program.

## Symptom

On the Master, both branches queued cue 2, but the pipeline reached EOS roughly 19 ms before the
authoritative cue boundary and remained held there:

```text
queueing prepared audio item next_item_index=1
queueing prepared video item next_item_index=1
Master reached EOS before its authoritative cue boundary; holding the cue remaining_ns=18587000
```

On the Raspberry Pi Slave, the media pipeline never completed its initial transition from `READY`
to `PAUSED`:

```text
Error: media did not preroll before timeout; current=Ready, pending=Paused
Caused by: Element failed to change its state
```

The audio branch nevertheless emitted `about-to-finish`, demonstrating that one branch could
advance while the overall pipeline was still waiting for preroll.

## Impact

The Master could stop permanently at the first cue boundary. A Slave could fail during startup
before joining the shared clock. Together these failures made the normal embedded-audio playlist
unusable across the target laptop and Raspberry Pi pair.

## Root cause

The application treated two independent `uridecodebin3` instances as though they formed one
gapless playback coordinator. Their preroll, `about-to-finish`, URI replacement, and EOS decisions
are independent, so there is no component responsible for atomically advancing the audio and video
streams as one presentation.

This architecture also duplicated coordination already supplied by `playbin3` for ordinary media.
The network transport was not responsible: the Master failed locally at EOS, and the Slave failed
while constructing its local pipeline after successful subscription and clock calibration.

## Resolution

- Programs whose cues all use their media's embedded audio now use one `playbin3` pipeline.
- A single `about-to-finish` callback advances the logical cursor and queues the next media URI.
- The P1 video-output controls are preserved by installing a custom video-sink bin containing the
  converter, scaler, optional cover crop, optional size caps, and selected physical sink.
- Programs containing an external-audio mapping retain the separate branch engine because
  `playbin3` has no external-audio URI input. That less-common path remains subject to dedicated
  hardware validation.

## Verification

- A real-time, 24-second headless Master run with the reported program queued cue 2, crossed the
  first boundary, remained `Playing`, and reported cue 2 with approximately 50 microseconds of
  position error.
- A loopback Slave using the Raspberry Pi mappings calibrated its network clock, completed preroll
  through the `playbin3` engine, prepared an initial join, and entered `Playing`.
- The laptop and Raspberry Pi still require repeated tests with their physical video and audio
  sinks, so the incident remains Mitigated rather than Resolved.

## Prevention and guardrails

- Use a single component to coordinate all streams belonging to one presentation whenever the
  framework provides one.
- Do not infer atomic cue handoff from independent `about-to-finish` callbacks.
- Exercise initial network-clock preroll as well as cue transitions in integration tests.
- Keep embedded-audio and external-audio pipeline modes explicit in logs and test both modes.
- Validate repeated transitions and playlist wrap on every supported GStreamer/hardware target.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- Design: [`docs/BLUEPRINT.md`](../BLUEPRINT.md), sections 14–19 and 85
- Related incident: [`INC-0002`](INC-0002-current-uri-race-panics-during-cue-handoff.md)
- Commit or pull request: not yet recorded

## Tags

- `gstreamer`
- `playbin3`
- `uridecodebin3`
- `playlist`
- `preroll`
- `eos`
- `transition`
