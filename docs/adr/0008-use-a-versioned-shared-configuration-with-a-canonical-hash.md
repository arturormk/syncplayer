# 0008 - Use a versioned shared configuration with a canonical hash

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

Every host must agree on playlist structure while resolving different media and event mappings.
An outdated configuration on one machine can remain superficially playable yet produce incorrect
cue transitions or physical actions. Hashing raw text would reject harmless formatting differences,
while hashing only the current player's subsection would miss changes to the installation as a
whole. Configuration semantics also need an explicit evolution boundary.

## Decision

Use a human-authored, explicitly versioned TOML program file that contains the logical program,
aliases, all player mappings, and all player events. Parse it into a strongly typed model with enums
for semantic variants such as event timing and action type.

Compute a deterministic canonical hash from the complete parsed semantic program, not from its raw
bytes and not only from the local player's mapping. Exchange that hash during subscription and
reject mismatches by default. Any override must be explicit and prominently diagnosed.

Canonicalization must define stable ordering and representations for every hashed value. The
schema version is part of compatibility handling; unknown incompatible versions fail clearly rather
than being guessed from available fields.

## Consequences

Positive:

- Formatting and comments do not create false mismatches.
- Changes affecting any host are detected across the whole installation.
- Typed parsing contains validation and avoids stringly typed runtime behavior.
- Schema evolution has an explicit compatibility signal.

Negative / Trade-offs:

- Canonicalization itself becomes a compatibility contract requiring precise tests.
- Whole-program hashes reject a host even when a changed section does not affect that host.
- TOML and the typed schema must evolve together.

Follow-ups:

- Specify canonical serialization before treating hashes as interoperable protocol values.
- Test semantic equivalence, ordering, and changes to mappings and events.
- Keep live reload outside V1; a later reload is an explicit new program/generation operation.

## Source blueprints

- `../BLUEPRINT.md`, sections 2.4, 9–12, 28, 34, 75–77, 90, and 111
