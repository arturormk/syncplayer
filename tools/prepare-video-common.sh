#!/usr/bin/env bash
# Shared implementation for Syncplayer's host-specific media preparation tools.
# Compatible with FFmpeg 4.4.2 and later.

set -Eeuo pipefail

SCRIPT_NAME=${0##*/}
AUDIO=1
OVERWRITE=0
CRF=20
FPS=25
WIDTH=1920
HEIGHT=1080
PRESET=slow
OUTPUT=""
INPUT=""
TMPFILE=""
INPUT_COLOR_TRANSFER=""
HDR_TO_SDR=0
HAS_INPUT_AUDIO=0

case "${PREP_PROFILE:-}" in
    rpi)
        TARGET_LABEL="Raspberry Pi"
        OUTPUT_SUFFIX="rpi"
        MAX_WIDTH=1920
        MAX_HEIGHT=1080
        MAX_FPS=30
        H264_LEVEL=4.0
        ;;
    pc)
        TARGET_LABEL="PC"
        OUTPUT_SUFFIX="pc"
        MAX_WIDTH=7680
        MAX_HEIGHT=4320
        MAX_FPS=120
        H264_LEVEL=""
        ;;
    *)
        printf 'ERROR: unknown media preparation profile: %s\n' "${PREP_PROFILE:-unset}" >&2
        exit 1
        ;;
esac

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS] INPUT

Prepare a normalized H.264/MP4 playlist item for Syncplayer on $TARGET_LABEL.

Options:
  -o, --output FILE   Output filename (default: INPUT-$OUTPUT_SUFFIX.mp4)
  -a, --audio         Preserve or synthesize normalized audio (default)
      --no-audio      Remove audio; not recommended for mixed playlists
  -q, --crf N         Video quality, 0-51 (default: 20)
      --size WIDTHxHEIGHT
                      Exact output canvas (default: 1920x1080)
      --fps N         Output frame rate (default: 25, max: $MAX_FPS)
      --preset NAME   x264 preset (default: slow)
  -f, --force         Overwrite an existing output file
  -h, --help          Show this help

Sequence normalization:
  Video:              H.264, yuv420p, exact size and constant frame rate
  Scaling:            Preserve aspect ratio and pad with black as needed
  GOP:                2 seconds, fixed keyframe interval
  Color:              SDR Rec.709 (HDR inputs are tone-mapped)
  Audio:              AAC, 48 kHz, stereo; silence added when absent
  Container:          MP4 with fast-start metadata

Examples:
  $SCRIPT_NAME video.mov
  $SCRIPT_NAME --size 1280x720 --fps 30 video.mp4
  $SCRIPT_NAME -o output.mp4 --crf 18 video.mkv
  $SCRIPT_NAME --no-audio video.mp4
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
        --size)
            (($# >= 2)) || die "Missing argument for $1"
            SIZE=$2
            shift 2
            [[ "$SIZE" =~ ^([0-9]+)x([0-9]+)$ ]] ||
                die "Size must use WIDTHxHEIGHT, for example 1920x1080"
            WIDTH=${BASH_REMATCH[1]}
            HEIGHT=${BASH_REMATCH[2]}
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

command -v ffmpeg >/dev/null 2>&1 || die "ffmpeg is not installed"
command -v ffprobe >/dev/null 2>&1 || die "ffprobe is not installed"

[[ "$CRF" =~ ^[0-9]+$ ]] && ((CRF >= 0 && CRF <= 51)) ||
    die "CRF must be an integer between 0 and 51"
[[ "$FPS" =~ ^[0-9]+$ ]] && ((FPS >= 1 && FPS <= MAX_FPS)) ||
    die "FPS must be an integer between 1 and $MAX_FPS"
((WIDTH >= 2 && WIDTH <= MAX_WIDTH && WIDTH % 2 == 0)) ||
    die "Width must be an even integer between 2 and $MAX_WIDTH"
((HEIGHT >= 2 && HEIGHT <= MAX_HEIGHT && HEIGHT % 2 == 0)) ||
    die "Height must be an even integer between 2 and $MAX_HEIGHT"

case "$PRESET" in
    ultrafast|superfast|veryfast|faster|fast|medium|slow|slower|veryslow) ;;
    *) die "Invalid x264 preset: $PRESET" ;;
esac

ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name \
    -of csv=p=0 -- "$INPUT" | grep -q . ||
    die "No readable video stream in input file"

if ffprobe -v error -select_streams a:0 \
    -show_entries stream=codec_name \
    -of csv=p=0 -- "$INPUT" | grep -q .; then
    HAS_INPUT_AUDIO=1
fi

INPUT_COLOR_TRANSFER=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=color_transfer \
    -of default=noprint_wrappers=1:nokey=1 -- "$INPUT")

case "$INPUT_COLOR_TRANSFER" in
    smpte2084|arib-std-b67) HDR_TO_SDR=1 ;;
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
    OUTPUT="${BASENAME}-${OUTPUT_SUFFIX}.mp4"
fi

INPUT_PATH=$(realpath -m -- "$INPUT")
OUTPUT_PATH=$(realpath -m -- "$OUTPUT")
[[ "$INPUT_PATH" != "$OUTPUT_PATH" ]] || die "Input and output must be different files"

if [[ -e "$OUTPUT" ]]; then
    [[ "$OVERWRITE" == 1 ]] || die "Output exists: $OUTPUT (use --force to overwrite)"
    [[ -f "$OUTPUT" && ! -L "$OUTPUT" ]] ||
        die "Refusing to overwrite a non-regular file"
fi

OUTPUT_DIR=$(dirname -- "$OUTPUT")
[[ -d "$OUTPUT_DIR" && -w "$OUTPUT_DIR" ]] ||
    die "Output directory is not writable: $OUTPUT_DIR"

TMPFILE=$(mktemp "${OUTPUT}.tmp.XXXXXXXX.mp4") ||
    die "Could not create temporary output file"

VIDEO_FILTER="fps=${FPS},scale=w=${WIDTH}:h=${HEIGHT}:force_original_aspect_ratio=decrease:force_divisible_by=2"
VIDEO_FILTER+=",pad=${WIDTH}:${HEIGHT}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"

if ((HDR_TO_SDR)); then
    VIDEO_FILTER+=",zscale=transfer=linear:npl=100,format=gbrpf32le"
    VIDEO_FILTER+=",tonemap=tonemap=hable:desat=0"
    VIDEO_FILTER+=",zscale=primaries=bt709:transfer=bt709:matrix=bt709:range=tv"
fi

VIDEO_FILTER+=",format=yuv420p"
GOP=$((FPS * 2))

FFMPEG_ARGS=(-hide_banner -nostdin -y -i "$INPUT")
if ((AUDIO && !HAS_INPUT_AUDIO)); then
    FFMPEG_ARGS+=(-f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=48000")
fi

FFMPEG_ARGS+=(-map 0:v:0)
if ((AUDIO)); then
    if ((HAS_INPUT_AUDIO)); then
        FFMPEG_ARGS+=(-map 0:a:0 -af "aresample=48000:async=1:first_pts=0,apad")
    else
        FFMPEG_ARGS+=(-map 1:a:0)
    fi
else
    FFMPEG_ARGS+=(-an)
fi

FFMPEG_ARGS+=(
    -c:v libx264
    -preset "$PRESET"
    -crf "$CRF"
    -pix_fmt yuv420p
    -profile:v high
)
if [[ -n "$H264_LEVEL" ]]; then
    FFMPEG_ARGS+=(-level:v "$H264_LEVEL")
fi
FFMPEG_ARGS+=(
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
        -ar 48000
        -ac 2
        -shortest
    )
fi

FFMPEG_ARGS+=(
    -sn
    -dn
    -movflags +faststart
    -f mp4
    "$TMPFILE"
)

echo "=== Syncplayer $TARGET_LABEL media preparation ==="
echo "Input:      $INPUT"
echo "Output:     $OUTPUT"
if ((AUDIO)); then
    if ((HAS_INPUT_AUDIO)); then
        echo "Audio:      AAC 48 kHz stereo (normalized from input)"
    else
        echo "Audio:      AAC 48 kHz stereo (generated silence)"
    fi
else
    echo "Audio:      None"
fi
echo "CRF:        $CRF"
echo "Preset:     $PRESET"
echo "FPS:        $FPS"
echo "Size:       ${WIDTH}x${HEIGHT}"
if ((HDR_TO_SDR)); then
    echo "Color:      HDR ($INPUT_COLOR_TRANSFER) -> SDR Rec.709"
else
    echo "Color:      SDR Rec.709"
fi
echo

ffmpeg "${FFMPEG_ARGS[@]}" || die "FFmpeg conversion failed"
[[ -s "$TMPFILE" ]] || die "FFmpeg produced an empty output file"

VIDEO_PROPERTIES=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name,width,height,pix_fmt,avg_frame_rate \
    -of default=noprint_wrappers=1 "$TMPFILE")
grep -qx 'codec_name=h264' <<<"$VIDEO_PROPERTIES" ||
    die "Output validation failed: video is not H.264"
grep -qx "width=$WIDTH" <<<"$VIDEO_PROPERTIES" &&
    grep -qx "height=$HEIGHT" <<<"$VIDEO_PROPERTIES" ||
    die "Output validation failed: video size is not ${WIDTH}x${HEIGHT}"
grep -qx 'pix_fmt=yuv420p' <<<"$VIDEO_PROPERTIES" ||
    die "Output validation failed: pixel format is not yuv420p"
FRAME_RATE=$(sed -n 's/^avg_frame_rate=//p' <<<"$VIDEO_PROPERTIES")
IFS=/ read -r FRAME_RATE_NUM FRAME_RATE_DEN <<<"$FRAME_RATE"
[[ "$FRAME_RATE_NUM" =~ ^[0-9]+$ && "$FRAME_RATE_DEN" =~ ^[1-9][0-9]*$ ]] &&
    ((FRAME_RATE_NUM == FPS * FRAME_RATE_DEN)) ||
    die "Output validation failed: frame rate is not $FPS fps"

OUTPUT_COLOR=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=color_space,color_transfer,color_primaries \
    -of default=noprint_wrappers=1 "$TMPFILE")
grep -qx 'color_space=bt709' <<<"$OUTPUT_COLOR" &&
    grep -qx 'color_transfer=bt709' <<<"$OUTPUT_COLOR" &&
    grep -qx 'color_primaries=bt709' <<<"$OUTPUT_COLOR" ||
    die "Output validation failed: video is not tagged as SDR Rec.709"

if ((AUDIO)); then
    AUDIO_PROPERTIES=$(ffprobe -v error -select_streams a:0 \
        -show_entries stream=codec_name,sample_rate,channels \
        -of default=noprint_wrappers=1 "$TMPFILE")
    grep -qx 'codec_name=aac' <<<"$AUDIO_PROPERTIES" &&
        grep -qx 'sample_rate=48000' <<<"$AUDIO_PROPERTIES" &&
        grep -qx 'channels=2' <<<"$AUDIO_PROPERTIES" ||
        die "Output validation failed: audio is not 48 kHz stereo AAC"
elif ffprobe -v error -select_streams a:0 \
    -show_entries stream=codec_name -of csv=p=0 "$TMPFILE" | grep -q .; then
    die "Output validation failed: audio was not removed"
fi

if ((OVERWRITE)); then
    mv -f -- "$TMPFILE" "$OUTPUT" || die "Could not move completed video to destination"
else
    ln -- "$TMPFILE" "$OUTPUT" || die "Could not create output (file may already exist)"
    rm -f -- "$TMPFILE"
fi
TMPFILE=""
