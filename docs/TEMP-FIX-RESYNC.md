# Temporary plan: detect drift and restore synchronization

## Status

Phases 1–3 are implemented in protocol version 4 and await loopback/physical-host verification.
This is not an architecture decision record and should be removed or replaced with durable
documentation once the behavior has been verified.

Implemented on 2026-10-12:

- per-process Master session UUIDs and stable transport-generation semantics;
- two-second application heartbeats, eight-second local-monotonic liveness expiry, stale-socket
  closure, fresh network-clock calibration, and prepared rejoin;
- typed, cue-aware position samples with loop-occurrence handling;
- three-sample warning/repair hysteresis, ten-second cooldown, and local Master/Slave repair from
  the authoritative logical plan;
- serialized recovery attempts with obsolete scheduled-release rejection; and
- complete state snapshots carried by heartbeats, including transport state, operation, sequence
  and clock anchors, cue index, cue-local position, and playlist iteration. Cue-local anchors are
  translated through each host's durations so rendition differences do not invalidate loop state.

Still pending:

- loopback and Raspberry Pi physical-sink verification against the acceptance criteria below;
- measured adjustment of provisional drift thresholds, if needed;
- automated Slave and Master `CLOCK_LOST` recovery; and
- replacement of this temporary plan with final protocol/operations documentation after results
  are recorded.

## Original problem

A Slave previously resynchronized only when it started or when its idle TCP connection to the
Master returned EOF or an error. Both ends removed the socket read timeout after the initial
handshake and then blocked indefinitely waiting for the other process to disappear.

This is not a reliable liveness test. If the Master host suspends, is heavily delayed, or temporarily
loses its network, the TCP connection can remain established. The Slave can therefore continue on
an extrapolated `GstNetClientClock` without ever entering the reconnect path. Restarting the Slave
appears to repair the problem because startup creates and calibrates a new network clock, obtains a
new release plan, and performs a complete pause/seek/rejoin.

There was a second, independent gap. Each player calculated expected-versus-actual position once
per second, but that value was only written to a debug log. No sustained-error threshold invoked a
repair. A `CLOCK_LOST` bus message was also only logged.

The implementation needed to distinguish these cases:

1. **Master or clock-source liveness was lost.** The Slave must keep playing, discard the stale
   connection and network clock, reconnect, recalibrate, and rejoin.
2. **The shared clock is healthy but one local pipeline is behind or ahead.** That player should
   repair its own pipeline after a material error persists.
3. **The Master's renderer stalls while its authoritative clock continues.** The Master should
   repair itself. Slaves should not be pulled backward to follow a transient fault in one renderer.

## Intended behavior

- Slaves free-run during a suspected Master outage; loss of liveness must not immediately stop or
  blank playback.
- A missed application heartbeat makes a Slave replace its control connection and
  `GstNetClientClock`, even if the kernel still considers the old TCP connection established.
- Reconnection uses the existing prepared pause/seek/future-release sequence.
- Small timing noise does not cause seeks. Only sustained, material position error causes local
  pipeline recovery.
- A Master pipeline stall is corrected on the Master against the authoritative Master clock. It is
  not published as a new timeline for all Slaves to chase.
- Recovery is serialized so heartbeat failure, `CLOCK_LOST`, and drift detection cannot start
  overlapping resets.
- Logs expose the transition through connected, free-running, reconnecting, preparing, and
  synchronized states.

## Phase 1: add application-level liveness

### Protocol

Extend the bounded, versioned protocol with post-release heartbeat messages. The exact spelling can
change during implementation, but the messages should carry at least:

```text
HEARTBEAT <master-session> <sequence> <master-clock-ns>
ALIVE     <master-session> <sequence>
```

The Master sends a heartbeat to every connected Slave. The Slave replies with the matching
sequence. These are application heartbeats, not TCP keepalive: receiving them proves that both
processes are being scheduled and that the protocol threads are making progress.

Create one unique Master session identifier per process run and include it in the handshake and
heartbeats. A Slave must never treat a heartbeat from a different session as continuation of its
current authority. Session identity is separate from transport generation.

The current generation counter is incremented for every joining Slave. That does not represent a
timeline change and should be separated from connection/join sequencing before generation is used
to reject stale state. Transport generation should change only when the authoritative timeline is
replaced or discontinuously changed.

### Timing

Start with conservative constants, kept together and named so they can become CLI/configuration
options later:

- heartbeat interval: 2 seconds;
- liveness timeout: 8 seconds;
- reconnect retry interval: 1 second, as today.

The liveness timeout is measured with the Slave host's local monotonic time, not the possibly stale
network clock. A timeout or malformed/wrong-session heartbeat closes the old connection and enters
the same free-running reconnect loop as EOF.

The Master's per-Slave session also expires when replies stop. It should terminate its old session
thread and allow the Slave's new connection to become authoritative without retaining an idle
thread forever.

### Slave recovery state machine

Replace the blocking `monitor_master` read loop with explicit states:

```text
Synchronized
    -> heartbeat timeout / EOF / protocol error
FreeRunning
    -> repeated TCP connection attempts
ClockCalibrating
    -> create a new GstNetClientClock and wait for synchronization
PreparingRejoin
    -> receive current authoritative state, pause, seek and preroll
Synchronized
    -> release at the supplied future Master-clock timestamp
```

Failures in calibration or preparation return to `FreeRunning`; they do not terminate otherwise
healthy local playback. Fatal GStreamer/media errors remain fatal.

Do not reuse the old `GstNetClientClock` after liveness expires. Recreating it is essential to the
reported failure because a restarted Slave currently recovers precisely by doing this.

## Phase 2: make drift telemetry actionable

### Position samples

Refactor `log_telemetry` so position measurement produces a typed sample as well as a debug log. A
sample should contain:

- current clock time;
- expected sequence position and expected item;
- actual queried position and actual item/URI where it can be determined;
- signed error;
- pipeline state;
- whether a release, seek, or cue transition is in progress.

Loop boundaries and playlist transitions must be handled before thresholds are enabled. A raw
subtraction of two item-local positions can look like a nearly full-item error at a boundary. Use a
sequence-aware or circular error and suppress samples whose actual cue identity cannot be
established reliably.

### Initial correction policy

Use hysteresis rather than correcting a single sample. Initial provisional values:

- log a warning after absolute error exceeds 100 ms for three consecutive valid samples;
- repair after absolute error exceeds 250 ms for three consecutive valid samples;
- clear the consecutive-error count once error falls below 100 ms;
- impose a 10-second recovery cooldown to prevent reset loops.

These values are deliberately much larger than the final one-frame synchronization target. They
are intended to recover obvious stalls without turning ordinary decoder jitter into repeated
visible seeks. Replace them with measured values after physical-host tests.

Do not count samples while paused, prerolling, seeking, waiting for a scheduled release, or during a
playlist handoff. A failed position query is observable but is not by itself a drift sample.

### Local repair

When a material error persists, calculate the authoritative sequence position at a future release
deadline, reset only the affected local pipeline, seek/preroll, and release it against its current
healthy shared clock. Reuse the existing `resynchronize` machinery after separating plan creation
from network join handling.

- On a Slave, repair against the most recent valid Master state and current synchronized clock.
- On the Master, repair against the Master's authoritative logical plan and playback clock.
- Do not derive a new global timeline from the Master's lagging `query_position`; otherwise every
  healthy Slave would be forced to reproduce a local Master rendering fault.

If the Slave's clock is not known to be healthy, do not perform a local drift repair. Enter the
liveness/reconnect path and obtain a freshly calibrated clock first.

## Phase 3: periodic authoritative state

Heartbeats solve the immediate stale-connection problem, but they do not replace the periodic state
snapshots required by ADRs 0004 and 0006. Add a complete, idempotent state snapshot containing at
least:

- Master session;
- transport generation;
- operation/transport state;
- sequence/cue identity;
- anchor media position;
- anchor clock timestamp;
- playlist occurrence information needed to disambiguate loops.

The snapshot may be combined with the heartbeat for this prototype, provided it remains bounded and
fresh state cannot be delayed indefinitely behind old reliable traffic. Receiving an unchanged
snapshot must not cause a seek. It refreshes authority and gives the Slave an independent basis for
calculating error. A new session or discontinuous generation enters prepared rejoin; stale or
duplicate snapshots are ignored.

This phase becomes necessary before adding live pause, resume, goto, or program changes. For the
present continuously playing prototype, Phase 1 plus local drift repair addresses the reported
failure without inventing those controls early.

## `CLOCK_LOST` handling

Replace the current unconditional "continuing while synchronization recovers" log with an actual
state transition:

- Slave `CLOCK_LOST`: mark clock health suspect and request/retry a fresh connection and network
  clock while playback free-runs where GStreamer permits it.
- Master `CLOCK_LOST`: reacquire or confirm the pipeline-selected authoritative clock, recreate the
  exported provider if its clock identity changes, assign a new Master session, and require Slaves
  to reconnect. The precise GStreamer sequence needs a focused physical-audio-sink experiment
  before being automated.

Until that Master sequence is verified, log it as an explicit unrecovered fault rather than
claiming recovery is occurring.

## Concurrency and ownership changes

The current monitor thread can reset the pipeline while the main player monitor is querying it.
Introduce a small recovery coordinator owned by `Player`:

- an atomic or mutex-protected recovery state;
- cancellation of an obsolete scheduled release before every new recovery;
- one serialized prepare/reset/release operation at a time;
- a monotonically increasing local recovery attempt identifier so late workers cannot publish old
  results;
- clean shutdown signaling for heartbeat and reconnect threads.

Protocol parsing, socket I/O, and clock calibration stay off the GStreamer bus-monitoring loop.
Pipeline state changes go through the recovery coordinator.

## Observability

Add structured logs for:

- heartbeat sequence and round-trip time at debug level;
- last heartbeat age;
- Master session and transport generation;
- transition into and out of free-running mode;
- network-clock replacement and calibration duration;
- consecutive drift sample count and threshold crossing;
- recovery reason (`heartbeat-timeout`, `clock-lost`, `local-drift`, `new-session`);
- position before correction, target position, release deadline, and error after release;
- recovery cooldown/suppression decisions.

Avoid an info log for every heartbeat. State transitions and corrections are info/warn events;
routine samples remain debug telemetry.

## Tests

### Unit tests

- Heartbeat and session-bearing messages round-trip and remain bounded.
- Wrong-session, stale-sequence, malformed, and oversized messages are rejected.
- Heartbeat timeout uses local monotonic elapsed time.
- The recovery state machine preserves playback while reconnecting and serializes attempts.
- Generation is stable when another Slave joins and changes only with authoritative transport.
- Drift hysteresis ignores isolated spikes, triggers after the configured consecutive samples,
  clears below the lower threshold, and observes cooldown.
- Position error is correct across a loop boundary and playlist item transition.
- A scheduled release from an obsolete recovery attempt cannot start playback.

Use traits or small injected clock/connection abstractions for deterministic state-machine tests;
do not make unit tests wait for real multi-second timeouts.

### Loopback integration tests

- Suppress Master heartbeats without closing TCP; the Slave enters free-running mode and retries.
- Resume heartbeats on the stale connection; the Slave does not adopt it after declaring it dead.
- Accept a new connection, calibrate a new clock, and perform one prepared rejoin.
- Delay the Master process longer than the liveness timeout while leaving its socket open.
- Inject sustained local position error and verify one repair, not a seek every second.
- Verify that an additional Slave joining does not change existing Slaves' transport generation.

### Physical-host verification

Run with real audio and video sinks, not only `fakesink`:

1. Lock the Master desktop without suspend and run for at least 30 minutes.
2. Suspend the Master beyond the heartbeat timeout, then resume it.
3. Temporarily block TCP while leaving UDP clock traffic available, then test the inverse.
4. Apply CPU and I/O pressure to the Master without killing it.
5. Capture `RUST_LOG=syncplayer=debug` on both hosts and film numbered-frame test media together.
6. Repeat with the Master audio sink active because it normally supplies the authoritative clock.

Acceptance criteria:

- the Slave detects an application-level Master stall within the liveness timeout;
- it continues playback during the outage;
- it reconnects and performs at most one visible prepared correction when the Master returns;
- after recovery, measured display separation returns to no more than one frame;
- a healthy sixty-minute run performs no drift-triggered correction;
- a stalled Master pipeline is repaired locally without forcing healthy Slaves to jump backward.

## Suggested implementation order

1. Add session identity and correct generation semantics.
2. Add heartbeat protocol tests and the liveness state machine.
3. Verify the reported suspend/lock scenario with fresh clock creation on reconnect.
4. Refactor position telemetry into reliable, sequence-aware samples.
5. Add conservative sustained-drift recovery for Slaves, then Masters.
6. Define and add periodic authoritative snapshots.
7. Verify `CLOCK_LOST` recovery with real audio hardware.
8. Update the README and prototype procedure, record results, and replace this temporary document
   with durable protocol/ADR documentation where needed.

## Out of scope for the first fix

- continuously rate-adjusting or time-stretching media to hide large corrections;
- making Slaves follow the Master's queried renderer position instead of its authoritative clock;
- operator-driven pause, resume, goto, or live program replacement;
- treating TCP keepalive as a substitute for application liveness;
- choosing final correction thresholds without physical-host measurements.
