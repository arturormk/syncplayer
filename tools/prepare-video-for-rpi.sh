#!/usr/bin/env bash
#
# Prepare a Raspberry Pi 3B+-compatible H.264/MP4 rendition for Syncplayer.
# Compatible with FFmpeg 4.4.2 and later.
#
set -Eeuo pipefail

SCRIPT_NAME=${0##*/}
AUDIO=0
OVERWRITE=0
CRF=20
FPS=30
PRESET=slow
OUTPUT=""
INPUT=""
TMPFILE=""
INPUT_COLOR_TRANSFER=""
HDR_TO_SDR=0

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS] INPUT

Prepare a video for Raspberry Pi hardware decoding with Syncplayer.

Options:
  -o, --output FILE   Output filename (default: INPUT-rpi.mp4)
  -a, --audio         Include audio, encoded as AAC 160k
      --no-audio      Remove audio (default)
  -q, --crf N         Video quality, 0-51 (default: 20)
      --fps N         Output frame rate (default: 30, max: 30)
      --preset NAME   x264 preset (default: slow)
  -f, --force         Overwrite an existing output file
  -h, --help          Show this help

Video encoding:
  Codec:              H.264 / AVC
  Profile:            High or lower (preset-dependent)
  Level:              4.0
  Pixel format:       yuv420p
  Maximum resolution: 1920x1080 (no upscaling)
  GOP:                2 seconds, fixed keyframe interval
  B-frames:           2
  Color:              SDR Rec.709 (HDR inputs are tone-mapped)
  Container:          MP4

Examples:
  $SCRIPT_NAME video.mov
  $SCRIPT_NAME --audio video.mp4
  $SCRIPT_NAME -o output.mp4 --crf 18 video.mkv
  $SCRIPT_NAME --fps 25 --preset medium video.mov
  $SCRIPT_NAME --force video.mp4
EOF
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [[ -n "$TMPFILE" && -e "$TMPFILE" ]]; then
        rm -f -- "$TMPFILE"
    fi
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while (($#)); do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        -a|--audio)
            AUDIO=1
            shift
            ;;
        --no-audio)
            AUDIO=0
            shift
            ;;
        -f|--force)
            OVERWRITE=1
            shift
            ;;
        -o|--output)
            (($# >= 2)) || die "Missing argument for $1"
            OUTPUT=$2
            shift 2
            ;;
        -q|--crf)
            (($# >= 2)) || die "Missing argument for $1"
            CRF=$2
            shift 2
            ;;
        --fps)
            (($# >= 2)) || die "Missing argument for $1"
            FPS=$2
            shift 2
            ;;
        --preset)
            (($# >= 2)) || die "Missing argument for $1"
            PRESET=$2
            shift 2
            ;;
        --)
            shift
            (($# == 1)) || die "Expected exactly one input file"
            INPUT=$1
            shift
            ;;
        -*)
            die "Unknown option: $1"
            ;;
        *)
            [[ -z "$INPUT" ]] || die "Multiple input files specified"
            INPUT=$1
            shift
            ;;
    esac
done

[[ -n "$INPUT" ]] || {
    usage >&2
    die "No input file specified"
}

[[ -f "$INPUT" && -r "$INPUT" ]] ||
    die "Input file does not exist or is unreadable: $INPUT"

command -v ffmpeg >/dev/null 2>&1 ||
    die "ffmpeg is not installed"

command -v ffprobe >/dev/null 2>&1 ||
    die "ffprobe is not installed"

[[ "$CRF" =~ ^[0-9]+$ ]] &&
    ((CRF >= 0 && CRF <= 51)) ||
    die "CRF must be an integer between 0 and 51"

[[ "$FPS" =~ ^[0-9]+$ ]] &&
    ((FPS >= 1 && FPS <= 30)) ||
    die "FPS must be an integer between 1 and 30"

case "$PRESET" in
    ultrafast|superfast|veryfast|faster|fast|medium|slow|slower|veryslow)
        ;;
    *)
        die "Invalid x264 preset: $PRESET"
        ;;
esac

ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name \
    -of csv=p=0 -- "$INPUT" | grep -q . ||
    die "No readable video stream in input file"

INPUT_COLOR_TRANSFER=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=color_transfer \
    -of default=noprint_wrappers=1:nokey=1 -- "$INPUT")

case "$INPUT_COLOR_TRANSFER" in
    smpte2084|arib-std-b67)
        HDR_TO_SDR=1
        ;;
esac

if ((HDR_TO_SDR)); then
    ffmpeg -hide_banner -filters 2>/dev/null |
        grep -Eq '[[:space:]]zscale[[:space:]]' ||
        die "This HDR input requires FFmpeg's zscale filter"
    ffmpeg -hide_banner -filters 2>/dev/null |
        grep -Eq '[[:space:]]tonemap[[:space:]]' ||
        die "This HDR input requires FFmpeg's tonemap filter"
fi

if [[ -z "$OUTPUT" ]]; then
    BASENAME=${INPUT%.*}
    [[ "$BASENAME" != "$INPUT" ]] || BASENAME=$INPUT
    OUTPUT="${BASENAME}-rpi.mp4"
fi

INPUT_PATH=$(realpath -m -- "$INPUT")
OUTPUT_PATH=$(realpath -m -- "$OUTPUT")

[[ "$INPUT_PATH" != "$OUTPUT_PATH" ]] ||
    die "Input and output must be different files"

if [[ -e "$OUTPUT" ]]; then
    [[ "$OVERWRITE" == 1 ]] ||
        die "Output exists: $OUTPUT (use --force to overwrite)"
    [[ -f "$OUTPUT" && ! -L "$OUTPUT" ]] ||
        die "Refusing to overwrite a non-regular file"
fi

OUTPUT_DIR=$(dirname -- "$OUTPUT")
[[ -d "$OUTPUT_DIR" && -w "$OUTPUT_DIR" ]] ||
    die "Output directory is not writable: $OUTPUT_DIR"

# Keep incomplete output hidden behind a temporary name. The final rename is
# performed only after FFmpeg and the compatibility checks succeed.
TMPFILE=$(mktemp "${OUTPUT}.tmp.XXXXXXXX.mp4") ||
    die "Could not create temporary output file"

# Fit within the Pi 3 decoder's conservative 1080p30 target without upscaling.
VIDEO_FILTER="fps=${FPS},scale=w='min(1920,iw)':h='min(1080,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2"

if ((HDR_TO_SDR)); then
    # Convert PQ/HLG to linear light, tone-map it, then encode a genuine SDR
    # Rec.709 image. Relabelling HDR metadata alone produces incorrect color.
    VIDEO_FILTER+=",zscale=transfer=linear:npl=100,format=gbrpf32le"
    VIDEO_FILTER+=",tonemap=tonemap=hable:desat=0"
    VIDEO_FILTER+=",zscale=primaries=bt709:transfer=bt709:matrix=bt709:range=tv"
fi

VIDEO_FILTER+=",format=yuv420p"
GOP=$((FPS * 2))

FFMPEG_ARGS=(
    -hide_banner
    -nostdin
    -y
    -i "$INPUT"
    -map 0:v:0
)

if ((AUDIO)); then
    FFMPEG_ARGS+=(
        -map '0:a:0?'
    )
else
    FFMPEG_ARGS+=(-an)
fi

FFMPEG_ARGS+=(
    -c:v libx264
    -preset "$PRESET"
    -crf "$CRF"
    -pix_fmt yuv420p
    -profile:v high
    -level:v 4.0
    -vf "$VIDEO_FILTER"
    -color_primaries bt709
    -color_trc bt709
    -colorspace bt709
    -color_range tv
    -x264-params colorprim=bt709:transfer=bt709:colormatrix=bt709:fullrange=off
    -g "$GOP"
    -keyint_min "$GOP"
    -sc_threshold 0
    -bf 2
)

if ((AUDIO)); then
    FFMPEG_ARGS+=(
        -c:a aac
        -b:a 160k
        -ac 2
    )
fi

FFMPEG_ARGS+=(
    -sn
    -dn
    -movflags +faststart
    -f mp4
    "$TMPFILE"
)

echo "=== Syncplayer Raspberry Pi media preparation ==="
echo "Input:      $INPUT"
echo "Output:     $OUTPUT"
echo "Audio:      $([[ "$AUDIO" == 1 ]] && echo AAC || echo None)"
echo "CRF:        $CRF"
echo "Preset:     $PRESET"
echo "FPS:        $FPS"
echo "Max size:   1920x1080"
if ((HDR_TO_SDR)); then
    echo "Color:      HDR ($INPUT_COLOR_TRANSFER) -> SDR Rec.709"
else
    echo "Color:      SDR Rec.709"
fi
echo

if ! ffmpeg "${FFMPEG_ARGS[@]}"; then
    die "FFmpeg conversion failed"
fi

[[ -s "$TMPFILE" ]] ||
    die "FFmpeg produced an empty output file"

ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name \
    -of csv=p=0 "$TMPFILE" | grep -qx h264 ||
    die "Output validation failed: video is not H.264"

OUTPUT_COLOR=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=color_space,color_transfer,color_primaries \
    -of default=noprint_wrappers=1 "$TMPFILE")

grep -qx 'color_space=bt709' <<<"$OUTPUT_COLOR" &&
    grep -qx 'color_transfer=bt709' <<<"$OUTPUT_COLOR" &&
    grep -qx 'color_primaries=bt709' <<<"$OUTPUT_COLOR" ||
    die "Output validation failed: video is not tagged as SDR Rec.709"

if ((OVERWRITE)); then
    mv -f -- "$TMPFILE" "$OUTPUT" ||
        die "Could not move completed video to destination"
else
    # Use a hard link for an atomic no-clobber commit on Linux.
    ln -- "$TMPFILE" "$OUTPUT" ||
        die "Could not create output (file may already exist)"
    rm -f -- "$TMPFILE"
fi

TMPFILE=""

echo
echo "Media preparation completed successfully."
echo "Output: $OUTPUT"
echo "Size:   $(du -h -- "$OUTPUT" | cut -f1)"
