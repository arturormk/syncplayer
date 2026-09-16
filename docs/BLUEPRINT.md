# Synchronized Distributed Video Player — Blueprint

## 1. Purpose of This Document

This document defines the architecture, behavior, constraints, configuration model, synchronization model, network protocol responsibilities, media pipeline requirements, installation/debug facilities, and implementation plan for a lightweight synchronized video player written in Rust.

The target audience is a coding agent implementing the project. Treat the architectural decisions in this document as intentional requirements unless implementation experience proves that a specific detail must change.

The project is deliberately narrower than a general-purpose media player and deliberately different from WarpWeaver.

Its purpose is:

> Play independent local media files on multiple networked computers while keeping all players synchronized to one authoritative Master timeline.

The system is intended primarily for installations such as:

- synchronized multi-screen playback;
- projection installations;
- museum and exhibition systems;
- architectural projection;
- installations where different displays show different but duration-matched videos;
- installations where one player provides the audio for all displays;
- installations requiring frame-sensitive coordination with physical features;
- installations requiring timed control of lighting, projectors, relays, external controllers, or other systems.

Each machine accesses its own local media.

Media is never streamed from the Master to the Slaves.

The network carries synchronization, control, clock, health, and installation-debug information only.

---

# 2. Core Design Principles

The implementation should preserve the following principles.

## 2.1 Local decoding

Every player decodes its own local media.

If four machines participate in an installation, there are four local media pipelines.

The Master does not distribute media.

Example:

```text
Master:
    /media/show/master/main.mp4

Slave left:
    /media/show/left/main.mp4

Slave right:
    /media/show/right/main.mp4

Slave ceiling:
    /media/show/ceiling/main.mp4
```

These files may contain completely different images.

They are expected to have matching playback durations.

---

## 2.2 One authoritative Master

There is always exactly one Master.

All Slaves synchronize to it.

The Master is authoritative for:

- current program;
- current cue;
- transport state;
- cue transitions;
- playback generation;
- debug seeks;
- debug loops;
- synchronized release from pause;
- network session identity.

The Master is normally the machine connected to the installation audio system.

The system does not initially require automatic Master election or Master failover.

---

## 2.3 Shared clock, not repeated seeking

Normal synchronization must not consist of repeatedly comparing playback positions and seeking Slaves.

Instead:

- the Master exports the clock used to drive its playback pipeline;
- Slaves maintain synchronized clocks derived from that Master clock;
- transport state establishes a relationship between Master clock time and cue-local media time;
- all pipelines then run naturally against the synchronized clock.

Seeking is an exceptional correction mechanism.

It is used for:

- initial Slave synchronization;
- explicit `goto`;
- program loading;
- major synchronization error recovery;
- Master reconnection when necessary.

It is not the normal means of staying synchronized.

---

## 2.4 Shared logical program, host-specific media

All hosts use the same program-definition file contents.

The file defines:

- logical playlist/cue structure;
- per-player local media mappings;
- per-player timed events;
- global program behavior.

A player selects its section of the shared file using a player identifier.

The player ID is:

1. explicitly supplied by CLI if present;
2. otherwise the host's hostname;
3. optionally resolved through aliases defined in the program configuration.

---

## 2.5 Cue-relative timing

The authored program is a sequence of logical cues.

There is no requirement for the installation author to maintain one cumulative absolute program timeline.

Events and playback positions are relative to the beginning or end of a specific cue.

Example:

```text
intro
    0 ---------------- EOF

main
    0 ------------ event @ 12.5 s ---------------- EOF

outro
    0 ---------------- EOF
```

Changing the duration of `intro` must not require recomputing event times in `main`.

Absolute cumulative program time may exist internally as derived metadata, but must not be required in authored configuration.

---

## 2.6 Host-specific events

Every timed external event belongs to exactly one player.

There are no implicitly global events.

If Slave 3 physically controls the lighting system, then Slave 3 contains and executes the lighting events.

Other players do not execute them.

If multiple hosts need equivalent events, the program may define equivalent event entries for each host explicitly.

There is no implicit event failover.

---

## 2.7 Installation controls are diagnostic tools

Pause, goto, loop, OSD and similar commands are installation/debugging facilities.

A completed installation normally runs continuously without these commands.

The implementation should therefore prioritize:

- usefulness;
- predictability;
- synchronization;
- simplicity;

over elaborate media-player UI semantics.

---

# 3. Primary Platforms

The initial required platforms are:

```text
x86_64 Linux
    primarily Ubuntu

aarch64 Linux
    Raspberry Pi
```

Likely Rust compilation targets:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

Raspberry Pi 64-bit is ARM64/AArch64, not AMD64.

Other targets are optional future work.

---

# 4. Technology Choice

## 4.1 Language

Rust.

The application should use safe Rust wherever practical.

Unsafe code should only appear where required by platform or FFI boundaries and should be tightly contained.

---

## 4.2 Media framework

GStreamer is the preferred media framework.

Use the Rust GStreamer bindings.

Reasons include:

- broad codec support;
- audio and video playback;
- hardware-accelerated decode support;
- timestamp-aware pipelines;
- gapless playback facilities;
- network clock support;
- mature Linux platform integration;
- support for X11, Wayland, DRM/KMS, audio systems, etc.

The application should avoid reimplementing media decoding, audio synchronization, frame scheduling, or codec selection itself.

---

# 5. Deployment Philosophy

A fully static single executable is desirable but not an absolute architectural requirement.

The stronger deployment goal is:

> A machine should not require the system administrator to manually install an arbitrary matching GStreamer environment before the player can run.

Possible deployment strategies include:

- statically linked GStreamer components where practical;
- statically registered GStreamer plugins;
- bundled shared libraries;
- bundled GStreamer plugins;
- a self-contained application directory;
- AppImage or similar packaging;
- system libraries only where inherently platform-dependent.

Do not sacrifice hardware acceleration or reliable platform integration solely to claim that every library is statically linked.

GPU drivers, VA-API, V4L2, Wayland, DRM, ALSA, PipeWire, etc. may necessarily remain dynamically supplied by the host.

Packaging should therefore be treated as an implementation workstream, not as something that distorts the playback architecture.

---

# 6. High-Level Architecture

Conceptually:

```text
                         syncplayer
                             |
          +------------------+------------------+
          |                  |                  |
        Config            Transport           Network
          |                  |                  |
          |           play/pause/goto      master/slave
          |           cue management       protocol
          |                  |                  |
          +------------------+------------------+
                             |
                       Playback Engine
                             |
             +---------------+---------------+
             |               |               |
         Playlist         Clock          Preloader
             |               |               |
             +---------------+---------------+
                             |
                         GStreamer
                        /         \
                   Video sink    Audio sink
```

Add an independent timed-event subsystem:

```text
Playback timeline
      |
Event scheduler
   /       \
process    packet
events     events
   |
low-priority
worker
```

And an installation/debug console on the Master:

```text
telnet / netcat / TCP client
             |
      Master console
             |
     Transport Controller
             |
  structured runtime state
      /             \
  Master           Slaves
```

---

# 7. Major Internal Components

The codebase should be divided into responsibilities rather than one monolithic player.

Suggested conceptual modules:

```text
config
identity
program
media
pipeline
clock
transport
sync
protocol
subscriber
events
console
osd
display
validation
logging
platform
```

Exact Rust module names may differ.

---

# 8. Player Identity

Each process has a `player_id`.

Resolution order:

```text
--player-id NAME
        |
        v
explicit NAME

otherwise
        |
        v
system hostname

optionally
        |
        v
alias lookup
```

Example:

```toml
[aliases]
"projection-pc-01" = "left-wall"
"projection-pc-02" = "center-wall"
"projection-pc-03" = "right-wall"
```

Player IDs must be unique among currently subscribed participants.

If two connected Slaves present the same logical player ID, the Master must report the collision clearly.

Do not silently accept ambiguous player identities.

---

# 9. Program Model

A program definition file describes one synchronized show.

TOML is the preferred initial format.

Example:

```toml
[program]
id = "museum-show"
loop = true

[[program.cue]]
id = "intro"

[[program.cue]]
id = "main"

[[program.cue]]
id = "outro"
```

The sequence of `program.cue` entries defines playlist order.

Cue IDs are logical names.

The same cue ID may eventually be allowed to occur multiple times in a playlist, so runtime state must also track cue index/occurrence.

---

# 10. Per-Player Media Mapping

Each player maps logical cues to local media.

Example:

```toml
[players.left-wall.intro]
video = "/media/show/left/intro.mp4"

[players.left-wall.main]
video = "/media/show/left/main.mp4"

[players.left-wall.outro]
video = "/media/show/left/outro.mp4"
```

Another host:

```toml
[players.right-wall.intro]
video = "/media/show/right/intro.mp4"

[players.right-wall.main]
video = "/media/show/right/main.mp4"

[players.right-wall.outro]
video = "/media/show/right/outro.mp4"
```

The synchronization protocol deals in:

```text
program ID
cue index
cue ID
cue-local time
```

It must not normally send media file paths.

---

# 11. Default Player Mapping

Support a default mapping so installations do not need to duplicate common settings.

Possible syntax:

```toml
[players.default.intro]
video = "/media/common/intro.mp4"

[players.default.outro]
video = "/media/common/outro.mp4"

[players.left-wall.main]
video = "/media/left/main.mp4"

[players.right-wall.main]
video = "/media/right/main.mp4"
```

Resolution order:

```text
players.<player-id>.<cue>
        |
        v
players.default.<cue>
        |
        v
configuration error
```

Player-specific values override default values.

---

# 12. Video and Audio Mapping

A cue may contain:

- video with embedded audio;
- video only;
- external audio;
- video plus external audio;
- potentially audio-only media in future.

Examples:

```toml
[players.master.main]
video = "/media/main.mp4"
```

External audio:

```toml
[players.master.main]
video = "/media/main-video.mp4"
audio = "/media/main-english.flac"
```

A Slave:

```toml
[players.left.main]
video = "/media/left-main.mp4"
mute = true
```

An external audio stream must remain synchronized to the same cue timeline as the video.

---

# 13. Media Duration

Do not require the author to manually specify cue durations under normal operation.

Instead:

- probe local media;
- determine duration from the media itself;
- validate corresponding durations across players during runtime/subscription.

The Master is authoritative for cue duration and cue completion.

A Slave should compare its own media duration against the Master's announced duration.

Example diagnostic:

```text
cue: main

Master: 66.712000 s
Slave:  66.711978 s
delta:  22 us

OK
```

A significant mismatch should be reported clearly.

The exact tolerance should be configurable or experimentally determined.

Do not silently ignore a substantial mismatch.

---

# 14. Cue Completion Authority

The Master decides when a cue ends.

Normal behavior:

```text
Master media reaches EOS
        |
        v
Master advances logical cue
        |
        v
new authoritative transport state
        |
        v
Slaves transition
```

Slaves should not independently change logical playlist position merely because their own decoder reports EOS.

Their local EOS is useful for:

- validation;
- diagnostics;
- preloading;
- detecting bad installation media.

If a Slave ends slightly early, it should remain logically in the current cue until the Master transitions.

Exact visual hold behavior may be determined experimentally.

---

# 15. Playlist Looping

If:

```toml
[program]
loop = true
```

then:

```text
cue 0
cue 1
cue 2
...
cue N
cue 0
cue 1
...
```

must be seamless to the extent supported by the media pipeline.

The transition from the final cue back to cue 0 should use the same preloading/gapless mechanism as any other cue boundary.

---

# 16. Seamless Playback

The player must support:

- seamless looping of a single media item;
- seamless transitions between different media files;
- seamless looping of an entire playlist.

The implementation should preload or preroll upcoming media before the current item ends.

Initial implementation may use GStreamer gapless playback facilities such as `playbin3`.

If testing shows that this cannot provide sufficiently deterministic transitions, implement a stronger architecture using independently prepared decode branches or pipelines feeding a switch such as an input selector.

Treat exact gapless implementation as an implementation experiment, but preserve the architectural requirement:

> The next cue must be prepared before the current cue ends.

Do not design around opening a new file only after EOS has already occurred.

---

# 17. Recommended Media Consistency

For the cleanest transitions, installation media should preferably use compatible characteristics across adjacent cues:

- resolution;
- frame rate;
- codec family;
- pixel format;
- audio sample rate;
- channel layout.

The player should not require these to match unless technically necessary, but validation may warn about transitions likely to require expensive pipeline reconfiguration.

---

# 18. Master Clock

The Master should use the actual clock driving its GStreamer playback pipeline whenever practical.

When audio is active, the audio sink clock is often desirable because the installation's audible playback should remain authoritative.

The Master exposes its playback clock through GStreamer's network clock facilities.

Preferred conceptual mechanism:

```text
Master pipeline clock
        |
        v
GstNetTimeProvider
        |
        +---- network ----+
                          |
                          v
                  GstNetClientClock
                          |
                          v
                    Slave pipeline
```

Exact GStreamer API usage should follow the current Rust bindings.

---

# 19. Master Clock Policy

Expose a clock-selection option conceptually similar to:

```text
--clock auto
--clock audio
--clock system
```

Suggested semantics:

### auto

Prefer a suitable media/audio clock where appropriate.

Fall back to a stable system clock.

### audio

Explicitly prefer the audio-driven clock where available.

### system

Use a system/monotonic clock.

Exact GStreamer behavior must be tested.

Muted audio must not accidentally disable stable playback timing.

---

# 20. Shared Time Model

Playback synchronization is based on an anchor relation.

A Master state snapshot conceptually contains:

```text
anchor_clock
anchor_cue_position
transport_state
```

For normal forward playback:

```text
desired_cue_position =
    anchor_cue_position
    + (current_synchronized_clock - anchor_clock)
```

When paused:

```text
desired_cue_position =
    anchor_cue_position
```

Do not use wall-clock date/time for synchronization.

Use monotonic media/network clocks.

---

# 21. Runtime Transport State

The authoritative state should contain enough information for a Slave to reconstruct playback without having received previous packets.

Conceptually:

```text
protocol_version
session_id
generation
program_id
program_hash

cue_index
cue_id
cue_iteration

transport_state

anchor_clock
anchor_cue_position

cue_duration

debug_loop_state
```

Not every field must exist in every packet if the protocol design provides an equivalent full snapshot.

The crucial principle is:

> State packets are snapshots, not incremental commands.

---

# 22. Session ID

Every Master process run should have a unique `session_id`.

For example, generate a UUID at startup.

If the Master disappears and restarts, the new session ID tells Slaves unambiguously that they are talking to a new playback authority.

---

# 23. Generation Counter

Maintain a monotonically increasing transport `generation`.

Increment it for discontinuous state changes such as:

- loading a new program;
- goto/seek;
- stop;
- entering or leaving debug loop mode;
- major resynchronization state changes;
- other operations that invalidate prior timeline assumptions.

Normal continuous playback packets retain the same generation.

This makes stale packets harmless.

---

# 24. Network Architecture

There are three conceptual network responsibilities:

```text
1. subscription/control
2. clock synchronization
3. transport-state distribution
```

These should remain logically separate even if some share sockets.

---

# 25. Default State Distribution: Unicast Fan-Out

The default synchronization-state distribution mode is unicast.

Slaves explicitly subscribe to the Master.

The Master maintains a subscriber table.

Example:

```text
Slave A ---> SUBSCRIBE ---> Master
Slave B ---> SUBSCRIBE ---> Master
Slave C ---> SUBSCRIBE ---> Master

Master ---> state ---> Slave A
       ---> state ---> Slave B
       ---> state ---> Slave C
```

This is preferred because it works reliably on:

- cheap routers;
- restricted LANs;
- shared networks;
- routed networks;
- VPNs;
- environments where multicast is filtered.

The extra bandwidth is negligible for typical installations.

---

# 26. Optional Multicast and Broadcast

Support optional state-distribution modes:

```text
--sync-mode unicast
--sync-mode multicast
--sync-mode broadcast
```

`unicast` is the default.

The synchronization payload format should remain the same.

Only packet destination strategy changes.

Conceptually:

```text
unicast:
    send same snapshot individually to subscribers

multicast:
    send once to configured multicast group

broadcast:
    send once to configured broadcast address
```

Even under multicast or broadcast, Slaves should still register and send keepalives.

This preserves:

- participant awareness;
- status reporting;
- health monitoring;
- protocol negotiation.

---

# 27. Slave Startup

A Slave is started with at minimum:

```text
syncplayer slave show.toml --master 192.168.1.40
```

Optionally:

```text
--player-id left-wall
```

The Slave:

1. loads and validates the program;
2. determines its player identity;
3. probes required local media;
4. contacts the Master;
5. sends a subscription request;
6. supplies its identity and program information;
7. receives session/synchronization information;
8. starts network-clock synchronization;
9. joins any configured state distribution mechanism;
10. synchronizes to current Master transport state;
11. begins or waits according to the Master state.

---

# 28. Subscription Handshake

A subscription request should conceptually include:

```text
protocol version
player ID
program ID
program hash
state receive port
capabilities
optional software version
```

The Master response should include:

```text
accepted/rejected
Master session ID
Master protocol version
clock endpoint
state transport mode
state transport parameters
current transport generation
current program/cue state
```

Program mismatch or duplicate player identity should be clearly reported.

---

# 29. Keepalive

Every Slave periodically sends keepalive packets to the Master.

Example cadence:

```text
every few seconds
```

The exact interval should be configurable or chosen conservatively.

If no keepalive is received from a Slave for approximately 60 seconds, the Master may mark/drop it from the subscriber registry.

The exact timeout should be configurable.

Dropping a subscriber means:

- remove it from active status;
- stop unicast fan-out to it.

It does not necessarily mean the Slave process has stopped.

---

# 30. Master Failure

If Master state packets disappear:

> Do not stop playback immediately.

The Slave should continue free-running from its current synchronized state.

It should continue:

- decoding;
- advancing through the current cue;
- following its locally known playlist;
- looping if the program loops;
- executing its own normal cue-relative events;
- maintaining playback at the current rate.

Meanwhile it should attempt to re-establish contact with the configured Master IP.

When the Master returns:

- detect whether the session ID is the same or new;
- obtain current authoritative state;
- determine synchronization error;
- rejoin smoothly if practical;
- seek/preroll if necessary.

A temporary Master outage should not turn all displays black.

---

# 31. Initial Slave Synchronization

A joining Slave must not simply seek to the Master's current position and immediately hit PLAY.

Preferred process:

1. subscribe;
2. establish/calibrate the network clock;
3. receive current cue and position;
4. load the correct local media;
5. seek while paused;
6. preroll;
7. choose or receive a future Master-clock release time;
8. calculate the correct cue position for that instant;
9. release playback against the shared clock.

This minimizes visible startup mismatch.

---

# 32. Synchronization Corrections

Normal steady-state playback should require little or no seeking.

Periodically compare:

```text
expected cue position
actual pipeline cue position
```

Define thresholds experimentally:

```text
tiny error:
    ignore

small persistent error:
    allow clock synchronization to correct naturally

moderate error:
    controlled correction if needed

large error:
    pause/seek/preroll/rejoin

generation/session discontinuity:
    authoritative resync
```

Do not continuously seek the pipeline to chase millisecond-level fluctuations.

---

# 33. Play Preparation / Synchronized Release

Installation-debug `play` should support a preparation interval.

For example:

```text
play
```

may use a default preparation delay.

Or:

```text
play 3.0
```

means:

> prepare everyone and resume at a Master-clock time approximately three seconds in the future.

The Master sends structured preparation state containing:

```text
cue
cue position
generation
release_at_master_clock
```

Slaves seek/preroll and wait.

They may ACK:

```text
READY generation=N
```

The actual release instant is defined by the shared future clock timestamp.

It is not defined by whichever network packet happens to arrive last.

The Master may send a packet at release time as confirmation, but that packet must not be the timing authority.

---

# 34. Program Definition Hash

All hosts are expected to have byte-equivalent or semantically equivalent copies of the same program definition.

Compute a canonical program hash.

Prefer hashing a canonical parsed representation rather than raw textual bytes so irrelevant formatting differences do not matter.

The hash should cover the complete program definition, including:

- logical playlist;
- all player mappings;
- all player events;
- aliases;
- relevant settings.

This intentionally detects cases where one machine is running an outdated installation configuration even if its own section did not change.

On subscription:

```text
Master hash: abc...
Slave hash:  def...
```

should cause a prominent mismatch warning and preferably rejection by default.

Provide an explicit override only if useful.

---

# 35. Timed Events

Timed events are first-class entries in the program configuration.

Every event belongs to exactly one player.

Example:

```toml
[[players.slave3.main.event]]
id = "dim-ambient"
at = "12.5s"
type = "process"
command = ["/opt/show/dim-lights.sh"]
```

Another:

```toml
[[players.slave3.main.event]]
id = "lighting-cue"
at = "37.250s"
type = "udp"
host = "192.168.1.80"
port = 5000
payload = "LIGHTING_CUE_17"
```

Only `slave3` executes these events.

---

# 36. Event Timing Forms

Support at least two relative timing forms.

## 36.1 Relative to cue start

```toml
at = "12.5s"
```

Meaning:

> 12.5 seconds after the beginning of this logical cue.

---

## 36.2 Relative to cue end

```toml
before_end = "2s"
```

Meaning:

> two seconds before the locally resolved cue end.

This is useful for events such as:

- preload external device;
- prepare lighting change;
- lower house lights just before the next cue;
- trigger external equipment before transition.

`at` and `before_end` should be mutually exclusive.

---

# 37. Event Time Parsing

Human-authored time values should be ergonomic.

At minimum accept seconds:

```toml
at = "12.5s"
```

Potentially accept forms such as:

```text
500ms
12s
1m2.5s
```

Internally convert all event timing to nanoseconds.

Do not store or compare floating-point seconds internally where exact integer time can be used.

---

# 38. Process Events

A process event launches a local process.

Example:

```toml
[[players.slave3.main.event]]
id = "dim-lights"
at = "12.5s"
type = "process"
command = ["/opt/show/dim-lights.sh"]
```

Prefer direct argv execution.

Possible optional shell form:

```toml
shell = "/opt/show/dim-lights.sh"
```

Do not execute process events on GStreamer streaming threads.

The event scheduler should enqueue work to a low-priority worker.

The worker should run child processes with low scheduling priority where practical.

On Linux, consider:

```text
nice 19
I/O idle priority
```

or equivalent Rust/system APIs.

Playback must never wait for the external process to finish.

Process output should not block playback.

If stdout/stderr are captured, read them asynchronously or redirect them safely.

---

# 39. Packet Events

Packet events are intended for timing-sensitive external control.

Initial required transport:

```text
UDP
```

Example:

```toml
[[players.slave3.main.event]]
id = "laser-cue"
at = "37.250s"
type = "udp"

host = "192.168.1.80"
port = 7000
payload = "LASER 3 ON"
```

Support at least textual payloads.

Possible additional form:

```toml
payload_hex = "01 10 00 ff"
```

Potential future payload types:

- binary file;
- OSC;
- other installation protocols.

Do not make these necessary for V1.

UDP sockets should preferably be prepared ahead of time rather than created synchronously at every event.

---

# 40. Future Scheduled External Events

Leave room for a future mode where a timing-critical packet is sent ahead of time with an exact future Master-clock execution timestamp.

Conceptually:

```text
event_id
execute_at_master_clock
payload
```

A dedicated external controller synchronized to the same clock could then execute the action locally at the requested time.

This is not required for V1.

Do not architect the event system in a way that makes such a future extension impossible.

---

# 41. Event Firing Semantics

Normal rules:

```text
normal forward playback crosses event:
    FIRE

playlist loops and crosses event again:
    FIRE AGAIN

explicit seek skips over event:
    DO NOT FIRE

Slave initial synchronization lands after event:
    DO NOT FIRE

resynchronization jumps over event:
    DO NOT FIRE
```

Events are triggered by a normal timeline crossing, not merely because:

```text
current_position >= event_time
```

The implementation must prevent duplicate firing due to:

- packet repetition;
- small clock corrections;
- position jitter;
- repeated status updates.

---

# 42. Event Traversal Identity

Track enough state to distinguish separate cue traversals.

Conceptually:

```text
session
generation
playlist iteration
cue index
cue traversal
event ID
```

If `event X` fired in loop iteration 17, it must become eligible again in loop iteration 18.

It must not fire twice in iteration 17 because the reported time moved around its trigger boundary.

---

# 43. Event Failover

There is no automatic failover for host-specific events.

Example:

```text
slave3 controls dimmer
slave3 fails
```

Result:

```text
dimmer event does not execute
```

Another host must not silently assume ownership.

If redundancy is required, define equivalent event entries explicitly for another player.

---

# 44. Audio Controls

Support:

```text
--mute
```

and gain in decibels:

```text
--gain-db -6
--gain-db 0
--gain-db +3
--gain-db +12
```

Internally convert:

```text
linear_gain = 10^(dB / 20)
```

Positive gain may clip.

Warn when appropriate.

Do not make positive gain impossible simply because unity is normally the natural maximum.

---

# 45. Display Controls

Support CLI configuration for display behavior.

At minimum:

```text
--fullscreen

--width WIDTH
--height HEIGHT

--display DISPLAY_ID
```

Scaling policy should support at least:

```text
contain
cover
stretch
```

Example:

```text
--fit stretch
```

For many installations the media will already match the physical display resolution, but scaling must remain available.

---

# 46. Multi-Display Selection

The application should provide a way to choose which physical display/output to use.

This is platform-specific.

Hide it behind a display abstraction.

Potential backends may include:

- Wayland;
- X11;
- DRM/KMS.

Do not let compositor-specific concepts leak into playback synchronization code.

Exact implementation is an experimental workstream.

---

# 47. Hardware Acceleration

Prefer hardware decoding where available.

Default behavior:

```text
--hwdec auto
```

Potential options:

```text
--hwdec auto
--hwdec off
```

Let GStreamer capability discovery/plugin ranking perform most decoder selection.

Do not hard-code assumptions such as:

```text
Raspberry Pi => decoder X
```

because available decoders depend on:

- OS;
- kernel;
- Pi generation;
- GStreamer version;
- plugin build;
- codec;
- output path.

Provide enough logging to report which decoder and sink were actually selected.

---

# 48. Master/Slave CLI

Prefer one executable with subcommands.

Example Master:

```text
syncplayer master show.toml \
    --player-id master \
    --fullscreen \
    --display 0
```

Example Slave:

```text
syncplayer slave show.toml \
    --master 192.168.1.40 \
    --player-id left-wall \
    --fullscreen \
    --display 1 \
    --mute
```

The same playback engine should be shared by both modes.

Do not create separate independent Master and Slave media implementations.

---

# 49. Validation Command

Provide:

```text
syncplayer validate show.toml
```

This validates the configuration for the current player.

Potential optional:

```text
--player-id NAME
```

Validation should include:

- TOML syntax;
- logical program structure;
- duplicate cue IDs where forbidden;
- player identity resolution;
- required player section;
- referenced media existence;
- media readability;
- media duration probing;
- event IDs;
- duplicate event IDs where problematic;
- events beyond cue EOF;
- invalid `before_end`;
- unsupported event types;
- invalid IP/port;
- conflicting audio definitions;
- unsupported or suspicious transition properties;
- playlist loops;
- program hash.

Example output:

```text
Program: museum-show
Player: wall-right

Cue       Video                              Duration       Result
intro     /media/right/intro.mp4             12.000 s       OK
main      /media/right/main.mp4              66.712 s       OK
outro     /media/right/outro.mp4              8.000 s       OK

Events:
main @ 12.500 s      dim-ambient              OK
main @ end-2.000 s   prepare-projector        OK

Validation successful.
```

---

# 50. Installation Console

The Master may optionally expose a simple line-oriented TCP control socket.

Example:

```text
--control-port 2323
```

A user can connect with:

```text
telnet master 2323
```

or:

```text
nc master 2323
```

This is not intended to implement the full Telnet protocol.

It is simply a text-oriented TCP console compatible with basic terminal clients.

The console should be disabled unless explicitly requested.

Provide a configurable bind address.

Because installations may use shared LANs, leave room for optional authentication later.

Do not expose an unauthenticated control interface by default on every interface.

---

# 51. Console Commands

Initial commands should include approximately:

```text
help

status

play
play SECONDS

pause
stop

goto TIME
goto CUE TIME

loop SECONDS
unloop

osd on
osd off
osd toggle
```

The exact grammar may evolve slightly.

Keep it human-friendly.

---

# 52. Pause

`pause` freezes the logical cue timeline.

Events also stop progressing because cue time stops.

Wall-clock time spent paused does not trigger events.

All players should converge to the same paused cue and cue-local position.

---

# 53. Play

`play` resumes from the current logical position.

Prefer synchronized future release rather than independently telling each machine to start immediately.

Example:

```text
play 3
```

means:

> prepare all players and resume three seconds from now according to the Master clock.

If no delay is specified, use a reasonable default installation-preparation interval.

---

# 54. Stop

For installation/debug semantics:

```text
stop
```

should mean approximately:

```text
pause
goto cue 0, time 0
```

The program remains loaded.

This is more useful than introducing a complicated additional media state.

---

# 55. Goto

Examples:

```text
goto 00:01:17.500
```

or:

```text
goto main 00:00:12.500
```

`goto` preserves the current paused/playing mode.

Rules:

```text
PAUSED + goto
    seek all players
    remain paused

PLAYING + goto
    prepare all players at destination
    resume synchronously
```

For a playing goto, use a near-future synchronized release time rather than allowing each host to resume independently as soon as its seek completes.

---

# 56. Time Input

Installation console input may accept convenient formats such as:

```text
77.5
00:01:17.500
```

For constant-frame-rate content, optionally support:

```text
HH:MM:SS,ff
```

Internally use nanoseconds.

Do not make frame number the authoritative representation.

---

# 57. Debug Loop

Provide:

```text
loop SECONDS
```

Meaning:

> Create a temporary playback loop beginning at the current cue position and ending SECONDS later.

Example:

Current position:

```text
47.230 s
```

Command:

```text
loop 5
```

Creates:

```text
47.230 -> 52.230 -> 47.230 -> ...
```

This is a runtime diagnostic override.

It must not modify the authored program.

Use cases include:

- geometry alignment;
- projection onto physical features;
- edge blending;
- seam checking;
- frame-sensitive masking;
- multi-display synchronization inspection;
- physical set alignment.

---

# 58. Debug Loop Scope

V1 debug loops should remain inside one cue.

If:

```text
current = 58 s
cue ends = 60 s
loop 5
```

return a clear error rather than implementing an implicit cross-cue loop.

Example:

```text
ERROR:
requested loop exceeds current cue

maximum loop length: 2.000 s
```

Cross-cue loops may be added later if useful.

---

# 59. Events During Debug Loop

Suppress timed external events during debug loops by default.

This prevents repeatedly triggering:

- dimmers;
- relays;
- shell scripts;
- projectors;
- external control systems.

A future explicit debug option may enable event replay if desired.

Normal playlist playback always uses the program's configured events.

---

# 60. Unloop

```text
unloop
```

removes the debug loop override.

Normal forward playback resumes from the current position.

If currently paused, remain paused unless explicitly commanded otherwise.

---

# 61. OSD

Provide an optional diagnostic OSD.

Example:

```text
PLAYER   wall-right
ROLE     slave
PROGRAM  museum-show
ITEM     03 / main
TIME     00:01:17,12
STATE    PAUSED
MASTER   192.168.1.40
```

Potential compact form:

```text
museum-show | 03 main | 00:01:17,12 | PAUSED | wall-right
```

OSD should be a runtime diagnostic overlay, not part of the program media.

Master console commands:

```text
osd on
osd off
osd toggle
```

may control all players.

Potential future targeting:

```text
osd wall-right on
```

is optional.

---

# 62. OSD Timecode

Internally use time.

For constant-frame-rate video, derive:

```text
HH:MM:SS,ff
```

from the known frame rate.

For variable-frame-rate material, prefer:

```text
HH:MM:SS.mmm
```

Do not pretend a fixed frame number exists when it does not.

---

# 63. Status Command

`status` should be useful during installation.

Example:

```text
PROGRAM museum-show
STATE   PAUSED
ITEM    3 main
TIME    00:01:17.483
LOOP    77.483-82.483

PLAYER       IP             STATUS    LAST SEEN
master       192.168.1.20   READY     local
wall-left    192.168.1.21   READY     0.3s
wall-right   192.168.1.22   READY     0.2s
ceiling      192.168.1.23   READY     0.4s
```

Future useful fields:

- clock offset estimate;
- clock stability;
- media duration mismatch;
- current local decoder;
- current sink;
- dropped/late frames;
- ready/preroll status.

Keep V1 manageable.

---

# 64. Console Commands vs Network Protocol

The Master console parses human text.

It should not simply forward that text to Slaves.

Instead:

```text
human console command
        |
        v
Master Transport Controller
        |
        v
structured transport operation/state
        |
        v
Slaves
```

This guarantees one interpretation of operations such as:

- goto;
- play;
- pause;
- loop.

---

# 65. Control Reliability

Distinguish between two types of network traffic.

## 65.1 Snapshot/state traffic

May be UDP and lossy.

Examples:

- current transport state;
- periodic heartbeat state;
- current cue position anchor.

Losing packets is harmless because the next snapshot is complete.

---

## 65.2 Commands requiring receipt

Examples:

- subscription;
- explicit prepare;
- program-load control;
- possibly OSD changes;
- ready status.

These should use acknowledgement/retry semantics.

This may still be implemented over UDP if carefully designed.

Alternatively, a reliable TCP control connection may be used if it simplifies the implementation.

Do not force every synchronization update onto a reliable ordered transport if it causes unnecessary latency/head-of-line behavior.

---

# 66. State Snapshot Frequency

Exact frequency is not architecturally fixed.

Something on the order of:

```text
5-20 Hz
```

is likely more than adequate because snapshots provide timeline anchors rather than frame-by-frame instructions.

Determine through testing.

Clock synchronization is separate from this packet cadence.

---

# 67. Program Loading

A program load operation should:

1. parse/validate program;
2. calculate program hash;
3. resolve player mapping;
4. probe media;
5. prepare cue 0;
6. increment generation;
7. remain paused at cue 0/time 0 by default.

Provide explicit load-and-play behavior only if useful.

Loading a new program must invalidate previous event traversal state.

---

# 68. Runtime Program Switching

The architecture should support future Master-driven selection of another program definition.

However, V1 may reasonably start with one program supplied on process startup.

Do not couple playback internals so tightly to one immutable playlist that program switching becomes impossible later.

---

# 69. Normal Runtime Behavior

A finished installation should be capable of:

```text
start Master
start Slaves

Master starts program
        |
        v
continuous playback
        |
        v
gapless cue transitions
        |
        v
playlist loop
        |
        v
repeat indefinitely
```

No operator interaction should be required.

---

# 70. Error Handling Philosophy

Different classes of errors should behave differently.

## Fatal startup examples

- invalid configuration syntax;
- current player ID has no usable mapping;
- required media missing;
- program structurally invalid;
- essential GStreamer components unavailable.

## Subscription rejection examples

- program hash mismatch;
- incompatible protocol version;
- duplicate active player ID.

## Runtime warnings

- temporary packet loss;
- small clock error;
- positive audio gain;
- suspicious codec transition.

## Runtime recovery

- Master disappears;
- temporary network outage;
- Slave briefly loses state packets.

Avoid terminating video playback for recoverable synchronization/network issues.

---

# 71. Logging

Provide structured, useful logs.

At minimum log:

- startup mode;
- player ID;
- program ID/hash;
- selected local media;
- media durations;
- GStreamer decoder choices;
- video sink;
- audio sink;
- hardware acceleration status;
- Master connection;
- subscription state;
- clock synchronization state;
- cue transitions;
- generation changes;
- major corrections;
- Master loss/reconnection;
- event firing;
- event execution failures;
- console commands;
- program mismatch;
- duplicate player identity.

Logging levels should include typical Rust levels:

```text
error
warn
info
debug
trace
```

Avoid high-frequency per-frame logging outside trace/debug diagnostics.

---

# 72. Event Logging

When an event fires, log enough to diagnose it:

```text
player=slave3
cue=main
cue_time=12.500
event=dim-ambient
type=process
```

For process completion:

```text
event=dim-ambient
exit=0
duration=...
```

Process completion logging must be asynchronous and must not block playback.

For UDP:

```text
event=lighting-cue
target=192.168.1.80:5000
bytes=...
```

---

# 73. Security

This is primarily an installation LAN tool, not an Internet service.

Nevertheless:

- console should be disabled by default;
- console bind address should be configurable;
- avoid automatically binding control services publicly;
- validate all network packet lengths/types;
- validate configuration-derived paths and payloads;
- do not execute arbitrary network-supplied shell commands;
- only configured local process events may launch commands.

Leave room for optional console authentication.

The Master synchronization protocol itself need not initially implement cryptographic authentication unless deployment experience shows it is required.

---

# 74. File Paths

Player-specific media paths are local to each machine.

Relative paths should preferably be resolved relative to the program-definition file.

Example:

```toml
video = "media/right/main.mp4"
```

resolved against:

```text
/path/to/show.toml
```

This makes whole installation bundles portable.

Absolute paths should also be allowed.

---

# 75. Program Configuration Evolution

Include an explicit schema/config version.

Example:

```toml
version = 1
```

Do not rely on guessing configuration semantics from available fields.

Future versions may add:

- advanced events;
- more transport types;
- cue metadata;
- transition policy;
- output cropping;
- color controls;
- external timecode;
- scheduled external controllers.

---

# 76. Suggested Program File Example

A fuller example:

```toml
version = 1

[program]
id = "museum-show"
loop = true

[[program.cue]]
id = "intro"

[[program.cue]]
id = "main"

[[program.cue]]
id = "outro"


[aliases]
"projection-pc-01" = "master"
"projection-pc-02" = "left-wall"
"projection-pc-03" = "right-wall"


[players.master.intro]
video = "media/master/intro.mp4"

[players.master.main]
video = "media/master/main.mp4"
audio = "media/audio/main.flac"

[players.master.outro]
video = "media/master/outro.mp4"


[players.left-wall.intro]
video = "media/left/intro.mp4"
mute = true

[players.left-wall.main]
video = "media/left/main.mp4"
mute = true

[players.left-wall.outro]
video = "media/left/outro.mp4"
mute = true


[players.right-wall.intro]
video = "media/right/intro.mp4"
mute = true

[players.right-wall.main]
video = "media/right/main.mp4"
mute = true

[[players.right-wall.main.event]]
id = "dim-ambient"
at = "12.5s"
type = "process"
command = ["/opt/show/dim-lights.sh"]

[[players.right-wall.main.event]]
id = "lighting-cue-17"
at = "37.250s"
type = "udp"
host = "192.168.1.80"
port = 5000
payload = "CUE 17"

[[players.right-wall.main.event]]
id = "prepare-outro"
before_end = "2s"
type = "udp"
host = "192.168.1.81"
port = 5001
payload_hex = "01 02 03 ff"

[players.right-wall.outro]
video = "media/right/outro.mp4"
mute = true
```

This syntax is illustrative.

Adjust TOML structure if Serde or usability considerations suggest a cleaner representation, but preserve the conceptual model.

---

# 77. Potential Rust Data Model

Conceptually:

```text
ProgramConfig
    version
    program
    aliases
    players

Program
    id
    loop
    cues[]

Cue
    id

PlayerConfig
    cue mappings

PlayerCue
    video
    audio
    mute
    gain_db
    events[]

Event
    id
    timing
    action
```

Timing:

```text
EventTiming
    At(Duration)
    BeforeEnd(Duration)
```

Actions:

```text
EventAction
    Process(...)
    Udp(...)
```

Do not encode event types as loosely parsed strings throughout the implementation.

Parse configuration into strongly typed enums.

---

# 78. Potential Runtime Model

Conceptually:

```text
RuntimeState
    session_id
    generation
    program_id
    program_hash

    cue_index
    cue_id
    cue_iteration

    transport_state

    anchor_clock_ns
    anchor_cue_position_ns

    cue_duration_ns

    debug_loop
```

Transport:

```text
TransportState
    Playing
    Paused
```

Stop can be represented as paused at cue 0/time 0 rather than requiring a permanent third state.

---

# 79. Debug Loop Runtime State

Conceptually:

```text
DebugLoop
    start_ns
    end_ns
    events_enabled = false
```

Debug looping must be synchronized across all players.

The Master defines the loop boundaries in logical cue time.

Every player loops the corresponding local media region.

---

# 80. Threading / Async Model

Do not run all work on one thread.

Likely responsibilities:

```text
GStreamer/main loop

network receive/send

Master control console

subscriber management

event scheduler

low-priority process executor

possibly status/metrics
```

Tokio may be useful for control/network/event work.

GStreamer may require its own GLib main-loop integration.

Choose an integration strategy carefully.

Avoid blocking async executors with:

- media decode;
- child process waits;
- synchronous filesystem operations during playback;
- long GStreamer calls where avoidable.

Do not perform external process execution on media threads.

---

# 81. Real-Time Expectations

This is not a hard-real-time system.

Goals are:

- frame-level or near-frame-level audiovisual synchronization;
- stable long-duration playback;
- repeatable transitions;
- sufficiently precise external UDP cues;
- graceful behavior under ordinary LAN jitter.

Do not introduce real-time Linux kernel requirements unless later measurements show them necessary.

Prioritize stable clock synchronization and preloading over attempting hard-real-time scheduling.

---

# 82. Master Audio Authority

Typical installation:

```text
Master
    video
    audio -> PA system

Slaves
    video
    muted
```

The entire synchronization design should work especially well for this case.

A viewer should not see Slave imagery drifting perceptibly relative to sound produced by the Master.

---

# 83. Audio-Less Installation

The architecture must also work when:

- there is no audio;
- all players are muted;
- media has no audio.

The Master must still have a stable clock.

Do not make synchronized playback dependent on the presence of an actual audio stream.

---

# 84. Separate Audio File

When a cue maps to:

```toml
video = "video.mp4"
audio = "audio.flac"
```

both must:

- start at cue time 0;
- follow the same cue clock;
- pause/resume together;
- seek together;
- loop together where applicable;
- transition together.

If durations differ significantly, validation should warn/error.

---

# 85. GStreamer Pipeline Experiments

Before finalizing pipeline internals, create focused prototypes for:

1. ordinary local video/audio playback;
2. exact external network clock usage;
3. synchronized two-host playback;
4. gapless same-file looping;
5. gapless A->B transition;
6. playlist end -> playlist beginning;
7. external audio file synchronized with video;
8. pause/seek/preroll/release-at-clock-time;
9. hardware decode x86_64;
10. hardware decode Raspberry Pi;
11. output display selection;
12. debug subrange looping.

Do not prematurely lock the entire application around an untested `playbin3` assumption.

---

# 86. Testing Synchronization

Develop a way to measure actual synchronization, not merely query GStreamer timestamps.

Useful test methods include:

- identical test videos with frame numbers;
- filming multiple displays simultaneously using a high-frame-rate camera;
- videos alternating black/white on known frame boundaries;
- shared audio click plus visual flash;
- timestamp overlays;
- comparing pipeline position telemetry.

The implementation should eventually expose enough diagnostics to quantify:

```text
expected position
actual position
error
clock calibration
```

---

# 87. Testing Gapless Transitions

Generate deterministic media test assets where:

```text
A final frame
B first frame
```

make missed/duplicated frames visually obvious.

Test:

- same codec;
- different codecs;
- same resolution;
- resolution change;
- same audio format;
- audio format change;
- one-file loop;
- full-playlist loop.

Document limitations rather than hiding them.

---

# 88. Testing Master Failure

Test:

```text
Master running
Slaves synchronized
        |
        v
kill Master network/process
        |
        v
Slaves continue
        |
        v
restart Master
        |
        v
Slaves reconnect
        |
        v
synchronized playback restored
```

Test both:

- Master process restart;
- temporary network loss.

---

# 89. Testing Event Semantics

Explicit tests must verify:

```text
normal crossing fires once

pause before event
wait
resume
event fires when cue reaches marker

seek across event
does not fire

join after event
does not fire

loop playlist
event fires next traversal

small position correction around marker
does not double-fire

debug loop
events suppressed
```

These should be automated where practical.

---

# 90. Testing Program Hash

Verify that:

- identical semantic config produces same canonical hash;
- whitespace changes do not matter if canonical parsing is used;
- changing a media mapping changes hash;
- changing another player's event changes hash;
- changing event timing changes hash;
- a Slave with mismatched hash is detected.

---

# 91. Potential Protocol Encoding

Use a compact, strongly specified binary encoding or efficient serialized representation.

Candidates include:

- postcard;
- bincode;
- MessagePack;
- CBOR;
- custom fixed binary protocol.

Do not use human-readable TOML/JSON for high-frequency state packets unless simplicity proves more valuable than size.

Protocol packets should include:

```text
magic
version
message type
session/generation where relevant
payload
```

Packets must be safely parseable from untrusted LAN data.

---

# 92. Protocol Versioning

Every protocol message should include or imply a protocol version.

The Master must reject incompatible Slaves cleanly.

Do not silently interpret unknown packet formats.

---

# 93. State Packet Ordering

Include a monotonically increasing packet/state sequence number if useful.

This lets receivers discard:

- stale;
- reordered;
- duplicate

state snapshots.

Generation handles major discontinuities.

Sequence handles normal ordering within a generation.

---

# 94. Network Address Configuration

Master CLI should allow configurable addresses/ports.

Potential concepts:

```text
--bind
--sync-port
--clock-port
--control-port
--sync-mode
--multicast-group
--broadcast-address
```

Choose sensible defaults but do not hard-code one installation topology.

---

# 95. Slave Master Address

Slave receives explicit Master address:

```text
--master 192.168.1.40
```

Automatic discovery is not required for V1.

Explicit addressing is preferable for installations.

Discovery could be added later.

---

# 96. No Broadcast Dependency

Do not require broadcast for:

- discovery;
- synchronization;
- control.

Broadcast may exist as an optional state-distribution mode only.

The system must function fully using ordinary unicast IP.

---

# 97. Master Subscriber Registry

Maintain runtime information such as:

```text
player_id
remote address
state port
last keepalive
program hash
software/protocol version
ready state
clock status if known
```

This registry powers:

- unicast fan-out;
- installation status;
- duplicate-ID detection;
- stale participant removal.

---

# 98. Ready State

For synchronized debug releases, track:

```text
NOT_READY
PREPARING
READY
```

or equivalent.

This is primarily diagnostic.

The requested future release time remains authoritative unless the command explicitly chooses to wait for every participant.

V1 does not need complex barrier negotiation.

---

# 99. Normal Cue Preloading

The Master and Slaves already know playlist order.

They should normally prepare the next cue before EOS.

This should not require the Master to repeatedly announce filenames or last-second transition commands.

At runtime each player knows:

```text
current cue index
next cue index
local media mapping
```

and can preload appropriately.

---

# 100. Cue Transition Synchronization

Although Master EOS determines logical advancement, transitions should be planned rather than reactive.

Where possible:

1. Master knows current media duration;
2. upcoming cue is already prepared;
3. Slaves also prepare upcoming cue;
4. transition is associated with an exact logical boundary;
5. all players switch using their synchronized clocks.

Avoid designs where the Slave waits for a network packet received after the Master has already shown the first frame of the next cue.

The exact GStreamer mechanism should be established experimentally.

---

# 101. Frame Rate and Variable Frame Rate

Synchronization authority is media time, not frame count.

Frame numbers are diagnostic conveniences.

The system should not require constant-frame-rate media unless specific GStreamer transition limitations force such a restriction.

If VFR media is supported, OSD and event timing remain time-based.

---

# 102. Color / Rendering Scope

This project is a plain synchronized media player.

Do not turn it into WarpWeaver.

Out of initial scope:

- arbitrary scene graphs;
- projection surface geometry;
- projector warping;
- spatial 3D rendering;
- multi-client rendering layers;
- shader scene composition;
- advanced calibration systems.

Simple media scaling/output is in scope.

---

# 103. Out of Scope for V1

Unless needed to support the core architecture, do not initially implement:

- automatic Master election;
- automatic event failover;
- media streaming;
- content distribution between hosts;
- GUI;
- web UI;
- remote file upload;
- transcoding;
- arbitrary HTTP control API;
- cross-cue debug loops;
- cryptographic synchronization;
- cloud control;
- database;
- user accounts;
- playlist compiler language;
- frame-accurate external SMPTE/LTC input;
- distributed rendering.

These may be future extensions.

---

# 104. Suggested Executable Interface

Examples:

```text
syncplayer master show.toml
```

```text
syncplayer slave show.toml --master 192.168.1.40
```

```text
syncplayer validate show.toml
```

Potential:

```text
syncplayer info video.mp4
```

may be useful later for media probing.

Keep CLI consistent and scriptable.

---

# 105. Startup Example — Master

```text
syncplayer master show.toml \
    --player-id master \
    --fullscreen \
    --display HDMI-A-1 \
    --sync-mode unicast \
    --control-port 2323
```

Expected startup:

```text
load config
resolve player ID
probe media
validate
initialize GStreamer
choose clock
expose network clock
open sync/control sockets
initialize program
prepare cue 0
start program or remain paused according to CLI policy
```

Whether Master auto-starts or starts paused should be configurable.

For unattended installations, auto-start is likely useful.

---

# 106. Startup Example — Slave

```text
syncplayer slave show.toml \
    --master 192.168.1.40 \
    --player-id right-wall \
    --fullscreen \
    --mute
```

Expected startup:

```text
load config
resolve player ID
probe media
validate
contact Master
validate program hash
initialize network clock
receive current state
load current cue
seek/preroll
join synchronized playback
```

---

# 107. Recommended Development Phases

## Phase 1 — Local Playback

Implement:

- Rust CLI;
- GStreamer playback;
- video;
- audio;
- mute/gain;
- fullscreen;
- scaling;
- media probing;
- single-player playlist;
- basic gapless transitions;
- playlist looping.

No networking yet.

---

## Phase 2 — Program Model

Implement:

- TOML parser;
- player identity;
- aliases;
- local media mapping;
- validation;
- canonical program hash;
- cue-relative timeline.

---

## Phase 3 — Master/Slave Clock Prototype

Implement:

- Master clock export;
- Slave network clock;
- two-machine synchronized playback;
- measurement tools.

Do not proceed to elaborate networking until clock behavior is validated experimentally.

---

## Phase 4 — Transport Protocol

Implement:

- session ID;
- generation;
- state snapshots;
- subscription;
- keepalive;
- unicast fan-out;
- Slave reconnection;
- Master-loss free-running.

---

## Phase 5 — Synchronized Cue Transitions

Implement:

- preloading;
- exact cue boundary behavior;
- playlist loop;
- Slave transitions.

Measure actual synchronization.

---

## Phase 6 — Timed Events

Implement:

- cue-relative `at`;
- `before_end`;
- process executor;
- UDP events;
- event crossing semantics;
- event traversal tracking.

---

## Phase 7 — Installation Console

Implement:

- TCP line console;
- status;
- pause;
- play;
- stop;
- goto;
- synchronized release;
- debug loop;
- OSD.

---

## Phase 8 — Platform Hardening

Implement/test:

- Ubuntu x86_64;
- Raspberry Pi aarch64;
- hardware decode;
- multi-display selection;
- packaging/bundled runtime;
- long-duration playback.

---

# 108. Long-Duration Stability Testing

Run installations for hours/days.

Watch for:

- clock drift;
- memory growth;
- GStreamer resource leaks;
- cumulative playlist-transition error;
- reconnect behavior;
- child-process zombies;
- socket leakage;
- frame drops;
- decoder instability.

The system is intended for unattended installations and must survive repeated looping.

---

# 109. Graceful Shutdown

On termination:

- stop accepting console clients;
- stop subscriber tasks;
- stop event scheduling;
- terminate or detach event workers cleanly;
- set GStreamer pipeline to NULL;
- release sinks;
- close sockets;
- log shutdown.

Do not leave external child processes as accidental zombies.

Configured installation scripts may intentionally outlive the player; distinguish detached behavior if needed.

---

# 110. Signals

Handle at least normal Unix termination signals gracefully:

```text
SIGINT
SIGTERM
```

Reload-on-SIGHUP is optional future work.

---

# 111. Configuration Reload

Live program-file reload is not required initially.

A future reload system should behave as an explicit new generation/program state rather than silently changing configuration underneath current playback.

Do not use filesystem-watch auto-reload in V1.

---

# 112. Design Invariant Summary

The following invariants should remain true throughout implementation.

### Invariant 1

There is one authoritative Master.

### Invariant 2

Every player decodes its own local media.

### Invariant 3

Media is not streamed over the synchronization network.

### Invariant 4

Steady-state synchronization is clock-based, not repeated-seek-based.

### Invariant 5

The Master owns logical cue progression.

### Invariant 6

Program definitions are shared across hosts.

### Invariant 7

Media paths are host-specific.

### Invariant 8

Timed events are host-specific.

### Invariant 9

Authored event times are cue-relative.

### Invariant 10

Program authors do not maintain cumulative absolute timestamps.

### Invariant 11

State packets are complete snapshots.

### Invariant 12

Unicast fan-out is the normal network mode.

### Invariant 13

Multicast and broadcast are optional deployment choices.

### Invariant 14

Temporary Master failure does not immediately stop Slave playback.

### Invariant 15

External process execution must never block playback.

### Invariant 16

Debug operations do not modify the authored program.

### Invariant 17

Debug loops suppress external events by default.

### Invariant 18

Program configuration mismatches are detectable.

---

# 113. Architectural Non-Goals

Do not over-engineer this into a distributed media orchestration framework.

The core should remain understandable:

```text
shared program
      +
host-local media
      +
Master clock
      +
authoritative transport state
      =
synchronized playback
```

Timed events and the installation console extend this model but should not obscure it.

---

# 114. Implementation Philosophy

Prefer simple deterministic mechanisms over elaborate distributed consensus.

Examples:

Prefer:

```text
one Master
```

over:

```text
leader election
```

Prefer:

```text
explicit Master IP
```

over:

```text
network discovery requirement
```

Prefer:

```text
local files
```

over:

```text
media distribution
```

Prefer:

```text
complete state snapshots
```

over:

```text
fragile incremental command history
```

Prefer:

```text
host-specific events
```

over:

```text
automatic distributed event ownership
```

Prefer:

```text
future clock release timestamp
```

over:

```text
GO packet arrival time
```

Prefer:

```text
preload next cue
```

over:

```text
open next file after EOS
```

---

# 115. First Prototype Goal

The first meaningful network prototype should demonstrate:

```text
PC A — Master
PC B — Slave

both have local copies of a 60-second test video

Master exports its playback clock

Slave uses Master-derived network clock

Master starts at a future synchronized timestamp

both displays show the same frame sequence

pause
resume
goto
playlist loop

remain visually synchronized
```

Do not attempt the complete configuration/event/console architecture before proving this synchronization primitive.

---

# 116. Second Prototype Goal

Demonstrate:

```text
Master:
    master-version.mp4

Slave:
    slave-version.mp4
```

Both files:

```text
same duration
same frame rate
different visual content
```

Verify:

- synchronized start;
- synchronized long-duration playback;
- synchronized cue transitions;
- synchronized playlist loop.

This validates the actual target use case rather than merely mirrored playback.

---

# 117. Third Prototype Goal

Demonstrate a delicate installation scenario:

```text
cue main

debug goto 00:00:40
pause
loop 5
```

All displays repeatedly play:

```text
40 s -> 45 s
```

in synchronization while external events remain suppressed.

This prototype validates the installation-debug use case.

---

# 118. Fourth Prototype Goal

Demonstrate host-specific event execution:

```text
Slave 3 event @ 12.5 s
    |
    +--> UDP packet to test listener

Slave 2 event @ 15 s
    |
    +--> low-priority shell script
```

Verify:

- correct cue-relative timing;
- no duplicate firing;
- pause semantics;
- seek semantics;
- playlist-loop semantics.

---

# 119. Documentation Requirements

The repository should eventually contain:

```text
README.md
BLUEPRINT.md
docs/
```

Document at least:

- installation;
- supported platforms;
- GStreamer dependencies/runtime strategy;
- program configuration;
- Master CLI;
- Slave CLI;
- validation;
- timed events;
- installation console;
- networking;
- troubleshooting;
- hardware acceleration diagnostics.

Include example program files.

---

# 120. Final Architectural Statement

The project should be understood as:

> A portable Rust/GStreamer playback engine for synchronized installations in which every host renders independent local media against one Master-derived timeline.

The Master is not a video server.

It is the authority for:

```text
time
transport
cue progression
installation control
```

Each host is independently responsible for:

```text
local media decoding
local display output
local audio behavior
local host-specific timed events
```

The shared program connects those responsibilities through logical cue identities.

The network synchronizes state and clocks, not pixels.

That separation is the central architectural idea of the project and should remain intact as the implementation evolves.
