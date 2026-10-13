# INC-0004 — Shorter rendition disconnects at authoritative cue end

- Status: Mitigated
- Date: 2026-10-13
- Affected component: Slave state-snapshot mapping
- Severity: Playback disruption

## Context and trigger

A laptop Master (`tweek`) and Raspberry Pi Slave (`player01`) played three host-local renditions in
a looping program. The configured 100 ms duration tolerance accepted the renditions. In the
reported transition, the Master's `starfield` cue was approximately 10.048 seconds and the Slave's
was approximately 10.034 seconds.

The failure occurred as the second cue advanced to the third cue on the second playlist pass.

## Symptom

The Slave closed its control connection near the authoritative cue boundary. The Master recorded
an unexpected TCP EOF, the Slave reconnected, and prepared resynchronization visibly froze the
image. Central telemetry placed the disconnect at an absolute media position within the last few
milliseconds of the Master's longer cue.

Telemetry also displayed an approximately ten-second error while `transition=true`. That value was
caused by the playlist cursor advancing in `about-to-finish` before the actual handoff and was
correctly excluded from drift repair. It was not the cause of the reconnect.

## Root cause

State snapshots contain the Master's authoritative cue-local position. The Slave used that value
unchanged when mapping the snapshot into its local timeline. During the interval after a shorter
local rendition's endpoint but before the Master's endpoint, `Timeline::sequence_position`
rejected the position as beyond the local cue duration. The resulting Slave control-monitor error
closed TCP and invoked the normal reconnect/resynchronization path.

The implementation therefore contradicted its duration-mismatch policy: it accepted a bounded
difference during subscription but could not represent that difference at runtime.

## Resolution

- Map an authoritative snapshot beyond the local rendition's endpoint to the last representable
  nanosecond of that local cue.
- Preserve the authoritative cue identity and wait for the Master to announce the next cue.
- Express actual diagnostic position in the playlist occurrence nearest the expected absolute
  position, making repeated-loop CSV values directly comparable.
- Retain transition observations in the raw CSV, but exclude them from aggregate error and rate
  statistics because the preloaded playlist cursor may not yet describe the rendered frame.
- Before closing a failed control session, send the Slave-side error to the Master when TCP remains
  writable. Future lifecycle records should therefore contain the originating error instead of
  only `UnexpectedEof`.

## Verification

- A regression test maps a remote cue position beyond a shorter local rendition and verifies that
  it holds at the local cue's last valid instant.
- A loop-occurrence test verifies that diagnostic actual position is selected near the expected
  absolute occurrence.
- All 58 unit tests pass.
- Formatting and Clippy with warnings denied pass.

Physical verification across repeated real-sink transitions is still required, so this incident
remains Mitigated rather than Resolved.

## Prevention and guardrails

- Every accepted duration mismatch must have defined behavior on both sides of the cue boundary.
- Never feed a remote cue-local timestamp into a local rendition without applying the negotiated
  duration policy.
- Preserve originating peer errors in centralized diagnostics before closing a healthy TCP path.
- Keep transition samples out of drift-repair hysteresis; cursor changes during preloading are not
  proof that rendered media has advanced. Exclude those samples from aggregate diagnostics for the
  same reason.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- Protocol: [`docs/state-protocol.md`](../state-protocol.md)
- Related ADR: [`ADR 0005`](../adr/0005-make-cue-progression-authoritative-and-transitions-prepared.md)

## Tags

- `duration-mismatch`
- `state-snapshot`
- `playlist`
- `reconnect`
- `telemetry`
