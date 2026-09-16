# Blueprint Index

This is a navigation aid for [BLUEPRINT.md](BLUEPRINT.md), not a replacement for it.
The blueprint is the authoritative source; this file deliberately summarizes only enough
context to locate the relevant source sections without loading the entire document.

## How to use this index

1. Find the subject in the task-oriented lookup below.
2. Read the listed sections in `BLUEPRINT.md` before making a design or implementation decision.
3. Use the complete section directory when the topic does not fit one lookup category.
4. Search for a section with its number and title, for example `# 31. Initial Slave Synchronization`.

Section ranges are inclusive. Related sections may be non-contiguous because the blueprint
revisits some concerns from configuration, runtime, protocol, testing, and implementation
perspectives.

## Architectural core

| Concern | Source | Essential orientation |
|---|---:|---|
| Project purpose | §§1–2, 120 | Independent local media is played against one Master-derived timeline. |
| Non-negotiable invariants | §§112–114 | Compact statement of the boundaries implementation must preserve. |
| High-level architecture | §§6–7 | Major responsibilities and suggested component boundaries. |
| Authority model | §§2.2–2.3, 14, 18–23 | The Master owns clock, transport, cue progression, session, and discontinuities. |
| Local versus shared data | §§2.1, 2.4–2.6, 9–12 | The program and logical cues are shared; media mappings and events are host-local. |
| Scope boundaries | §§102–103, 113 | Plain synchronized playback, not warping, distributed rendering, or general orchestration. |
| Delivery sequence | §§85, 107, 115–118 | Validate risky GStreamer and synchronization assumptions with focused prototypes first. |

## Task-oriented lookup

| When working on… | Read these sections | What they cover |
|---|---:|---|
| Language and media stack | §§3–5 | Linux targets, Rust, GStreamer, and deployment philosophy. |
| Module or crate boundaries | §§6–7, 77–80 | Conceptual components, typed data models, runtime state, and concurrency. |
| Player identity | §§8, 27–29, 97 | CLI/hostname/alias resolution, uniqueness, handshake, and registry data. |
| Program schema | §§9–12, 74–77 | Cues, player mappings, defaults, paths, schema version, example TOML, and Rust types. |
| Program hashing | §§28, 34, 90 | Canonical whole-program hash, handshake behavior, and tests. |
| Program load or switch | §§23, 67–68, 111 | Generation changes, initial state, future switching, and reload boundaries. |
| Media probing and duration | §§13–14, 17, 49, 84 | Local probing, Master duration authority, mismatch handling, and validation. |
| Playlist and cue lifecycle | §§9, 14–17, 67, 69, 99–101 | Cue identity, EOS authority, looping, preloading, and clock-aligned transitions. |
| Gapless playback | §§15–17, 85, 87, 99–100 | Requirements, candidate GStreamer mechanisms, experiments, and transition tests. |
| Clock selection | §§18–20, 82–84 | Master pipeline clock, audio/system policies, audio-less operation, and shared time relation. |
| Transport state model | §§20–23, 78–79 | Anchor relation, snapshot fields, session ID, generation, and debug-loop state. |
| Network responsibilities | §§24–26, 65–66 | Separation of subscription/control, clocks, and state distribution. |
| State distribution | §§21, 25–26, 65–66, 91–96 | Snapshot semantics, unicast default, optional multicast/broadcast, encoding, and ordering. |
| Subscription protocol | §§27–29, 34, 92, 97 | Startup, handshake, keepalive, compatibility, identity, hash, and subscriber state. |
| Slave joining | §§27–28, 31, 33, 98 | Clock calibration, seek/preroll, future release, and readiness. |
| Network or Master failure | §§22, 30, 32, 70, 88 | Free-running Slaves, reconnection, correction policy, and failure tests. |
| Synchronization correction | §§20, 31–33, 86 | Expected versus actual position, thresholds, resync, and measurement. |
| Synchronized operations | §§33, 52–60, 98 | Future-clock release, pause/play/stop/goto, loops, and readiness. |
| Timed events | §§35–43, 72, 89, 118 | Configuration, timing, execution, crossing semantics, traversal identity, and tests. |
| Process events | §§38, 72, 80, 109 | Non-blocking low-priority execution, logging, threading, and shutdown. |
| UDP events | §§39–40, 72 | Prepared sockets, payloads, logging, and future timestamped controllers. |
| Audio behavior | §§12, 18–19, 44, 82–84 | Mapping, Master audio authority, clock policy, gain/mute, and external audio. |
| Video/display behavior | §§45–47, 101–102 | Sizing, fit modes, display abstraction, hardware decoding, VFR, and rendering scope. |
| CLI | §§44–50, 94–95, 104–106 | Playback/display options, roles, addressing, validation, and startup flows. |
| Installation console | §§50–64 | Exposure policy, grammar, transport operations, debug loop, OSD, and status. |
| Validation | §§13, 17, 34, 37, 49, 74–77 | Structural, identity, media, duration, event, network, and hash checks. |
| Error classification | §70 | Fatal startup, rejection, warning, and recoverable runtime failures. |
| Logging and diagnostics | §§47, 63, 71–72, 86 | Selected media stack, sync state, participant status, events, and measurement. |
| Security | §§50, 73, 91–92 | Disabled-by-default console, safe packet parsing, and command-execution boundary. |
| Async/threading design | §§38–40, 80–81 | GLib/GStreamer integration, network/control work, workers, and real-time expectations. |
| Packaging and platforms | §§3, 5, 47, 85, 107 | Linux targets, bundled runtime goals, hardware acceleration, experiments, and hardening. |
| Test strategy | §§85–90, 108, 115–118 | Pipeline, synchronization, gapless, recovery, events, hashing, endurance, and prototypes. |
| Shutdown and signals | §§109–111 | Cleanup, child-process handling, SIGINT/SIGTERM, and future explicit reload. |
| V1 exclusions | §§40, 58, 68, 73, 102–103, 111 | Deferred capabilities and intentional non-goals. |

## Complete section directory

### Foundations and configuration (§§1–17)

- §1 — Purpose of This Document
- §2 — Core Design Principles
  - §2.1 — Local decoding
  - §2.2 — One authoritative Master
  - §2.3 — Shared clock, not repeated seeking
  - §2.4 — Shared logical program, host-specific media
  - §2.5 — Cue-relative timing
  - §2.6 — Host-specific events
  - §2.7 — Installation controls are diagnostic tools
- §3 — Primary Platforms
- §4 — Technology Choice
  - §4.1 — Language
  - §4.2 — Media framework
- §5 — Deployment Philosophy
- §6 — High-Level Architecture
- §7 — Major Internal Components
- §8 — Player Identity
- §9 — Program Model
- §10 — Per-Player Media Mapping
- §11 — Default Player Mapping
- §12 — Video and Audio Mapping
- §13 — Media Duration
- §14 — Cue Completion Authority
- §15 — Playlist Looping
- §16 — Seamless Playback
- §17 — Recommended Media Consistency

### Clock, transport, and network (§§18–34)

- §18 — Master Clock
- §19 — Master Clock Policy
- §20 — Shared Time Model
- §21 — Runtime Transport State
- §22 — Session ID
- §23 — Generation Counter
- §24 — Network Architecture
- §25 — Default State Distribution: Unicast Fan-Out
- §26 — Optional Multicast and Broadcast
- §27 — Slave Startup
- §28 — Subscription Handshake
- §29 — Keepalive
- §30 — Master Failure
- §31 — Initial Slave Synchronization
- §32 — Synchronization Corrections
- §33 — Play Preparation / Synchronized Release
- §34 — Program Definition Hash

### Timed events (§§35–43)

- §35 — Timed Events
- §36 — Event Timing Forms
  - §36.1 — Relative to cue start
  - §36.2 — Relative to cue end
- §37 — Event Time Parsing
- §38 — Process Events
- §39 — Packet Events
- §40 — Future Scheduled External Events
- §41 — Event Firing Semantics
- §42 — Event Traversal Identity
- §43 — Event Failover

### Playback controls and installation tools (§§44–64)

- §44 — Audio Controls
- §45 — Display Controls
- §46 — Multi-Display Selection
- §47 — Hardware Acceleration
- §48 — Master/Slave CLI
- §49 — Validation Command
- §50 — Installation Console
- §51 — Console Commands
- §52 — Pause
- §53 — Play
- §54 — Stop
- §55 — Goto
- §56 — Time Input
- §57 — Debug Loop
- §58 — Debug Loop Scope
- §59 — Events During Debug Loop
- §60 — Unloop
- §61 — OSD
- §62 — OSD Timecode
- §63 — Status Command
- §64 — Console Commands vs Network Protocol

### Reliability, operations, and configuration details (§§65–84)

- §65 — Control Reliability
  - §65.1 — Snapshot/state traffic
  - §65.2 — Commands requiring receipt
- §66 — State Snapshot Frequency
- §67 — Program Loading
- §68 — Runtime Program Switching
- §69 — Normal Runtime Behavior
- §70 — Error Handling Philosophy
- §71 — Logging
- §72 — Event Logging
- §73 — Security
- §74 — File Paths
- §75 — Program Configuration Evolution
- §76 — Suggested Program File Example
- §77 — Potential Rust Data Model
- §78 — Potential Runtime Model
- §79 — Debug Loop Runtime State
- §80 — Threading / Async Model
- §81 — Real-Time Expectations
- §82 — Master Audio Authority
- §83 — Audio-Less Installation
- §84 — Separate Audio File

### Experiments, testing, and protocol detail (§§85–101)

- §85 — GStreamer Pipeline Experiments
- §86 — Testing Synchronization
- §87 — Testing Gapless Transitions
- §88 — Testing Master Failure
- §89 — Testing Event Semantics
- §90 — Testing Program Hash
- §91 — Potential Protocol Encoding
- §92 — Protocol Versioning
- §93 — State Packet Ordering
- §94 — Network Address Configuration
- §95 — Slave Master Address
- §96 — No Broadcast Dependency
- §97 — Master Subscriber Registry
- §98 — Ready State
- §99 — Normal Cue Preloading
- §100 — Cue Transition Synchronization
- §101 — Frame Rate and Variable Frame Rate

### Scope, delivery, and lifecycle (§§102–120)

- §102 — Color / Rendering Scope
- §103 — Out of Scope for V1
- §104 — Suggested Executable Interface
- §105 — Startup Example — Master
- §106 — Startup Example — Slave
- §107 — Recommended Development Phases
  - Phase 1 — Local Playback
  - Phase 2 — Program Model
  - Phase 3 — Master/Slave Clock Prototype
  - Phase 4 — Transport Protocol
  - Phase 5 — Synchronized Cue Transitions
  - Phase 6 — Timed Events
  - Phase 7 — Installation Console
  - Phase 8 — Platform Hardening
- §108 — Long-Duration Stability Testing
- §109 — Graceful Shutdown
- §110 — Signals
- §111 — Configuration Reload
- §112 — Design Invariant Summary
- §113 — Architectural Non-Goals
- §114 — Implementation Philosophy
- §115 — First Prototype Goal
- §116 — Second Prototype Goal
- §117 — Third Prototype Goal
- §118 — Fourth Prototype Goal
- §119 — Documentation Requirements
- §120 — Final Architectural Statement

## High-value reading sets

These sets are shortcuts for recurring work. They do not supersede the task-oriented lookup.

### Before changing architecture

Read §§2, 6–7, 102–103, and 112–114.

### Before implementing playback

Read §§12–20, 44–47, 82–87, and 99–101.

### Before implementing synchronization or networking

Read §§18–34, 65–66, 78, 88, and 91–100.

### Before implementing configuration or validation

Read §§8–13, 34–37, 49, 74–77, and 90.

### Before implementing timed events

Read §§2.5–2.6, 35–43, 59, 72, 80, 89, and 118.

### Before implementing installation controls

Read §§2.7, 33, 50–64, 79, 98, and 117.

### Before defining milestones

Read §§85–90, 107–108, and 115–118.
