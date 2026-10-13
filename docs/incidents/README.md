# Incident Log

This directory records non-trivial Syncplayer failures whose diagnosis produces a reusable
engineering lesson. Incident reports complement ADRs: an ADR explains a design decision, while an
incident report preserves the evidence that exposed a faulty assumption and the guardrails added as
a result.

## Inclusion criteria

Write an incident report when one or more of the following apply:

- A playback, synchronization, clock, transport, or media-pipeline failure could plausibly recur.
- The failure only appears with particular hardware, sinks, media, timing, or network conditions.
- Diagnosis required several competing hypotheses or experiments.
- The result changes an architectural rule, test matrix, diagnostic signal, or operating procedure.

Routine defects with an obvious cause and no reusable lesson do not need an incident report.

## Format and lifecycle

- Name reports `INC-XXXX-kebab-case-title.md`, using the next unused four-digit ID.
- Start from [`TEMPLATE.md`](TEMPLATE.md) and keep concrete commands and timestamped evidence when
  they materially support the diagnosis.
- Use `Open` while diagnosis or remediation is incomplete, `Mitigated` when impact is contained but
  the underlying problem remains, and `Resolved` once the correction has been verified.
- Distinguish observed facts from inferred mechanisms. Record discarded hypotheses so future work
  does not repeat them.
- Verification of presentation behavior must state whether it used real audio/video sinks or test
  sinks such as `fakesink`.

## Reports

- [`INC-0001 — Forced system clock stalls master playback after delayed startup`](INC-0001-forced-system-clock-stalls-master-after-delayed-startup.md)
- [`INC-0002 — Current URI race panics during cue handoff`](INC-0002-current-uri-race-panics-during-cue-handoff.md)
- [`INC-0003 — Independent decode branches break preroll and cue handoff`](INC-0003-independent-decode-branches-break-preroll-and-handoff.md)
- [`INC-0004 — Shorter rendition disconnects at authoritative cue end`](INC-0004-shorter-rendition-disconnects-at-authoritative-cue-end.md)
