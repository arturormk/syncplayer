# 0012 - Let Slaves present the Master timeline with a fixed delay

Status: Proposed
Date: 2026-11-06
Supersedes: None
Superseded by: None

## Context

Some installations want a display to replay what another display showed a fixed time earlier, for
example a canon effect across a row of screens, or a second room that trails the first. Running
such a host as an independent player loses the shared clock, the authoritative cue progression,
and the operator controls. The delay must hold across cue boundaries and playlist wraps, not only
within one media file.

ADR 0004 defines forward playback as
`desired_position = anchor_position + (synchronized_clock - anchor_clock)`, and ADR 0005 makes
the Master the owner of logical cue progression. Positions exchanged between participants are
already expressed as an absolute sequence position spanning cues and playlist iterations.

## Decision

A Slave may be configured with a non-negative presentation delay `d`. A delayed Slave presents
`authoritative(t - d)`, so its desired forward position is
`anchor_position + (synchronized_clock - anchor_clock) - d`.

The delay is applied by the Slave to every authoritative anchor it accepts: release plans and
state snapshots. It is applied in absolute sequence space, so the delayed Slave crosses cue
boundaries and playlist wraps `d` after the Master through the same prepared transitions. The
Master, the protocol, and other participants are unaware of the delay, and different Slaves may use
different delays.

- A playing anchor whose delayed position would precede the sequence start holds the first frame
  and releases at the shared-clock instant at which the delayed position reaches zero.
- A paused anchor keeps the lag (saturating at the sequence start), so a later resume preserves
  it.
- When a non-looping program completes, the delayed Slave keeps following its current plan to its
  own end.
- Debug loops are not delayed; all participants repeat them in lockstep.
- Host-local events (ADR 0009), the OSD, and drift diagnostics follow the delayed timeline.

The delay is configured on the Slave command line or per player in the shared program file as
`[players.<id>] delay = <seconds>`. Resolution follows the program's other player settings: the
player's own value, then `[players.default]`, then zero; an explicit command-line value overrides
the program file. Program-file delays are covered by the program hash, which is unchanged for
programs that declare none, so participants still agree on a single program definition. `delay`
is therefore reserved and cannot be a cue ID. The Master ignores any delay resolved for its own
player.

Discontinuous operations (ADR 0010) still release at the Master-selected timestamp; only the
target position is delayed. A delayed Slave therefore does not replay the final `d` seconds before
an operator pause, stop, or goto.

## Consequences

Positive:

- Delayed displays keep the synchronization, recovery, and control properties of ordinary
  Slaves.
- No protocol change or Master configuration is required.
- Cue transitions and playlist loops need no special handling beyond the absolute sequence
  position.

Negative / Trade-offs:

- Operator discontinuities take effect immediately on a delayed Slave rather than `d` later.
- Joining a debug loop mid-loop places a delayed Slave at the delayed position until drift repair
  realigns it with the loop.
- Centralized pairwise separation includes the configured delay.

Follow-ups:

- Decide whether discontinuous operations should optionally be deferred by `d` on delayed Slaves.
- Consider reporting each Slave's delay in its subscription so Master diagnostics can subtract it.

## Source blueprints

- `../BLUEPRINT.md`, sections 18–23 and 31–33

Source citations record provenance only; they do not incorporate the cited document wholesale
into this decision.

When this decision is replaced, create and accept a new ADR that names this ADR in `Supersedes`.
Then change this record to `Status: Superseded` and set `Superseded by` to the new ADR's linked
name and acceptance date. Do not rewrite the historical decision.
