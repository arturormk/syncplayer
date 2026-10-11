# Media preparation

Syncplayer hosts decode local files against a shared timeline. The logical content must match, but
each host may use a rendition suited to its decoder. Prepare and validate every rendition before an
installation starts; real-time transcoding is outside Syncplayer's scope.

## Raspberry Pi renditions

If any player is a Raspberry Pi, prepare every video intended for that player with
[`tools/prepare-video-for-rpi.sh`](../tools/prepare-video-for-rpi.sh). The conservative output
profile targets the Raspberry Pi 3B+ and remains suitable for newer Raspberry Pi models:

- H.264/AVC, High profile or lower, level 4.0;
- 8-bit `yuv420p` at no more than 1920x1080 and 30 frames per second;
- a fixed two-second keyframe interval;
- MP4 with fast-start metadata; and
- SDR Rec.709 color, with PQ or HLG sources tone-mapped rather than merely relabelled.

The Rec.709 conversion matters. A Raspberry Pi 3 hardware decoder can decode the H.264 payload of
some HDR-tagged files but reject their `bt2100-pq` caps during GStreamer negotiation, causing an
otherwise avoidable software-decoder fallback.

Install FFmpeg and run:

```sh
./tools/prepare-video-for-rpi.sh -o media/example-rpi.mp4 source/example.mov
```

Audio is omitted by default. Add `--audio` when the cue requires it:

```sh
./tools/prepare-video-for-rpi.sh --audio -o media/example-rpi.mp4 source/example.mov
```

Run the command on a workstation; software x264 encoding and HDR tone mapping can be slow on a
Raspberry Pi. Copy the completed rendition to each Pi and configure that host to use its local path.

## Keeping renditions synchronized

Different host renditions must preserve the same start point, duration, playback rate, and program
content. Do not independently trim, pad, retime, or rearrange them. Record a checksum for every
deployed file so that replacing media is an explicit installation change.

For the shared-clock prototype, prefer using the same Pi-compatible output on every participating
host and verify that all copies have the same checksum. This removes content and encoder differences
from the clock experiment. The production design may use host-specific renditions once their timing
equivalence is validated.
