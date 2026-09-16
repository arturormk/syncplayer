# 0010 - Schedule discontinuous transport operations against the shared clock

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Starting, resuming, or seeking each host as soon as its command arrives makes network latency and
local preparation time visible as synchronization error. Different hosts need time to load, seek,
and preroll. Waiting for a last `GO` packet still makes packet arrival the timing authority and can
produce a staggered release.

## Decision

Coordinate discontinuous transport operations using a release timestamp in the shared Master clock
domain. The Master selects a future instant and distributes the destination cue, cue-local position,
generation, and release timestamp. Each participant prepares and prerolls independently, then
releases against that timestamp rather than against command or confirmation arrival.

Use this mechanism for initial Slave joining, resume from pause, and goto while playing. A paused
goto seeks and remains paused. Stop is represented as paused at the first cue at time zero rather
than as a separate permanent transport mode. Debug-loop bounds are Master-defined cue-local times
and use the same synchronized transport model.

Readiness acknowledgements provide diagnostics and may inform a future barrier policy, but do not
replace the timestamp as timing authority. This ADR does not fix the preparation delay, readiness
timeout, or behavior when a participant cannot be ready in time.

## Consequences

Positive:

- Release alignment is independent of per-host command latency.
- Slow preparation is visible before the scheduled transition.
- Initial join and installation controls share one synchronization primitive.

Negative / Trade-offs:

- Operations incur a deliberate preparation delay.
- All participants need a sufficiently calibrated clock before release.
- Late or failed preparation requires an explicit observable policy.

Follow-ups:

- Prototype pause/seek/preroll and scheduled release with real GStreamer pipelines.
- Define default preparation delays and late-participant behavior from measurements.
- Keep diagnostic loops within one cue initially and suppress their events under ADR 0009.

## Source blueprints

- `../BLUEPRINT.md`, sections 31, 33, 52–60, 79, and 98
