# 0006 - Separate clock control and state network traffic

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Clock synchronization, participant lifecycle, reliable operations, and frequent transport state
have different latency and delivery requirements. Putting all traffic on one reliable ordered
stream can make fresh state wait behind retransmission. Making everything lossy complicates
subscription and preparation. Requiring multicast or broadcast would exclude common museum LANs,
routed networks, and VPNs.

## Decision

Keep three network responsibilities logically separate:

1. clock synchronization;
2. subscription and reliable control;
3. periodic transport-state distribution.

Use explicit Slave subscription and keepalives even when state delivery is multicast or broadcast.
Use acknowledged/retried delivery or a reliable connection for operations whose receipt matters,
including subscription and explicit preparation. State updates are complete snapshots and may use
lossy datagrams because a later snapshot replaces a lost one.

Use unicast fan-out to registered Slaves as the default state-distribution mode. Multicast and
broadcast may be optional destination strategies for the same state payload, but the system must
not depend on either. Keep protocol messages versioned, bounded, and safely parseable.

This ADR specifies delivery semantics, not socket counts, exact transports, serialization library,
port numbers, packet frequency, or retry intervals.

## Consequences

Positive:

- State freshness is not coupled to reliable-stream head-of-line blocking.
- The default works on ordinary unicast IP networks.
- Subscriber awareness, health, and duplicate identity checks remain available in every mode.
- Optional distribution strategies do not require different state semantics.

Negative / Trade-offs:

- The application must coordinate multiple protocol responsibilities.
- Unicast sends one state packet per subscriber.
- Reliable operation and snapshot state can briefly report different phases and need clear models.

Follow-ups:

- Select and document concrete transports and encoding after prototyping.
- Define protocol compatibility, sequence handling, packet limits, and subscriber expiry.

## Source blueprints

- `../BLUEPRINT.md`, sections 24–29, 65–66, and 91–98
