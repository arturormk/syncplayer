# Four-Raspberry-Pi soak test report — 2026-10-16/17

## Summary

A four-player Syncplayer installation ran continuously for approximately 17 hours 37 minutes.
Playback was visually observed to remain flawless, with the four displays in sync and no process
exiting after all four players had been started successfully. The copied diagnostics and resource
artifacts end earlier, after approximately 17.16 hours and 629 playlist iterations; a final Master
terminal sample records continued healthy playback at iteration 645.

The recorded steady-state results are a strong pass. Clock health remained good, synchronization stayed sub-millisecond in normal operation, there were no corrective recoveries or steady-state disconnects, and resource use stabilized. The largest valid pairwise separation was 4.414 ms, and none of more than 300,000 pairwise aligned estimates exceeded 40 ms.

The overall test was not entirely clean: player04's first startup attempt terminated with SIGSEGV. The operator inspected `program.toml` without changing it and manually launched player04 again; the second process then completed the soak with the same program file. No core dump from the failed process is available, and the second invocation overwrote its local logs. The startup crash is therefore unresolved and is not covered by the steady-state pass. Termination also exposed a launcher-level problem: Ctrl-C on the Master ended with `syncplayer=141` and `tee=130`, while all three Slaves ended with `syncplayer=0`.

## Test circumstances

- Observed playback window: approximately 2026-10-16 15:25 UTC through 2026-10-17 09:02 UTC
  (17.62 hours).
- Test window represented by the copied resource logs: approximately 2026-10-16 15:25 UTC
  through 2026-10-17 08:36 UTC (17.16 hours).
- Topology: four Raspberry Pis named player01 through player04.
- Master: player01 at `192.168.1.18:46000`.
- Slaves: player02, player03, and player04.
- Platform: Raspberry Pi OS Lite based on Debian GNU/Linux 13; player04 used an AArch64 release build.
- Playback program: `__stuff/videos/program.toml` with a repeating three-cue playlist.
- Completed playlist iterations in the copied logs: 629. A final Master terminal sample at
  2026-10-17 09:02:47 UTC records iteration 645.
- Master launch command:

  ```text
  tools/test-master.sh --debug 192.168.1.18:46000 __stuff/videos/program.toml
  ```

- Slave launch command:

  ```text
  tools/test-slave.sh 192.168.1.18:46000 __stuff/videos/program.toml
  ```

- Slave test options applied by the script included drift slew, muted audio, warning-level Syncplayer logging, GStreamer warning logging, and one-second resource sampling.
- The Master selected `v4l2h264dec` hardware video decoding, automatic `glimagesink` output, and
  `GstSystemClock`; no audio decoder or audio sink was selected. This run therefore did not test an
  audio-backed Master clock.
- Visual observation: synchronization appeared perfect throughout the successful run.

The collected artifacts are under `__stuff/soak/`:

- `diagnostics`: a master console snapshot.
- `player01/analyze-diagnostics`: output from `tools/analyze-diagnostics.py`.
- `player01/output.csv`: centralized diagnostics recorded by the master.
- `player01/master.log` and `player{02,03,04}/slave.log`: application logs.
- `player*/gstreamer.log`: GStreamer logs.
- `player01/master-resources.csv` and `player{02,03,04}/slave-resources.csv`: resource measurements for the successful processes.

The four `sampling.csv` files are not part of this run. They are byte-identical, cover only 299 seconds, and are dated 2026-10-15. They were excluded from the resource conclusions.

## Player04 startup crash

The first player04 launch printed:

```text
/home/main/src/syncplayer/tools/run-with-resource-sampling.sh: line 48: 936 Segmentation fault "$@"
syncplayer=139 tee=0
```

The master-side lifecycle data corresponds to that failure:

1. player04 connected at master clock 429.399 s.
2. Its session ended 125 ms later with `UnexpectedEof` while the master was reading a Slave control message.
3. The operator opened `program.toml` only to verify that its player04 entry was present. The file was not changed. Its timestamp remained `Oct 16 13:34`, before the soak began.
4. The operator ran `test-slave.sh` again with the same program file.
5. A new player04 session connected at master clock 476.101 s, 46.577 seconds after the failed session ended.
6. The new process reached playing state within approximately four seconds and supplied its first valid settled diagnostic sample approximately six seconds after reconnecting. That sample's error was 0.079 ms.

This sequence was a process crash followed by a manual restart, not an automatic recovery within Syncplayer. The successful process consequently reports zero disconnects and zero corrective recoveries.

The available player04 files begin with the second invocation. Its resource log starts at 2026-10-16 15:27:38 UTC, and its GStreamer log records PID 1291 rather than failed PID 936. `test-slave.sh` and `run-with-resource-sampling.sh` rewrite their output files on each invocation, so the failed process's application, GStreamer, and resource logs did not survive. No traditional core file is present, `coredumpctl` is unavailable on the machine, and no captured backtrace is available. The cause of the segmentation fault therefore cannot be determined from this soak archive.

The same unchanged program file was used for both attempts. The successful retry therefore provides no indication that program configuration caused the first process to crash. The SIGSEGV appears transient from the available evidence, but it remains a real startup defect until reproduced or otherwise explained.

## Synchronization results

The centralized CSV contains 227,615 samples and five lifecycle events. A sample is considered valid when the pipeline is playing, no transition is in progress, and the clock is healthy.

| Player | Valid / total | Invalid | RMS error | Absolute p99 | Absolute p99.9 | Valid signed range |
|---|---:|---:|---:|---:|---:|---:|
| player01 | 54,519 / 57,015 | 2,496 (4.38%) | 0.099 ms | 0.276 ms | 0.635 ms | +0.024 to +4.416 ms |
| player02 | 53,480 / 56,908 | 3,428 (6.02%) | 0.112 ms | 0.385 ms | 0.643 ms | -0.075 to +1.964 ms |
| player03 | 53,474 / 56,865 | 3,391 (5.96%) | 0.133 ms | 0.426 ms | 0.674 ms | -0.049 to +2.513 ms |
| player04 | 53,388 / 56,827 | 3,439 (6.05%) | 0.120 ms | 0.398 ms | 0.644 ms | -0.069 to +2.798 ms |

No player had a valid absolute error above 40 ms. Almost every invalid sample was marked as a transition. The counts of samples marked not playing were zero for player01 and only three, four, and five for players 02, 03, and 04 respectively; these overlapped startup or transition periods. There were no clock-unhealthy samples.

Hourly absolute p99 errors remained stable for the duration of the test:

- player01: approximately 0.25–0.32 ms.
- player02: approximately 0.31–0.42 ms.
- player03: approximately 0.40–0.46 ms.
- player04: approximately 0.37–0.42 ms.

There is no sign of synchronization error accumulating with uptime. The console's `rate` field uses a short current window and varied between snapshots; it is not evidence of long-term drift.

### Pairwise separation

Positive mean values indicate that the first player was ahead of the second.

| Pair | Aligned valid samples | Mean | RMS | Absolute p99 | Absolute p99.9 | Signed range |
|---|---:|---:|---:|---:|---:|---:|
| player01 − player02 | 51,651 | +0.003 ms | 0.104 ms | 0.309 ms | 0.675 ms | -1.530 to +4.373 ms |
| player01 − player03 | 51,650 | -0.025 ms | 0.109 ms | 0.326 ms | 0.664 ms | -2.184 to +4.406 ms |
| player01 − player04 | 51,562 | -0.007 ms | 0.105 ms | 0.318 ms | 0.664 ms | -1.800 to +4.414 ms |
| player02 − player03 | 51,507 | -0.028 ms | 0.119 ms | 0.375 ms | 0.618 ms | -2.148 to +1.693 ms |
| player02 − player04 | 51,454 | -0.010 ms | 0.114 ms | 0.364 ms | 0.638 ms | -2.219 to +1.871 ms |
| player03 − player04 | 51,437 | +0.018 ms | 0.119 ms | 0.384 ms | 0.632 ms | -2.049 to +2.235 ms |

Every pair recorded zero excursions above 40, 50, 100, and 250 ms. The worst pairwise estimates involving player01 coincide with a small number of player01's isolated 3.6–4.4 ms valid samples; they do not form a sustained interval.

## Continuity and transitions

- All successful players remained at generation 1.
- No corrective recovery attempt occurred. The CSV's recovery value of 1 represents initial synchronization; the console and analyzer exclude it and correctly report zero recoveries.
- Apart from the failed first player04 process, no player disconnected during the steady-state run.
- Normal diagnostic sampling gaps are approximately five seconds at cue handoffs, with ordinary maxima of 6.03 seconds. The only longer gap was player04's 46.7-second gap between the failed process and its replacement.
- Playback returned immediately to sub-millisecond error after ordinary handoffs.

The run therefore validates sustained healthy operation. Its termination supplied only a brief,
combined Master control/clock-loss observation; it did not deliberately qualify separated network
failures, successful Master reconnection, a Slave restart during established playback, or clock
recovery.

## Termination observations

At 2026-10-17 09:02:47 UTC, immediately before termination, the Master was playing iteration 645
with a healthy settled sample and an error of 0.177 ms. Pressing Ctrl-C on the Master launcher
produced:

```text
syncplayer=141 tee=130
```

Exit 141 indicates that the Master process died from SIGPIPE. The most likely immediate mechanism
is the launcher's `run-with-resource-sampling.sh | tee` pipeline: Ctrl-C also terminates `tee`, and
the still-shutting-down player can then write to a closed output pipe. Regardless of mechanism,
this does not satisfy the clean-shutdown criterion and no `shutdown complete` evidence was
retained for the Master.

After the Master stopped, player04 repeatedly logged that reconnect attempts failed while it was
still free-running. It continued in that state for at least approximately 40 seconds, until the
operator pressed Ctrl-C at about 09:03:30 UTC. Its launcher then reported:

```text
syncplayer=0 tee=130
```

This is evidence that the successful player04 process handled Ctrl-C cleanly and that a Slave can
continue briefly after Master loss. The operator also confirmed `syncplayer=0` for player02 and
player03 on Ctrl-C, so all three successful Slave processes passed the process-exit portion of the
shutdown check. This does not qualify reconnection or prolonged Master loss.

## Resource results

Statistics below exclude the initial two-minute startup interval when calculating CPU summaries.

| Player | Duration | Mean CPU | CPU p95 | CPU max | Final RSS | RSS change, hour 1 to 17 |
|---|---:|---:|---:|---:|---:|---:|
| player01 | 17.163 h | 22.64% | 29.06% | 51.52% | 133.4 MiB | +2.23 MiB |
| player02 | 17.165 h | 21.61% | 27.83% | 37.21% | 141.6 MiB | +1.55 MiB |
| player03 | 17.164 h | 21.17% | 27.59% | 36.29% | 137.8 MiB | +0.90 MiB |
| player04 | 17.156 h | 21.65% | 28.07% | 37.40% | 139.1 MiB | +1.14 MiB |

RSS grew mainly during early operation and substantially flattened later. From hour 8 to hour 17, each process added only 0.23–0.31 MiB. This is not convincing evidence of a memory leak, although a 48–72-hour run would give greater confidence.

Other resource indicators remained bounded:

- The master normally held 45 file descriptors, briefly rising to 46.
- Slaves normally held 38 file descriptors, briefly rising to 39.
- Master thread count cycled with prepared playback work and returned to its baseline; slave counts similarly returned to 16 after temporary rises to 20.
- Virtual memory was flat at approximately 1.96 GiB for the master and 1.62–1.68 GiB for the slaves.
- No handle, thread, or virtual-memory count showed monotonic accumulation.

## Application and GStreamer logs

No application panic, fatal error, clock failure, or unrecovered steady-state error appears in the retained logs.

The application emitted frequent warnings that a local cue boundary differed from the Master's authoritative cue. The cue 1-to-2 form occurred once in every one of the 629 iterations on every player. Additional forms occurred 39–65 times per process at the other boundaries. These messages explicitly state that the local transition is treated as diagnostic only. Their systematic cadence, together with the synchronization measurements and visual result, indicates expected boundary timing rather than playback divergence. The warning level is nevertheless too noisy for a condition that occurs normally.

GStreamer recorded sparse QoS frame-drop warnings:

| Player | QoS drop messages |
|---|---:|
| player01 | 36 |
| player02 | 34 |
| player03 | 20 |
| player04 | 30 |

This is approximately one to two logged dropped frames per player-hour and was not visibly apparent. Remaining GStreamer messages were startup warnings: V4L2 capability-query failures, four decreasing-timestamp warnings per player, and eight initial `clock is not synchronized yet` messages on each slave. No GStreamer error was logged.

The master application log is 64.8 MB and contains 175,023 lines, of which 172,197 are DEBUG records dominated by keepalive acknowledgements and position telemetry. Together with centralized diagnostics and resource samples, player01 generated approximately 93 MB in 17 hours, or roughly 130 MB per day. Production deployments need log rotation or a quieter default level.

## Final verdict

**Steady-state playback and synchronization: PASS.**

Once all four successful processes were running, the system sustained excellent four-player synchronization for approximately 17 hours 37 minutes. Typical errors were tens to hundreds of microseconds, all p99 values in the analyzed 17.16-hour capture were below 0.5 ms, no valid or pairwise sample exceeded 4.5 ms, clock health remained good, no process exited during playback, and resource use stayed bounded.

**Startup robustness: FAIL/UNRESOLVED for the first player04 attempt.**

The initial SIGSEGV is a serious defect even though the replacement process ran flawlessly. It cannot be diagnosed from the retained artifacts because there is no core dump and the retry overwrote the first invocation's logs. The soak must not be cited as evidence that every startup path is reliable or that runtime reconnection works.

**Launcher-mediated shutdown: FAIL for the Master; PASS for all three Slaves.**

The Master returned 141 after Ctrl-C instead of completing a demonstrably graceful shutdown.
Players 02, 03, and 04 returned zero from Syncplayer after Ctrl-C. Player04's `tee` returned 130 as
expected for an interrupted pipeline; only the Syncplayer process result is used for the Slave
pass.

Recommended follow-up work:

1. Change test tooling to create a unique timestamped directory for every invocation so retries cannot destroy crash evidence.
2. Enable traditional per-process core files with an unlimited core-size limit before attempting to reproduce the player04 startup failure.
3. Retry repeated player startup with the unchanged `program.toml` while crash capture is enabled.
4. Fix or restructure launcher signal/output handling so Ctrl-C cannot turn a graceful player
   shutdown into SIGPIPE, then verify a zero Master exit and retain `shutdown complete` records.
5. Downgrade or rate-limit the expected cue-boundary warning.
6. Configure log rotation or reduce master DEBUG logging for long deployments.
7. Run explicit fault-injection tests for established slave loss, master loss, network interruption, and process restart.
8. Consider a 48–72-hour follow-up soak to increase confidence in the flattening RSS trend.
