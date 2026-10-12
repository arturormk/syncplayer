# Media preparation

Syncplayer hosts decode local files against a shared timeline. The logical content must match, but
each host may use a rendition suited to its decoder. Prepare and validate every rendition before an
installation starts; real-time transcoding is outside Syncplayer's scope.

Every item in one gapless playlist should have the same frame size, frame rate, pixel format, and
audio layout. In particular, do not mix files with and without audio: the Master normally exports
the audio-device clock, and removing that stream at an item boundary can stop that clock.

## PC renditions

Prepare videos for Linux PCs with [`tools/prepare-video-for-pc.sh`](../tools/prepare-video-for-pc.sh):

```sh
./tools/prepare-video-for-pc.sh -o media/example-pc.mp4 source/example.mov
```

The default output is H.264/MP4 on an exact 1920x1080 canvas at a constant 25 fps. The source is
scaled without distortion and centered with black padding when its aspect ratio differs. Audio is
encoded as 48 kHz stereo AAC, padded with silence if it ends before the video, or generated as a
silent track when the source has no audio.

Use the same overrides for every item in a playlist when a different format is required:

```sh
./tools/prepare-video-for-pc.sh --size 1280x720 --fps 30 source/example.mov
```

## Raspberry Pi renditions

If any player is a Raspberry Pi, prepare every video intended for that player with
[`tools/prepare-video-for-rpi.sh`](../tools/prepare-video-for-rpi.sh). The conservative output
profile targets the Raspberry Pi 3B+ and remains suitable for newer Raspberry Pi models:

- H.264/AVC, High profile or lower, level 4.0;
- 8-bit `yuv420p` on an exact 1920x1080 canvas at 25 frames per second by default;
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

As with the PC tool, audio is preserved or synthesized by default and normalized to 48 kHz stereo
AAC. `--no-audio` remains available for isolated single-media playback, but it is not recommended
for a playlist that contains any item with audio. The Pi profile accepts `--size` values up to
1920x1080 and `--fps` values up to 30.

Run the command on a workstation; software x264 encoding and HDR tone mapping can be slow on a
Raspberry Pi. Copy the completed rendition to each Pi and configure that host to use its local path.

## Keeping renditions synchronized

Different host renditions must preserve the same start point, duration, playback rate, and program
content. Invoke both tools with identical `--size` and `--fps` values across the complete sequence.
Do not independently trim, pad, retime, or rearrange renditions. Record a checksum for every
deployed file so that replacing media is an explicit installation change.

For the shared-clock prototype, prefer using the same Pi-compatible output on every participating
host and verify that all copies have the same checksum. This removes content and encoder differences
from the clock experiment. The production design may use host-specific renditions once their timing
equivalence is validated.
