# 0007 - Let Slaves free-run through Master outages

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

An unattended exhibition should not turn its displays black because of transient packet loss, a
switch interruption, or a Master restart. Slaves already have local media, the logical playlist,
and a disciplined clock estimate, so stopping immediately would discard useful autonomy. However,
locally continued state is provisional because ADR 0002 makes the Master authoritative.

## Decision

When Master state or connectivity disappears, Slaves continue playback from their last valid
synchronized state. They advance through locally known cues, loop according to the shared program,
and execute normal host-local events while repeatedly trying to reconnect to the configured Master.

Do not promote this locally continued timeline to authoritative state. On reconnection, compare the
Master session and generation, obtain a fresh snapshot, measure the error, and converge to the
Master. Continue naturally when sufficiently close; use a prepared seek/rejoin when a material
discontinuity or error requires it.

This policy applies to recoverable communication or Master outages. It does not require continued
playback after a fatal local media or pipeline failure.

## Consequences

Positive:

- Temporary infrastructure failures do not immediately blank the installation.
- Slaves require no media service from the Master while disconnected.
- Same-session packet loss and new-session restarts can be distinguished.

Negative / Trade-offs:

- Participants can diverge while disconnected.
- Host-local events may execute on a timeline the restarted Master later replaces.
- Rejoining may require a visible correction if the outage is long or clocks drift.

Follow-ups:

- Test temporary network isolation separately from Master process restart.
- Define measured correction thresholds and observable disconnected/rejoining states.
- Ensure reconnection cannot double-fire events crossed by an authoritative seek.

## Source blueprints

- `../BLUEPRINT.md`, sections 22, 30–32, 41–42, 70, and 88
