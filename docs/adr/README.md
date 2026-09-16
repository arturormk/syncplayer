# Syncplayer Architecture Decision Records

This directory contains the binding Architecture Decision Records (ADRs) for Syncplayer.

Documents such as `docs/BLUEPRINT.md` are descriptive design input. They explain the intended
system and provide context, but they are not prescriptive. Accepted ADRs record the current
decisions that govern implementation. If descriptive documentation conflicts with an Accepted
ADR, the ADR takes precedence until a later ADR supersedes it.

## Index

<!-- ADR-INDEX:BEGIN -->
- [0000 - Govern architecture with supersedable ADRs](0000-govern-architecture-with-supersedable-adrs.md) (Accepted, 2026-09-16)
- [0001 - Build the playback engine with Rust and GStreamer on Linux](0001-build-the-playback-engine-with-rust-and-gstreamer-on-linux.md) (Accepted, 2026-09-16)
- [0002 - Use one authoritative Master](0002-use-one-authoritative-master.md) (Accepted, 2026-09-16)
- [0003 - Keep media local behind a shared logical program](0003-keep-media-local-behind-a-shared-logical-program.md) (Accepted, 2026-09-16)
- [0004 - Synchronize from a shared monotonic clock and state snapshots](0004-synchronize-from-a-shared-monotonic-clock-and-state-snapshots.md) (Accepted, 2026-09-16)
- [0005 - Make cue progression authoritative and transitions prepared](0005-make-cue-progression-authoritative-and-transitions-prepared.md) (Accepted, 2026-09-16)
- [0006 - Separate clock control and state network traffic](0006-separate-clock-control-and-state-network-traffic.md) (Accepted, 2026-09-16)
- [0007 - Let Slaves free-run through Master outages](0007-let-slaves-free-run-through-master-outages.md) (Accepted, 2026-09-16)
- [0008 - Use a versioned shared configuration with a canonical hash](0008-use-a-versioned-shared-configuration-with-a-canonical-hash.md) (Accepted, 2026-09-16)
- [0009 - Execute host-local events on timeline crossings](0009-execute-host-local-events-on-timeline-crossings.md) (Accepted, 2026-09-16)
- [0010 - Schedule discontinuous transport operations against the shared clock](0010-schedule-discontinuous-transport-operations-against-the-shared-clock.md) (Accepted, 2026-09-16)
<!-- ADR-INDEX:END -->

## Authority and lifecycle

- `Proposed` records are review candidates and are not binding.
- `Accepted` records are binding for implementation.
- `Superseded` records are retained as historical context but are no longer binding.
- Descriptive documents are evidence and context, not an additional source of binding decisions.
- A decision changes through a new ADR, not by rewriting the decision in an Accepted ADR.
- A superseding ADR names and dates every ADR it replaces. Each replaced ADR is updated only to
  set its status and `Superseded by` metadata.
- Editorial corrections that do not alter a decision may be made in place.

## Conventions

- Filename: `NNNN-kebab-title.md`.
- Required metadata: `Status`, `Date`, `Supersedes`, and `Superseded by`.
- Required sections: `Context`, `Decision`, `Consequences`, and `Source blueprints`.
- Use `None` when a supersession field is not applicable.
- A populated supersession field uses `[NNNN - Title](filename.md) (YYYY-MM-DD)`, where the date
  is that ADR's acceptance date.
- Dates use ISO 8601 (`YYYY-MM-DD`).
- Source citations explain where a decision came from; they do not make every statement in the
  cited source binding.

## Rationale

ADRs preserve context behind technical decisions, enabling future contributors (human or
AI-assisted) to understand the *why*, distinguish current policy from exploratory design, and
evolve the architecture safely.

See [TEMPLATE.md](TEMPLATE.md) and
[ADR 0000](0000-govern-architecture-with-supersedable-adrs.md) for authoring and governance rules.
