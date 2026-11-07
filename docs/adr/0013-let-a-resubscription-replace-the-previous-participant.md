# 0013 - Let a resubscription replace the previous participant

Status: Accepted
Date: 2026-11-07
Supersedes: None
Superseded by: None

## Context

ADR 0002 listed, as a follow-up, that the Master rejects duplicate active player identities during
subscription. The implementation kept each subscribed player ID in a tokenized registry until its
control connection ended or its keepalive deadline expired, and refused a new subscription for an
ID that was still registered.

A Slave that reboots, crashes, or loses its network cannot announce its departure. Its old control
connection stays open on the Master until the keepalive timeout or a TCP error closes it. During that
window the restarted Slave's subscriptions were rejected as duplicates, so a rebooted display could
not rejoin promptly, which defeats the late-join and reconnect behavior required by ADRs 0004 and 0007.

Player IDs come from `--player-id`, the program's `[aliases]` table, or the system hostname.
Two hosts that resolve to the same ID are a configuration error, and installations consider
detecting it automatically less valuable than prompt recovery from reboots.

## Decision

The most recent subscription for a player ID is authoritative. When the Master accepts a
subscription whose player ID already has a registered participant, it:

- removes the previous participant from the registry, so its lease expires and can no longer
  update or remove the new entry;
- shuts down the previous participant's control connection so its handler stops promptly instead of
  waiting for its keepalive deadline;
- logs a warning that names the previous and new remote addresses; and
- records the end of the previous connection as a `superseded` diagnostics event rather than
  `disconnected`.

Rejection of player IDs equal to the Master's own player ID, and of incompatible program, hash,
capability, or duration state, is unchanged. This decision replaces only the duplicate-identity
rejection named in ADR 0002's follow-ups; the authority model of ADR 0002 is unchanged.

## Consequences

Positive:

- A rebooted or reconnected Slave rejoins on its first successful subscription.
- No operator action or timeout tuning is needed to clear a stale participant.
- Diagnostics distinguish a replaced connection from an ordinary loss of contact.

Negative / Trade-offs:

- Two running hosts that resolve to the same player ID are no longer rejected. They repeatedly
  replace each other, which appears as resubscription warnings and `superseded` events rather than a
  clear rejection.
- Any host that knows a valid player ID and compatible program can displace that participant. The
  control protocol was already unauthenticated, so this does not weaken the trust model.

Follow-ups:

- If shared-ID misconfiguration becomes a practical problem, consider refusing replacement only
  while the previous connection is demonstrably live and originates from a different address.

## Source blueprints

- `../BLUEPRINT.md`, sections 28 and 29
