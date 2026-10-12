# Full shared program configuration

Status: Complete

Implement the configuration TODO as one milestone split into independently testable slices. External
audio, cue-local playback settings, and timed events are represented and validated here; their
runtime engines remain later milestones.

## Slices

- [x] Add the strict versioned program model, typed events, exact event times, and canonical hash.
- [x] Resolve player identity, aliases, default mappings, and program-relative paths.
- [x] Add local-player media and event validation through `syncplayer validate`.
- [x] Replace the prototype media/playlist CLI with `master <PROGRAM>` and `slave <PROGRAM>`, add
      examples, and update user documentation.

## Decisions

- Make a clean cutover: remove `--media`, `--playlist`, `--loop`, and the prototype playlist TOML.
- An explicit `--player-id` is final. Otherwise use the hostname and apply an exact alias lookup.
- Merge default and player cue mappings field by field; an explicit player event list replaces the
  inherited list.
- Hash the complete portable semantic configuration with SHA-256. Resolved machine-local absolute
  paths are not hashed.
- Probe only the resolved player's files; other player paths need not exist on this machine.
- Reject playback startup when a selected mapping uses a configured feature whose runtime engine
  has not yet been implemented, rather than silently omitting it.

## Completion checks

Run:

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
```
