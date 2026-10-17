# MVP and v1.0 release checklist

Last reviewed: 2026-10-17.

Syncplayer is feature-complete for the MVP described by [`BLUEPRINT.md`](BLUEPRINT.md). The core
configuration, playback, synchronization, state protocol, event, diagnostic, and installation
control layers are implemented. No additional playback feature is required before an MVP release.

The remaining work is release qualification and deployment hardening. Optional/future work and the
explicit V1 exclusions in blueprint section 103 are non-blocking.

## Current release assessment

- **MVP feature scope:** complete.
- **Automated checks:** 71 tests pass on the current revision; CI also checks formatting, all
  targets, and Clippy.
- **Endurance:** the four-Raspberry-Pi production-topology run was observed for 17.62 hours and 645
  playlist iterations; the copied diagnostics/resources cover 17.16 hours and 629 iterations.
  It produced excellent steady-state results: no corrective recovery, no
  steady-state disconnect, no unhealthy clock, no process exit after successful startup, bounded
  resources, and no valid or pairwise diagnostic error above 4.5 ms. The first player04 launch did
  terminate with an unexplained SIGSEGV before the operator manually restarted the unchanged
  program. The run also exposed a launcher SIGPIPE during Master shutdown; that defect has since
  been fixed and requalified with zero Master and Slave pipeline statuses. Player04 had also
  continued free-running after Master loss. See
  [`reports/2026-10-17-four-raspberry-pi-soak-test.md`](reports/2026-10-17-four-raspberry-pi-soak-test.md).
- **v1.0 readiness:** release candidate, pending the P0 gates below.

## P0 — v1.0 release gates

### Close the remaining endurance qualification gaps

- [x] Complete an overnight production-topology run. Four Raspberry Pis were observed for 17.62
  hours and 645 playlist iterations; the copied analyzable artifacts cover 17.16 hours and 629
  iterations, and the successful processes stayed alive throughout playback.
- [x] Record commands, elapsed time, playlist iterations, disconnect/recovery counts, warnings,
  synchronization distributions, GStreamer messages, and resource behavior in a dated
  qualification record.
- [x] Archive or summarize the Master/Slave logs, diagnostics CSV, and resource CSVs. The report
  also identifies the four stale `sampling.csv` files and excludes them from its conclusions.
- [x] Analyze the complete centralized diagnostics rather than relying on visible playback. Across
  227,615 samples, individual RMS error was 0.099–0.133 ms, pairwise p99 was at most 0.384 ms,
  the worst valid pairwise estimate was 4.414 ms, and no sample exceeded the 40 ms one-frame
  reference threshold.
- [x] Establish a bounded resource profile for the four-Pi topology. Final RSS was
  133.4–141.6 MiB; only 0.23–0.31 MiB was added from hour 8 to hour 17; thread, descriptor, and
  virtual-memory counts remained bounded. The earlier approximately 1.36 GiB laptop-Master result
  did not recur on this topology and must be resolved while qualifying Ubuntu x86-64 for v1.0.
- [ ] Record the tested Git revision, exact Pi models, OS/GStreamer versions, media checksums, and
  display/audio topology. The launch commands and Raspberry Pi OS Lite/Debian 13 family are known,
  but the full reproducibility manifest is incomplete.
- [x] Fix and requalify launcher-mediated shutdown. Both launchers now keep `tee` alive during
  SIGINT. The Master reached `shutdown complete` and returned `syncplayer=0 tee=0`; a Slave
  retrying an unavailable Master also returned `syncplayer=0 tee=0` on Ctrl-C.
- [ ] Reproduce or otherwise explain the first player04 startup SIGSEGV. The failed PID 936 left no
  core dump, and the manual retry overwrote its local logs. The second process used the unchanged
  `program.toml` and then ran for the full soak.
- [x] Change the physical-test launchers to preserve each invocation in a unique timestamped log
  directory so a retry cannot destroy crash evidence. They also enable traditional core dumps,
  record the host core policy, preserve matching Ubuntu Apport reports, and export matching
  `systemd-coredump` entries when available.

Passing criterion: playback and shutdown remain correct, no unexplained crash or error/recovery
loop occurs, and memory, threads, descriptors, and I/O are bounded rather than growing per
playlist iteration.
The practical installation target is a 12-hour operating day without a crash and no more than one
corrective resynchronization per hour, with one per two hours or less preferred. Initial
synchronization is never counted as a recovery. The 2026-10-16/17 run passes the sustained-playback,
synchronization, and resource portions but does not close this gate because startup crashed once
and remains unexplained. Launcher-mediated shutdown has been requalified successfully.

### Complete synchronization and recovery qualification

- [x] Record sustained multi-host visual inspection and analyze `output.csv` with
  `tools/analyze-diagnostics.py`; the four-Pi soak remained visually flawless and its full
  distributions are in the dated report. Filming displays remains optional rather than a v1.0 gate.
- [ ] For at least ten cold starts and a 60-minute continuous run, record signed mean, RMS,
  absolute-error percentiles, worst samples, rate error, disconnects, and recoveries for every
  player. At 25 fps, use 40 ms as the one-frame reference threshold.
- [x] Exercise A→B transitions and playlist wrap with equal-duration host-local renditions. The
  three-cue soak completed 629 wraps without a steady-state failure.
- [ ] Exercise the focused same-file transition case.
- [ ] Exercise the supported codec, resolution, frame-rate, and audio-format transition matrix.
- [ ] Test TCP/state/clock network loss separately, Master restart, late Slave join, and one Slave
  restarting without disturbing the others. The final shutdown supplied limited supporting
  evidence—player04 remained free-running for at least approximately 40 seconds after the Master
  exited—but did not exercise separated failure modes or successful reconnection.
- [ ] Test CPU/I/O pressure and an injected sustained-drift recovery.
- [ ] Test a real audio-backed Master clock and automatic `CLOCK_LOST` recovery.
- [ ] Record measured transition limits and either confirm or revise the duration and drift
  thresholds.

Use [`prototypes/clock-sync.md`](prototypes/clock-sync.md) for the procedure and acceptance
criteria. Add dated result documents beside it. Position telemetry measures synchronization and
sustained stalls; visual inspection remains the check for rendering glitches that position samples
cannot observe.

### Qualify the promised platforms and output paths

- [x] Run at least 12 hours on the production topology of four Raspberry Pis: one Master and three
  Slaves. The 2026-10-16/17 run supplied 17.62 observed hours and 17.16 hours of archived,
  analyzed steady-state evidence.
- [x] Fix the Master's launcher-mediated SIGPIPE on Ctrl-C and verify clean shutdown. The Master
  logged `shutdown complete` and both Master and Slave launcher checks returned
  `syncplayer=0 tee=0`.
- [ ] Close the remaining production-topology soak exception by reproducing or explaining the
  player04 startup SIGSEGV. Until then, the run is a steady-state pass rather than final platform
  acceptance.
- [ ] Qualify Ubuntu x86-64, including the intended NUC-class deployment, with the intended display
  server, sinks, decoder, and audio device. Use available PC hardware and a mixed-platform
  synchronized run where possible; a representative multi-NUC endurance soak cannot currently be
  performed because enough machines are unavailable, and that limitation must be recorded in the
  v1.0 qualification record and release notes.
- [ ] Qualify Raspberry Pi AArch64 hardware decoding and the intended Wayland or direct KMS/GL
  output path.
- [ ] Verify fullscreen and display/connector selection on the actual deployment topology.
- [ ] Record selected decoders/sinks, negotiated memory path, rendered/dropped frames, and known
  platform limitations. The soak confirms `v4l2h264dec` hardware decode, automatic `glimagesink`
  output, and 20–36 sparse QoS-drop messages per host, but the complete platform manifest and
  negotiated memory path remain to be recorded.

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
