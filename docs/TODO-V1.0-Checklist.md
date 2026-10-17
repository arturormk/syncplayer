# Syncplayer v1.0 release checklist

Last reviewed: 2026-10-17.

Syncplayer is an MVP-complete v1.0 release candidate. No additional playback, synchronization,
transport, event, console, diagnostic, or shutdown feature is currently required for v1.0. The
remaining work is release qualification, supported-platform definition, deployment hardening, and
release housekeeping. Optional features and the explicit v1 exclusions in
[`BLUEPRINT.md`](BLUEPRINT.md) remain out of scope.

This checklist turns the broader release gates in [`TODO.md`](TODO.md) into the concrete path to
the v1.0 tag. If qualification exposes a defect, add the resulting fix and regression test here;
otherwise, do not expand the feature set before release.

## Current status

- [x] Core MVP feature scope is implemented.
- [x] The current revision passes all 71 automated tests.
- [x] The current revision has a successful release build and green CI on `main`.
- [x] No known source `TODO`, `FIXME`, or unimplemented production path blocks v1.0.
- [x] Graceful SIGINT/SIGTERM shutdown and playback-stall watchdog behavior are implemented.
- [ ] Physical qualification and release preparation below are complete.

The 2026-10-16/17 four-Raspberry-Pi soak is documented in
[`reports/2026-10-17-four-raspberry-pi-soak-test.md`](reports/2026-10-17-four-raspberry-pi-soak-test.md).
It is a strong 17.62-hour observed steady-state pass with 17.16 hours of copied diagnostic and
resource evidence, but it is not final acceptance because player04's first launch segfaulted
without a retained core or log and Ctrl-C ended the Master with SIGPIPE.

## 1. Close the remaining four-Raspberry-Pi soak items

The run used the intended production topology of one Master and three Slaves. A run does not pass
merely because the displays continue advancing.

- [ ] Record the tested Git revision, Pi models, OS and GStreamer versions, media checksums, and
  display/audio topology.
- [x] Record complete launch commands. The Master used
  `tools/test-master.sh --debug 192.168.1.18:46000 __stuff/videos/program.toml`; Slaves used
  `tools/test-slave.sh 192.168.1.18:46000 __stuff/videos/program.toml`.
- [x] Record elapsed time, playlist iterations, disconnects, corrective recoveries, watchdog exits,
  and warnings for every player. The successful processes were observed for 17.62 hours and 645
  iterations; the copied evidence covers 17.16 hours and 629 iterations with no corrective
  recovery, unhealthy clock, watchdog exit, or steady-state disconnect.
- [ ] Stop every process normally and verify that each log ends with `shutdown complete`. The
  Master currently fails this item (`syncplayer=141 tee=130` after Ctrl-C). Players 02, 03, and 04
  all returned `syncplayer=0`, so the Slave process-exit portion passes; retain explicit
  `shutdown complete` evidence when rechecking the Master launcher.
- [x] Archive or summarize the Master/Slave logs, diagnostics CSV, and resource CSVs in a dated
  qualification record.
- [x] Run `tools/analyze-diagnostics.py` on the complete diagnostics CSV and review signed mean,
  RMS, absolute-error percentiles, worst samples, rate error, disconnects, and recoveries for every
  player.
- [x] Confirm that diagnostic outliers correspond to expected startup, transition, or recovery
  activity; investigate unexplained outliers.
- [x] Confirm that RSS, threads, file descriptors, and I/O are bounded rather than growing per
  playlist iteration.
- [x] Check for recurrence of the previously observed high memory profile. It did not recur on the
  four-Pi topology: final RSS was 133.4–141.6 MiB and late-run growth flattened to 0.23–0.31 MiB
  over the final nine hours. The earlier approximately 1.36 GiB laptop-Master profile must still be
  addressed if Ubuntu x86-64 is included in the supported v1.0 platforms.
- [ ] Confirm no crash, unrecovered freeze, or unexplained reconnect/recovery loop occurred.
- [ ] Reproduce or explain player04's first-start SIGSEGV. PID 936 connected briefly, exited 139,
  and was manually replaced 46.6 seconds later. The replacement used the same unchanged program
  file and ran normally, but its launch overwrote the failed invocation's local evidence.
- [ ] Preserve every test invocation in a unique timestamped directory and enable crash capture
  before repeating startup qualification.

Minimum acceptance target: at least 12 hours, no crash, clean shutdown, and no more than one
corrective resynchronization per hour; one per two hours or less is preferred. Initial
synchronization is not a recovery. The run meets the duration, steady-state synchronization,
recovery-frequency, and resource requirements, but the startup crash and failed Master shutdown
keep this section open.

## 2. Complete focused synchronization and recovery qualification

Ordinary soak playback does not replace deliberate failure testing. Record the following using the
procedure and metrics in [`prototypes/clock-sync.md`](prototypes/clock-sync.md):

- [ ] Complete at least ten cold starts and record the synchronization result for every player.
- [ ] Complete and analyze a continuous 60-minute real-sink run.
- [x] Exercise A-to-B and playlist-wrap transitions. The three-cue program completed 629 wraps.
- [ ] Exercise the focused same-file transition case.
- [ ] Exercise the codec, resolution, frame-rate, audio-format, embedded-audio, and external-audio
  combinations that v1.0 will claim to support.
- [ ] Test TCP control loss, UDP state loss, and UDP clock loss separately. At final shutdown,
  player04 did remain free-running for at least approximately 40 seconds after Master loss, but
  that combined event does not replace the separated tests or demonstrate reconnection.
- [ ] Test Master restart, late Slave join, and one Slave restarting without disturbing the others.
- [ ] Test CPU and I/O pressure.
- [ ] Inject sustained drift and verify one bounded correction followed by convergence.
- [ ] Test an audio-backed Master clock and automatic `CLOCK_LOST` recovery.
- [ ] Confirm or revise the documented duration tolerance, drift thresholds, and transition limits
  from the measurements.
- [ ] Review the mitigated incident reports and mark each resolved only when its stated real-sink
  verification has been satisfied.

At 25 fps, use 40 ms as the one-frame reference threshold. A healthy 60-minute run should not
require a drift-triggered correction.

## 3. Define and qualify the supported v1.0 platforms

Only configurations actually tested should be described as supported.

- [ ] Decide whether v1.0 supports only the qualified Raspberry Pi deployment or also Ubuntu
  x86-64.
- [ ] For Raspberry Pi AArch64, record the supported OS, GStreamer version, decoder, negotiated
  memory path, video/audio sinks, display path, and known limitations.
- [ ] Verify hardware decoding, fullscreen behavior, and connector/display selection on the actual
  Raspberry Pi deployment.
- [ ] If Ubuntu x86-64 is included, qualify its intended display server, decoder, sinks, audio
  device, fullscreen behavior, and display selection to the same standard.
- [ ] Record rendered/dropped-frame statistics for every supported output path.
- [ ] Remove or clearly label claims about configurations that were not qualified for v1.0.

## 4. Finish deployment support

This is the remaining implementation workstream known before qualification. The development
quickstart alone is not the supported unattended deployment story.

- [ ] Choose the v1.0 delivery unit: native package, self-contained runtime directory/AppImage, or
  a precisely documented host-runtime dependency set.
- [ ] Document reproducible installation and upgrade steps for every supported platform.
- [ ] Provide an unattended supervisor recipe, such as a systemd unit, including the service user,
  working directory, environment, permissions, restart policy, and graceful-stop behavior.
- [ ] Test cold boot and automatic start using the documented recipe.
- [ ] Test automatic restart after a watchdog exit with status 70.
- [ ] Test a clean SIGTERM shutdown through the supervisor.
- [ ] Verify that a new machine can be provisioned from the documentation without relying on an
  undocumented GStreamer plugin or machine-local setting.

## 5. Perform release housekeeping

- [ ] Resolve or explicitly defer the three open dependency-update pull requests after reviewing
  their compatibility and CI results.
- [ ] Change the Cargo package version from `0.1.0` to `1.0.0` and update `Cargo.lock`.
- [ ] Replace the Cargo package's experimental description with the final release description.
- [ ] Update `README.md`, `SECURITY.md`, and `CONTRIBUTING.md` so they no longer describe the project
  as an experimental prototype or say that no supported release exists.
- [ ] Write v1.0 release notes containing supported platforms/output paths, installation steps,
  operational expectations, protocol compatibility, known limitations, and qualification results.
- [ ] Confirm that all examples and documented commands work with the release artifact.
- [ ] Run the complete locked release verification: formatting, all-target checks, tests, Clippy
  with warnings denied, release build, and `git diff --check`.
- [ ] Tag the verified commit as `v1.0.0` and publish the corresponding release artifact and notes.

## Release decision

Release v1.0 only when all applicable items above are complete and the final supported-platform
claim matches the recorded qualification evidence. A successful weekend soak closes the principal
endurance gate, but it does not by itself close deliberate recovery testing, deployment support, or
release housekeeping.
