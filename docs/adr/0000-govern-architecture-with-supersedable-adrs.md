# 0000 - Govern architecture with supersedable ADRs

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Syncplayer has a detailed blueprint, but implementation experiments—especially around GStreamer,
clocks, display systems, and gapless transitions—may reveal better designs. Treating the blueprint
as immutable would turn exploratory detail into accidental policy. Conversely, changing major
choices without preserving their rationale would make the system difficult to evolve safely.

## Decision

Use ADRs as the binding record of durable architectural decisions.

Blueprints, indexes, plans, examples, and other design documents are descriptive inputs. They do
not become prescriptive merely by being cited by an ADR. An Accepted ADR governs implementation
within its stated boundary; an Accepted ADR may deliberately depart from the blueprint.

Change an Accepted decision by accepting a new ADR that names the old one in `Supersedes`. Retain
the old record, mark it `Superseded`, and link it to the replacement. Do not rewrite historical
context or decisions, except for editorial corrections that do not change meaning.

Keep ADRs focused on choices with material, lasting effects. Feature inventories, tunable values,
temporary experiments, and implementation details that can change locally do not require ADRs.

## Consequences

Positive:

- Current architectural authority is explicit and reviewable.
- Experiments can overturn earlier assumptions without erasing history.
- Contributors can distinguish design context from binding policy.

Negative / Trade-offs:

- Accepted decisions require lifecycle maintenance when they change.
- The ADR index and supersession links can become stale if changes are incomplete.

Follow-ups:

- Keep the index in `docs/adr/README.md` synchronized with the records.
- Create a new ADR before intentionally departing from an Accepted decision.

## Source blueprints

- `../BLUEPRINT.md`, sections 1, 85, 107, and 114–120
