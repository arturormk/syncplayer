# INC-0002 — Current URI race panics during cue handoff

- Status: Mitigated
- Date: 2026-10-13
- Affected component: Master media pipeline and transition telemetry
- Severity: Development

## Context and trigger

The affected P1 local-playback implementation routed every program through separate
`uridecodebin3` video and audio branches. Each branch received its next URI from an
`about-to-finish` callback so it could prepare the next logical cue before the current cue reached
EOS. Embedded-audio programs now use the coordinated `playbin3` engine, while the separate branch
engine remains available for external-audio mappings.

At the same time, the main monitoring thread sampled pipeline position once per second. It read the
video decoder's `current-uri` GObject property to map the queried item-local position back to a
logical cue. Master and Slave EOS diagnostics also read the same property.

The failure was reported on a Master while it switched from the first video to the second video in
a playlist. The exact physical video and audio sinks used in that report were not captured.

A headless reproduction used the repository's local test program:

```console
RUST_LOG=syncplayer=debug \
  target/debug/syncplayer master __videos/program.toml \
  --player-id tweek \
  --video-sink fakesink \
  --audio-sink fakesink \
  --clock system \
  --bind 127.0.0.1:46192 \
  --clock-port 46193
```

## Symptom

During the first-to-second cue handoff, the Master process terminated with:

```text
thread 'main' panicked at glib-0.22.9/src/value.rs:898:42:
Invalid UTF-8: Utf8Error { valid_up_to: 1, error_len: Some(1) }
```

The panic originated in GLib's conversion of a `gchar*` GValue to Rust `&str`. It was a process-wide
failure rather than a recoverable GStreamer bus error.

## Impact

Any affected player could terminate at a cue transition. On the Master this stops the authoritative
playback process, clock provider, state publisher, and control server together. A corresponding
failure on a Slave would remove that display from the installation until the process restarted.

The timing-dependent failure also made ordinary transition testing unreliable: the media branches
could successfully queue the next cue, then telemetry could crash the process while observing the
handoff.

## Investigation

1. The panic site in `glib::value` was inspected. Line 898 converts a GObject string pointer with
   `CStr::to_str()` and panics when the pointed-to bytes are not valid UTF-8.
2. Every runtime GObject string read was reviewed. The only reads performed repeatedly by the main
   monitor during a cue transition were `uridecodebin3.current-uri` queries in position telemetry
   and EOS diagnostics.
3. The transition callback was confirmed to set the next `uri` from a GStreamer streaming thread.
   GStreamer's `about-to-finish` documentation explicitly warns that the callback can execute at
   the same time as application-thread code.
4. All `current-uri` reads were removed. Cue identity and diagnostic URIs now come from the
   mutex-protected `PlaylistCursor` and immutable Rust-owned playback sequence.
5. A real-time headless Master run queued the second video and audio URIs, crossed the following
   telemetry sample, and did not reproduce the UTF-8 panic.

The headless run separately observed an early pipeline EOS approximately 21 ms before the
authoritative cue boundary on GStreamer 1.20. That behavior is not evidence for or against the
string race and is not treated as part of this incident. It requires its own diagnosis if it also
occurs with the laptop and Raspberry Pi's physical sinks.

## Root Cause

The monitor treated `uridecodebin3.current-uri` as a stable cross-thread source of transport
identity. That assumption was unsafe during gapless handoff: `about-to-finish` changes the URI from
a streaming thread while the main thread can query the read-only current URI.

The observed panic and the successful mitigation support a concurrent property-read failure. The
precise lifetime violation inside the installed GStreamer/GLib implementation remains an inference
because the original report did not include a native backtrace or memory-sanitizer trace.

Independently of the binding behavior, a mutable framework-owned display string was the wrong
authority for logical cue identity. Syncplayer already owns that identity in its playlist cursor.

## Resolution

- Position telemetry no longer reads `current-uri`.
- It obtains the current item index from the mutex-protected playlist cursor and clones the
  corresponding URI from the immutable Rust playback sequence.
- Transition suppression compares the cursor's item with the clock-derived expected item.
- Master and Slave EOS diagnostics use the cursor and Rust-owned media path instead of querying a
  GObject string.
- The URI reverse-lookup helper was removed because runtime identity no longer depends on a
  framework property or on media URIs being unique within a playlist.

## Verification

- A 24-second headless Master run with `fakesink` for both audio and video crossed the first cue's
  `about-to-finish` callbacks and the subsequent telemetry samples without an invalid UTF-8 panic.
- Logs confirmed that both second-cue URIs were queued.
- All 47 unit tests pass with `cargo test`.
- Formatting passes with `cargo +stable fmt --all -- --check`.
- Clippy passes with `cargo +stable clippy --all-targets -- -D warnings`.
- `git diff --check` passes.

This verification used test sinks. The incident remains Mitigated until the laptop Master and
Raspberry Pi Slave complete repeated playlist transitions with their real sinks.

## Prevention and guardrails

- Do not use mutable GStreamer string properties as authoritative runtime identity when a streaming
  callback can update the associated object concurrently.
- Keep logical program, cue, and traversal identity in Rust-owned state protected by the existing
  synchronization primitives.
- Use framework properties for diagnostics only when their ownership and cross-thread read
  semantics are known to be safe.
- Include several consecutive cue transitions and playlist wrap in both real-sink and headless
  smoke tests.
- Capture `RUST_BACKTRACE=1`, GStreamer version, sink factories, and the last transition logs if a
  similar panic recurs.
- Track early EOS or failure to advance separately; do not conflate presentation handoff failures
  with this property-read crash.

## References

- Code: [`src/main.rs`](../../src/main.rs)
- Design: [`docs/BLUEPRINT.md`](../BLUEPRINT.md), sections 14–19 and 85
- GStreamer: [`playbin3` `about-to-finish` documentation](https://gstreamer.freedesktop.org/documentation/playback/playbin3.html)
- Related incidents: [`INC-0001`](INC-0001-forced-system-clock-stalls-master-after-delayed-startup.md),
  [`INC-0003`](INC-0003-independent-decode-branches-break-preroll-and-handoff.md)
- Commit or pull request: not yet recorded

## Tags

- `gstreamer`
- `glib`
- `playlist`
- `transition`
- `telemetry`
- `concurrency`
- `panic`
