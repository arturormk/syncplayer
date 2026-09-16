# 0009 - Execute host-local events on timeline crossings

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Installations need cue-timed actions for equipment physically attached to particular hosts. A
simple `current_position >= event_time` test duplicates actions after packet repetition, jitter, or
correction and incorrectly fires events skipped by seek or late join. External processes can also
block unpredictably and must not interfere with decoding or frame presentation.

## Decision

Every timed event belongs to one explicitly configured player and one logical cue. There is no
implicit global event execution, reassignment, or failover. Author event time relative to cue start
or cue end and represent resolved time internally as integer nanoseconds.

Fire an event once when normal forward playback crosses its marker during an eligible cue
traversal. Do not fire merely because the current position is later than the marker. Explicit
seeks, initial synchronization, and resynchronization do not fire skipped events. A new playlist or
cue traversal makes its events eligible again. Track identity sufficient to deduplicate across
session, generation, playlist iteration, cue traversal, and event ID.

Suppress external events during diagnostic loops by default. Execute process actions asynchronously
through a low-priority worker; playback never waits for process completion or blocked output.
Prepare UDP resources ahead of timing-sensitive sends where practical.

## Consequences

Positive:

- Event behavior remains predictable across pause, seek, correction, and playlist loops.
- A failing or slow external program cannot block media threads.
- Physical ownership of external equipment is explicit.

Negative / Trade-offs:

- Traversal state and discontinuity classification are required.
- A failed event host has no automatic replacement.
- Process events cannot promise hard-real-time execution.

Follow-ups:

- Automate tests for crossings, pause, seek, join, correction, loops, and debug suppression.
- Log scheduling, dispatch, target, and asynchronous completion without per-frame noise.
- Preserve a path to future devices that accept an ahead-of-time Master-clock timestamp.

## Source blueprints

- `../BLUEPRINT.md`, sections 2.5–2.6, 35–43, 59, 72, 80–81, and 89
