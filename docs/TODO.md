# MVP and v1.0 release checklist

Last reviewed: 2026-10-16.

Syncplayer is feature-complete for the MVP described by [`BLUEPRINT.md`](BLUEPRINT.md). The core
configuration, playback, synchronization, state protocol, event, diagnostic, and installation
control layers are implemented. No additional playback feature is required before an MVP release.

The remaining work is release qualification and deployment hardening. Optional/future work and the
explicit V1 exclusions in blueprint section 103 are non-blocking.

## Current release assessment

- **MVP feature scope:** complete.
- **Automated checks:** 71 tests pass on the current revision; CI also checks formatting, all
  targets, and Clippy.
- **Endurance:** in progress. On 2026-10-16 the current physical run passed 402 playlist iterations
  and continued playing. It required one corrective Slave resynchronization after roughly 5.5
  hours while the laptop Master was also exposed to screen locking and desktop power management.
  This is encouraging evidence, not a completed result: stop/shutdown and final log analysis still
  need to be recorded.
- **v1.0 readiness:** release candidate, pending the P0 gates below.

## P0 — v1.0 release gates

### Finish and evaluate the endurance run

- [ ] Let the current run reach its chosen end condition and verify graceful shutdown.
- [ ] Record revision, host/platform details, media and commands, elapsed time, playlist iteration
  count, disconnect/recovery count, and any warnings or corrections.
- [ ] Archive or summarize the Master/Slave logs, diagnostics CSV, and resource CSVs in a dated
  qualification record.
- [ ] Explain or fix the observed memory profile before release. At the 402-iteration checkpoint,
  the available Master resource log had reached roughly 1.36 GiB RSS (about 1.40 GiB peak) after
  approximately 11 hours. It appeared broadly flat over the last four hours, and thread/file
  descriptor counts were stable, but the earlier growth is too large to ignore without a baseline
  comparison and a documented plateau.
- [ ] Review diagnostic outliers rather than judging only visible playback. Confirm that large
  errors correspond to expected startup/recovery events or investigate them.

Passing criterion: playback and shutdown remain correct, no unexplained error/recovery loop occurs,
and memory, threads, descriptors, and I/O are bounded rather than growing per playlist iteration.
The practical installation target is a 12-hour operating day without a crash and no more than one
corrective resynchronization per hour, with one per two hours or less preferred. Initial
synchronization is never counted as a recovery.

### Complete synchronization and recovery qualification

- [ ] Record sustained multi-host visual inspection and analyze `output.csv` with
  `tools/analyze-diagnostics.py`; filming displays is optional rather than a v1.0 gate.
- [ ] For at least ten cold starts and a 60-minute continuous run, record signed mean, RMS,
  absolute-error percentiles, worst samples, rate error, disconnects, and recoveries for every
  player. At 25 fps, use 40 ms as the one-frame reference threshold.
- [ ] Exercise same-file, A→B, and playlist-wrap transitions with equal-duration host-local
  renditions.
- [ ] Exercise the supported codec, resolution, frame-rate, and audio-format transition matrix.
- [ ] Test TCP/state/clock network loss separately, Master restart, late Slave join, and one Slave
  restarting without disturbing the others.
- [ ] Test CPU/I/O pressure and an injected sustained-drift recovery.
- [ ] Test a real audio-backed Master clock and automatic `CLOCK_LOST` recovery.
- [ ] Record measured transition limits and either confirm or revise the duration and drift
  thresholds.

Use [`prototypes/clock-sync.md`](prototypes/clock-sync.md) for the procedure and acceptance
criteria. Add dated result documents beside it. Position telemetry measures synchronization and
sustained stalls; visual inspection remains the check for rendering glitches that position samples
cannot observe.

### Qualify the promised platforms and output paths

- [ ] Run the final 12-hour production-topology soak on four Raspberry Pis: one Master and three
  Slaves. Apply the same crash, recovery-frequency, resource, and diagnostics criteria used above.
- [ ] Qualify Ubuntu x86-64 with the intended display server, sinks, decoder, and audio device.
- [ ] Qualify Raspberry Pi AArch64 hardware decoding and the intended Wayland or direct KMS/GL
  output path.
- [ ] Verify fullscreen and display/connector selection on the actual deployment topology.
- [ ] Record selected decoders/sinks, negotiated memory path, rendered/dropped frames, and known
  platform limitations.

Only output paths actually tested should be described as supported in the v1.0 release notes.

### Choose and document the supported deployment

- [ ] Decide the v1.0 delivery unit: a native package, self-contained runtime directory/AppImage,
  or a clearly supported host-runtime dependency set.
- [ ] Document reproducible installation and upgrade steps for each supported platform.
- [ ] Provide an unattended supervisor recipe (for example, a systemd unit) with restart policy,
  working directory, environment, user permissions, and graceful stop behavior.
- [ ] Test cold boot, automatic start, process restart after watchdog exit, and clean SIGTERM
  shutdown using that recipe.

The source-build quickstart is useful for development, but by itself does not satisfy the
blueprint's deployment goal of a predictable GStreamer runtime.

## P1 — small post-MVP usability candidates

These are useful but do not block the unattended auto-start MVP:

- [ ] Add an explicit `--start-paused` Master option if installations need to inspect a prepared
  system before the first `play`. Auto-start is already implemented and is the normal unattended
  behavior.
- [ ] Add `syncplayer info <MEDIA>` if standalone media probing proves useful; validation already
  reports the information needed to run a program.
- [ ] Add console authentication before exposing it beyond a trusted installation LAN. The console
  remains opt-in and should bind to loopback by default in operator recipes.

## Implemented MVP foundation

- [x] Versioned shared program model, player identity/aliases, local mappings, canonical hash, and
  full media/program validation.
- [x] GStreamer video, embedded/external audio, mute/gain, scaling/fullscreen/display selection,
  clock policy, decoder controls, and prepared cue handoff.
- [x] Master clock export, Slave clock calibration, future-clock release, late join, reconnect, and
  free-running playback during control/state loss.
- [x] Production subscription registry with compatibility, duplicate-ID, program-hash, capability,
  duration, readiness, and liveness checks.
- [x] Version 9 bounded binary TCP control and unicast UDP state protocols with complete snapshots,
  ordering checks, configurable cadence, and stale-session/generation rejection.
- [x] Authoritative cue progression, playlist wrap, local early-EOS handling, clock-loss recovery,
  drift detection, slew/hard repair, and frame-stall watchdog.
- [x] Cue-relative process/UDP events with traversal identity, duplicate suppression, asynchronous
  execution, and debug-loop suppression.
- [x] Installation console with status, diagnostics, pause/play/stop/goto, in-cue loop, and OSD.
- [x] Centralized telemetry/CSV logging, resource sampling, startup stack reporting, periodic clock
  and rendered/dropped-frame health, and incident records.
- [x] Graceful SIGINT/SIGTERM shutdown of pipelines, network tasks, event workers, watchdogs, and
  sockets.
- [x] Example programs, media preparation tools, native quickstart, physical test launchers, and CI.

## Deferred beyond v1.0

- Multicast/broadcast state distribution.
- Runtime program switching or live reload.
- Targeted OSD and scheduled ahead-of-time external-controller events.
- Automatic Master election, event failover, GUI/web UI, media transfer, cross-cue debug loops,
  arbitrary HTTP control, user accounts, and the other exclusions in blueprint section 103.
