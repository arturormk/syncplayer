# Overlay layer protocol version 1

A player started with `--overlay-bind IP:PORT` accepts overlay layers over TCP
([ADR 0014](adr/0014-accept-host-local-overlay-layers-on-a-per-player-channel.md)). The channel is
independent of the synchronization protocol described in [`state-protocol.md`](state-protocol.md).
The suggested port is 46002, which is also the client's default.

A connection carries any number of request/response exchanges, one at a time. The player closes
a connection after 30 seconds without a request, after any malformed frame, and when more than
four connections are open (after sending an `ERROR`).

## Envelope

Every frame begins with this 12-byte network-byte-order envelope:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `SYNO` |
| 4 | 2 | protocol version (`1`) |
| 6 | 1 | message kind |
| 7 | 1 | reserved flags, zero |
| 8 | 4 | payload length |

Request payloads are limited to 16 MiB and response payloads to 64 KiB. Integers use network
byte order. A string is a two-byte byte length followed by UTF-8. Unknown versions, kinds, or
flags, oversized lengths, malformed fields, and trailing bytes are rejected.

## Common fields

- **token**: string of at most 256 bytes. Every request starts with it. It must equal the
  player's `--overlay-token`, or be empty when the player has none.
- **id**: string of 1-64 bytes of ASCII letters, digits, `.`, `_`, or `-`.
- **geometry**, 19 bytes:

| Size | Field | Meaning |
| ---: | --- | --- |
| 4 | x (i32) | render rectangle left edge in output-frame pixels, within ±16384 |
| 4 | y (i32) | render rectangle top edge, within ±16384 |
| 4 | w (u32) | render width, 1-16384 |
| 4 | h (u32) | render height, 1-16384 |
| 2 | z (i16) | stacking order; higher is drawn on top, ties ordered by id |
| 1 | opacity (u8) | whole-layer opacity, 0 transparent to 255 opaque |

The image is scaled to the render rectangle. Rectangles may extend beyond the frame and are
clipped. In `PUT`, `w = h = 0` renders the image at its own size.

## Requests

| Kind | Name | Payload after the token |
| ---: | --- | --- |
| 1 | `PUT` | id, geometry, then PNG bytes to the end of the payload |
| 2 | `MOVE` | id, geometry |
| 3 | `DELETE` | id |
| 4 | `CLEAR` | — |
| 5 | `LIST` | — |

`PUT` creates or replaces a layer. Any PNG color type and bit depth is accepted. Decoded images
are limited to 4096x4096, a player holds at most 32 layers, and all layers together at most 32 Mi
decoded pixels. `MOVE` replaces the geometry of an existing layer without new pixels.

## Responses

| Kind | Name | Payload |
| ---: | --- | --- |
| 64 | `OK` | — |
| 65 | `ERROR` | message string, at most 1024 bytes |
| 66 | `LAYERS` | u16 count, then per layer: id, geometry, u32 image width, u32 image height |

`LAYERS` answers `LIST`, in drawing order with the bottom layer first, and reports the resolved
render size even for layers put at their natural size.

## Rendering

Layers are drawn over the video and beneath the diagnostic OSD. They are host-local: they are not
part of the shared program, are not synchronized between players, and are lost when the player
restarts.
