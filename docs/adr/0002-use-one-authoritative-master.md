# 0002 - Use one authoritative Master

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

The installation needs a single interpretation of program selection, cue progression, transport
operations, and synchronization state. Consensus, leader election, and automatic takeover would
add distributed-systems complexity and could create split-brain playback. Installations can
explicitly identify the machine connected to the primary audio system as the authority.

## Decision

Each running Syncplayer installation has exactly one configured Master. The Master is authoritative
for the active program, logical cue and occurrence, transport state, timeline discontinuities,
cue transitions, synchronized debug operations, and network session identity.

Slaves do not elect a Master, take over authority automatically, or advance the authoritative cue
because their local pipeline reaches EOS. Master discovery and automatic failover are outside this
decision; Slaves are configured with the Master's address.

The Master role is logical authority, not a media server. Its loss behavior is governed separately
by ADR 0007.

## Consequences

Positive:

- Every participant can resolve conflicting state deterministically.
- The protocol avoids consensus and split-brain handling.
- Installation operation and diagnosis remain understandable.

Negative / Trade-offs:

- There is no automatic authoritative failover.
- Operators must configure and maintain one known Master endpoint.
- A restarted Master creates a new authority session that Slaves must reconcile with.

Follow-ups:

- Give every Master process run a unique session identity.
- Reject incompatible program state and duplicate active player identities during subscription.

## Source blueprints

- `../BLUEPRINT.md`, sections 2.2, 14, 22, 28, 95, 112, and 114
