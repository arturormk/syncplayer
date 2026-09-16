# 0005 - Make cue progression authoritative and transitions prepared

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Locally encoded files can differ slightly in duration, and reacting to EOS only after it occurs
cannot provide deterministic gapless transitions. If every Slave advances independently, small
duration or decoder differences become logical playlist divergence. If a Slave waits for a packet
sent after Master EOS, it can miss the intended boundary.

## Decision

The Master owns logical cue completion and playlist advancement. A Slave's local EOS is diagnostic
information and never independently changes its logical cue.

All participants prepare the known next cue before the current cue ends. Associate the transition
with the authoritative logical cue boundary and synchronized clock so participants can switch
without relying on a last-moment network packet. Apply the same mechanism to ordinary A-to-B
transitions, single-item loops, and the final-to-first playlist transition.

The requirement is prepared, clock-aligned transition behavior. This ADR does not prescribe
`playbin3`, input selectors, multiple pipelines, or another specific GStreamer topology; experiments
will determine the mechanism and document any media-format limitations.

## Consequences

Positive:

- Players cannot diverge logically due to small local duration differences.
- Transition timing does not depend on post-boundary packet arrival.
- Playlist looping and ordinary transitions share one model.

Negative / Trade-offs:

- Preloading consumes additional resources and complicates pipeline lifecycle management.
- Local duration mismatches require clear diagnostics and policy.
- Some format changes may prevent perfectly seamless transitions on some platforms.

Follow-ups:

- Prototype and measure candidate GStreamer transition topologies.
- Generate test media that exposes dropped, repeated, or late boundary frames.
- Define experimentally justified duration tolerances and early-EOS visual behavior.

## Source blueprints

- `../BLUEPRINT.md`, sections 13–17, 85, 87, and 99–100
