The repository has a solid synchronization prototype, but it is not yet the complete player described by the blueprint. The clock/recovery work is comparatively advanced; configuration, authoritative transport, events, and installation controls are the main missing layers.

I treated explicitly optional/future items and the V1 exclusions in BLUEPRINT.md (docs/BLUEPRINT.md:3061) as non-blocking.

## Remaining work, in priority order

### P0 — Full shared program configuration

Status: implemented.

Replace the current host-local list of video paths with the blueprint’s logical program model.

Implement:

- [x] Versioned ProgramConfig with program ID, loop policy, ordered cue IDs, aliases, players, defaults, and strongly typed per-cue settings.
- [x] Player identity resolution: --player-id, hostname fallback, then aliases.
- [x] Player-specific mapping with players.default fallback.
- [x] Cue-local video, external audio, mute, gain, and event definitions.
- [x] Relative path resolution against the program file.
- [x] Canonical serialization and whole-program hash.
- [x] A syncplayer validate show.toml command covering structure, mappings, files, durations, events, audio conflicts, and transition warnings.
- [x] Example program files and migration/removal of the prototype playlist schema.

Why first: nearly every remaining subsystem needs logical cue IDs, player IDs, program hashes, and typed events. The current model contains only looping and videos (playlist.rs (src/playlist.rs:9)); the blueprint specifies the richer model in sections 8–13 and 75–77 (BLUEPRINT.md (docs/BLUEPRINT.md:371)).

### P0 — Production subscription and participant registry

Once the program model exists, replace the laboratory welcome exchange with a real subscription handshake.

Implement:

- Subscription containing player ID, program ID/hash, protocol/software version, capabilities, and state endpoint.
- Rejection of incompatible program hashes and protocol versions.
- Duplicate active player-ID detection.
- Per-cue duration exchange and configurable mismatch policy.
- A Master subscriber registry with last-seen, readiness, clock status, and remote address.
- Stale-subscriber expiration.
- Status information suitable for the later installation console.

Currently a Slave effectively sends only READY; no identity or program information is exchanged (main.rs (src/main.rs:290)). The protocol is explicitly identified as a small clock-lab protocol (protocol.rs (src/protocol.rs:1)).

### P0 — Authoritative transport and cue progression

Introduce a real Master transport controller rather than a single immutable startup plan.

Implement:

- Mutable runtime state containing program/cue identity, cue occurrence, local cue position, duration, paused/playing state, debug loop, session, and generation.
- Generation increments for goto, stop, pause/release discontinuities, loop changes, and program loads.
- Master-owned cue completion and transition decisions.
- Slaves holding or diagnosing early local EOS rather than advancing independently.
- Prepared, exact-clock cue transitions and playlist wrap.
- Defined behavior for local duration differences and early/late EOS.
- Recovery when the Master pipeline reports CLOCK_LOST.

At present generation is fixed at 1 (main.rs (src/main.rs:161)), TransportState has only Waiting and Playing (protocol.rs (src/protocol.rs:80)), and every host queues its next URI independently through playbin3. Logical cue mapping exists, but full Master authority at boundaries does not.

### P0 — Turn the networking prototype into the specified state protocol

Preserve the working shared-clock/reconnection mechanism, but separate its responsibilities.

Implement:

- Reliable control channel for subscription, preparation, readiness, and discontinuous operations.
- Independent complete state snapshots, normally unicast, at an experimentally selected rate around the blueprint’s suggested 5–20 Hz.
- Packet ordering and stale/duplicate rejection within a generation.
- Bounded, strongly specified production encoding rather than newline text.
- Configurable control/state ports and keepalive intervals.
- Continue free-running when only state/control traffic is lost.

Currently complete snapshots are carried in acknowledged TCP heartbeats every two seconds (main.rs (src/main.rs:357)). That proves the state model but does not yet meet the blueprint/ADR separation between reliable control and freshness-oriented state traffic.

### P1 — Complete the local playback/output feature set

Fill the remaining Phase 1 playback gaps:

- --gain-db, including positive-gain warning.
- --fullscreen, width/height, display selection, and contain/cover/stretch.
- --clock auto|audio|system.
- --hwdec auto|off and logging of the selected decoder/sinks.
- External audio files synchronized with cue video.
- Duration validation between external video and audio.
- Probe and validate single-file media as thoroughly as playlists.
- Decide whether playbin3 is sufficient or move to prepared pipelines/branches for deterministic switching.

The current CLI exposes media/playlist, mute, looping, and sink overrides only (main.rs (src/main.rs:84)).

### P1 — Host-local timed events

Implement the entire Phase 6 event subsystem:

- at and before_end parsing into integer nanoseconds.
- Typed process and UDP actions, including textual and hexadecimal payloads.
- Timeline-crossing scheduler.
- Traversal identity across cue and playlist iterations.
- No firing on seek, initial join, or resynchronization.
- Re-eligibility on the next normal traversal.
- Duplicate suppression around jitter/corrections.
- Low-priority asynchronous process executor with safe output handling and child reaping.
- Prepared/reused UDP sockets.
- Structured firing, completion, and failure logs.
- Default suppression during debug loops.

No event configuration or runtime scheduler currently exists. This is the blueprint’s Phase 6 (BLUEPRINT.md (docs/BLUEPRINT.md:3253)).

### P1 — Installation transport controls and console

Build the transport operations on top of the new controller, then expose them through an opt-in TCP console:

- status, pause, synchronized play [delay], stop, and goto.
- Human-friendly time parsing.
- In-cue diagnostic loop SECONDS and unloop.
- Ready/preparing status and late-participant policy.
- Optional OSD with player, role, program, cue, time, and connection state.
- Safe bind defaults; console disabled unless explicitly enabled.
- Console text translated into structured operations rather than forwarded to Slaves.

The future-release primitive is already reusable, but operator pause/resume/goto remain explicitly unimplemented in the prototype documentation (clock-sync.md (docs/prototypes/clock-sync.md:105)).

Events should precede debug looping so event suppression can be designed and tested correctly.

### P2 — Physical synchronization and transition validation

Complete and record the experiments that turn current thresholds and pipeline choices into supported behavior:

- Two or more physical hosts with filmed numbered frames.
- Different host-local renditions of equal logical duration.
- Same-file, A→B, and playlist-wrap transitions.
- Codec, resolution, frame-rate, and audio-format changes.
- Network loss versus Master process restart.
- CPU/I/O pressure and sustained-drift injection.
- Real audio-backed Master clock.
- Master clock-loss recovery.
- Define duration tolerance, drift thresholds, early-EOS behavior, and acceptable transition limits from results.

The procedure exists, but the results section contains no recorded physical test results. The document still calls its correction thresholds provisional.

### P2 — Platform, lifecycle, and unattended-operation hardening

Implement/test:

- Ubuntu x86-64 and Raspberry Pi AArch64 hardware decoding/output.
- Display selection across the supported Wayland/X11/DRM deployment paths.
- Hours/days of loop and reconnect soak testing, including memory and resource monitoring.
- Graceful SIGINT/SIGTERM shutdown.
- Clean shutdown of listeners, subscriber tasks, event workers, sockets, and pipelines.
- Packaging or a clearly supported native runtime bundle.
- Better runtime reporting for decoder, sinks, clock, dropped frames, and hardware acceleration.

Quickstart scripts and media preparation are already present, but Phase 8 and long-duration qualification remain incomplete (BLUEPRINT.md (docs/BLUEPRINT.md:3282)).

### P3 — Optional deployment modes and later enhancements

Only after unicast V1 is stable:

- Multicast and broadcast snapshot distribution.
- Runtime program switching/loading.
- Targeted OSD.
- Optional console authentication.
- Scheduled ahead-of-time external-controller events.
- Live reload as an explicit new generation.

Automatic Master election, event failover, GUI/web UI, media transfer, cross-cue debug loops, and similar items should remain out of V1.

## What is already substantially implemented

The following should be evolved rather than rebuilt:

- GStreamer local playback and muting.
- Gapless playbin3 handoff for playlists and loops.
- Media probing and timeline calculations for playlists.
- Master playback-clock export and Slave network-clock calibration.
- Future-clock preparation/release.
- Unique Master sessions.
- Complete prototype snapshots with sequence/cue-local positions.
- Late join and reconnect.
- Slave free-running during Master loss.
- Snapshot refresh without constant seeking.
- Sustained-drift detection and prepared local repair.
- Multiple concurrent Slaves.
- Linux/Pi build assistance and media-normalization tooling.

All 32 current unit tests pass. The most coherent next milestone is therefore: implement the complete ProgramConfig, validation/hash, and identity-aware subscription together, then use that foundation to replace the fixed-generation laboratory timeline with the authoritative cue/transport controller.
