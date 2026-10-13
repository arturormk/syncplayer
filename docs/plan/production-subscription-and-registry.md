# Production subscription and participant registry

Status: Complete

## Delivered

- [x] Slave-first subscription with player/program identity, canonical hash, software/protocol
      version, capabilities, state endpoint, and per-cue durations.
- [x] Explicit acceptance or diagnostic rejection before network-clock calibration.
- [x] Rejection of program/hash/capability incompatibility and IDs colliding with the Master.
- [x] Tokenized active-participant registry with duplicate detection and stale expiration.
- [x] Readiness, clock synchronization, last-seen, remote address, endpoint, version, capability,
      and duration status suitable for the later console.
- [x] Configurable Master duration tolerance and warn/reject policy.

## Compatibility boundary

This increments the laboratory control protocol to version 5. Subscription remains on the bounded
TCP control channel; independent state transport and production binary encoding remain the later
state-protocol milestone. The subsequent authoritative-transport milestone increments the combined
protocol to version 6 without changing these subscription fields. The completed state-protocol
milestone subsequently moves the combined binary control/state protocol to version 7.
