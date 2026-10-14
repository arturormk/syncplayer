# State protocol version 9

Syncplayer separates three network responsibilities:

- reliable subscription and control over TCP;
- clock calibration through `GstNetTimeProvider`;
- freshness-oriented complete state snapshots over unicast UDP.

## Binary envelope

Every control frame and state datagram begins with this 12-byte network-byte-order envelope:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `SYNP` |
| 4 | 2 | protocol version (`9`) |
| 6 | 1 | message kind |
| 7 | 1 | reserved flags, currently zero |
| 8 | 4 | payload length |

TCP control payloads are limited to 16,384 bytes. UDP state datagrams, including the envelope, are
limited to 1,024 bytes. Integers use network byte order. Strings use a two-byte byte length followed
by UTF-8 and are bounded per field. Cue-duration arrays have a two-byte count and at most 128
entries. Unknown versions, message kinds, flags, enum values, lengths, trailing bytes, and invalid
UTF-8 are rejected.

## Reliable control

Control message kinds cover `SUBSCRIBE`, `SUBSCRIBED`, `READY`, `RELEASE`, `PREPARED`,
`KEEPALIVE`, `KEEPALIVE_ACK`, `TELEMETRY`, and `ERROR`. Subscription advertises the Slave's UDP receive port.
The response supplies the Master session, transport generation, clock port, selected state cadence,
and authoritative cue durations. A release plan identifies the structured operation, target
paused/playing state, media position, and shared-clock deadline. Preparation and release remain
reliable discontinuous operations.

After joining, the Slave periodically sends a monotonically increasing keepalive and requires the
matching acknowledgement. A failed control exchange closes that session; playback continues
free-running while reconnection and prepared rejoin happen in the background.

While the control session is active, a Slave periodically sends a bounded telemetry sample
containing shared clock time, generation, expected and actual sequence positions, signed error, cue position,
pipeline/transition/clock health, and cumulative disconnect/recovery counts. Telemetry is
diagnostic: it never changes authoritative transport state and its loss never stops playback.

## UDP state

Each UDP packet is a complete snapshot containing the Master session, packet sequence, sampled
Master clock, generation, program identity/hash, transport state and operation, sequence and cue
anchors, cue identity/occurrence/duration, playlist iteration, optional debug-loop state, and the
installation OSD enabled state. This lets `osd on`, `osd off`, and `osd toggle` converge through
the normal complete-snapshot path without introducing a second ad-hoc command channel.

The default Master cadence is 10 Hz and the default distribution is unicast fan-out to ready
registry participants. Within the current session and generation, a Slave accepts only a sequence
strictly newer than the last accepted packet. Wrong-session, old-generation, duplicate, and
reordered packets are ignored. A future generation requires the reliable prepared-rejoin path.
Malformed packets and playing anchors later than their sampled Master clock are rejected.

When an accepted duration mismatch makes a Slave rendition shorter than the authoritative cue, the
Slave holds that rendition at its last representable in-cue instant until a snapshot advances the
authoritative cue. The bounded mismatch must not invalidate the control session.

UDP state loss never stops playback and does not by itself tear down a healthy control session.
TCP control loss also leaves playback free-running. Clock synchronization has its own endpoint and
failure behavior.
