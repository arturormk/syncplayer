# 0004 - Synchronize from a shared monotonic clock and state snapshots

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Repeatedly comparing positions and seeking Slaves introduces visible discontinuities and makes
network jitter part of frame scheduling. A participant joining late or recovering from packet loss
must also reconstruct the current timeline without replaying a command history. Wall-clock time is
unsuitable because it can jump and is not the clock driving media presentation.

## Decision

Drive steady-state synchronization from a monotonic clock derived from the clock used by the
Master's playback pipeline. Slaves discipline a local client clock to that Master clock and run
their pipelines against it.

Represent authoritative transport as complete, idempotent state snapshots containing enough
information to reconstruct the current cue position. At minimum, the model includes session and
generation identities, logical cue identity and occurrence, transport mode, a clock/position
anchor, cue duration, and active debug-loop state. During forward playback:

`desired_position = anchor_position + (synchronized_clock - anchor_clock)`.

Use seeks for initial joining, explicit discontinuities, and recovery from material error—not as
the normal correction loop. Sequence snapshots so stale, reordered, or duplicate packets can be
discarded. Change the session on Master restart and the generation when prior timeline assumptions
are invalidated.

## Consequences

Positive:

- Ordinary packet jitter does not directly schedule frames.
- Lost state packets are repaired by later snapshots.
- Late joiners can reconstruct state without command replay.
- Stale state is distinguishable from current authority.

Negative / Trade-offs:

- Clock export, calibration, and observability become foundational dependencies.
- Pipeline clocks and base times must be managed carefully across pause and seek.
- Correction thresholds require measurement on real systems.

Follow-ups:

- Validate actual GStreamer network-clock behavior on two physical hosts.
- Expose expected position, actual position, error, and clock-calibration diagnostics.

## Source blueprints

- `../BLUEPRINT.md`, sections 2.3, 18–23, 31–32, 86, and 91–93
