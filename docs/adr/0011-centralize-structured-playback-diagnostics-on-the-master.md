# 0011 - Centralize structured playback diagnostics on the Master

Status: Accepted
Date: 2026-10-13
Supersedes: None
Superseded by: None

## Context

Physical synchronization work needs comparable evidence from every player: loss of contact,
recovery, expected and actual position, transient error, and persistent playback-rate error. Host
logs alone have unrelated timestamps and are awkward to correlate. Sending human console text or
formatted log lines between players would couple measurement to presentation and make later
analysis fragile.

The installation console is a natural place for an operator to request a summary, but it is not a
suitable telemetry transport. Syncplayer already has a bounded reliable control session for every
subscribed Slave and a shared clock domain in which observations can be timestamped.

## Decision

Each player periodically samples expected and actual sequence position in the Master clock domain.
Slaves send a bounded structured telemetry message on their existing control session. The Master
records its own equivalent samples, aggregates every player's current/minimum/maximum/RMS error,
health and recovery counters, and derives playback-rate error from error slope in parts per
million.

Generation changes, disconnections, recoveries, transitions, and unhealthy-clock periods reset the
rate-estimation window. Telemetry is observational: it never changes transport state and loss of a
sample never interrupts playback.

Expose summaries through the Master installation console and optionally append every sample to a
flushed CSV log. Keep console commands textual and Master-local; Slaves receive only structured
transport state and telemetry protocol messages.

## Consequences

Positive:

- Physical tests produce one correlated data set without clock-aligning separate host logs.
- Persistent rate errors such as 0.1% slow playback are directly measurable.
- Console presentation and offline analysis share the same structured measurements.
- No additional listening socket or discovery mechanism is required on Slaves.

Negative / Trade-offs:

- The control protocol version changes when the telemetry schema changes.
- One-second samples can reveal sustained error and sampled stalls but cannot prove frame-level
  smoothness; sustained direct visual inspection and sink-level dropped/late-frame metrics remain
  necessary. High-speed filming is an optional stricter measurement, not a v1.0 release gate.
- CSV persistence performs a small flushed write per sample when explicitly enabled.

## Source blueprints

- `../BLUEPRINT.md`, sections 47, 50, 63, 71, 86, and 99.
- `../TODO.md`, P1 installation controls and P2 physical synchronization validation.
