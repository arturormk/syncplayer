# 0003 - Keep media local behind a shared logical program

Status: Accepted
Date: 2026-09-16
Supersedes: None
Superseded by: None

## Context

The target installation renders preprocessed, display-specific videos on several hosts. Files can
contain different imagery while representing the same sequence of logical cues. Streaming pixels
from the Master would add bandwidth, decoding centralization, and a failure path that the use case
does not require. Authors also need cue-relative timing without maintaining cumulative timestamps.

## Decision

Every player decodes media from its own local storage. The synchronization network carries clocks,
logical state, control, and health information, never program media.

All hosts load the same logical program definition. The program defines ordered cue identities and
per-player mappings from those cues to local video, audio, settings, and events. Runtime protocol
state identifies a program and cue logically; it does not distribute media paths. Authored timing
is relative to a cue start or end rather than to a cumulative whole-program timeline.

Resolve the local player identity from an explicit CLI value first, otherwise from the hostname,
with optional configuration aliases. Relative media paths resolve from the program file's
directory.

## Consequences

Positive:

- Each host can render content prepared specifically for its output.
- Playback bandwidth is independent of media bitrate and host count.
- Cue-relative authoring isolates later cues from earlier duration changes.
- Installation bundles can remain portable through relative paths.

Negative / Trade-offs:

- Media distribution and consistency are deployment responsibilities.
- Every host requires sufficient local decode and storage capability.
- Local files must have compatible logical durations even when their imagery differs.

Follow-ups:

- Validate local media before playback and compare Slave durations with Master-announced durations.
- Keep content transfer, transcoding, and remote upload outside the initial system.

## Source blueprints

- `../BLUEPRINT.md`, sections 2.1, 2.4–2.5, 8–13, 74, 96, and 120
