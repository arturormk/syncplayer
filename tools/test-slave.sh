#!/usr/bin/env bash

set -o pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS] <MASTER_IP:PORT> <PROGRAM>

Run a local Syncplayer slave with diagnostic logging enabled.

Arguments:
  MASTER_IP:PORT  Address of the Syncplayer master, for example 192.168.1.21:46000
  PROGRAM         Path to the program.toml file

Options:
  -h, --help      Show this help text and exit

The tool automatically selects a release build for the native target, falling
back to a native debug build. It writes GStreamer logs to gstreamer.log and the
combined player output to slave.log in the current directory.
EOF
}

die() {
    printf 'Error: %s\n\n' "$*" >&2
    usage >&2
    exit 2
}

native_target_triple() {
    case "$(uname -s):$(uname -m)" in
        Linux:aarch64 | Linux:arm64) printf '%s\n' 'aarch64-unknown-linux-gnu' ;;
        Linux:x86_64 | Linux:amd64) printf '%s\n' 'x86_64-unknown-linux-gnu' ;;
        *) return 1 ;;
    esac
}

find_syncplayer() {
    local target_dir="$PROJECT_ROOT/target"
    local target_triple
    local candidate
    local -a candidates=()

    if target_triple=$(native_target_triple); then
        candidates+=(
            "$target_dir/$target_triple/release/syncplayer"
            "$target_dir/release/syncplayer"
            "$target_dir/$target_triple/debug/syncplayer"
            "$target_dir/debug/syncplayer"
        )
    else
        candidates+=(
            "$target_dir/release/syncplayer"
            "$target_dir/debug/syncplayer"
        )
    fi

    for candidate in "${candidates[@]}"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

main() {
    local master
    local program
    local syncplayer
    local -a pipeline_status
    local syncplayer_status
    local tee_status

    if (($# == 1)) && [[ "$1" == '-h' || "$1" == '--help' ]]; then
        usage
        return 0
    fi

    (($# >= 1)) || die 'Missing required MASTER_IP:PORT argument.'
    (($# >= 2)) || die 'Missing required PROGRAM argument.'
    (($# == 2)) || die "Unexpected argument: $3"

    master=$1
    program=$2

    [[ "$master" =~ ^.+:[0-9]+$ ]] \
        || die "Invalid master address '$master'; expected IP:PORT."
    [[ -f "$program" ]] \
        || die "Program file does not exist or is not a regular file: $program"

    if ! syncplayer=$(find_syncplayer); then
        die "No native Syncplayer executable found. Build one with 'cargo build --release'."
    fi

    printf 'Executable: %s\n' "$syncplayer"
    printf 'Master:     %s\n' "$master"
    printf 'Program:    %s\n' "$program"
    printf 'Logs:       %s/gstreamer.log and %s/slave.log\n\n' "$PWD" "$PWD"

    GST_DEBUG_NO_COLOR=1 \
    GST_DEBUG_FILE="$PWD/gstreamer.log" \
    GST_DEBUG='2' \
    RUST_LOG=syncplayer=debug \
    RUST_BACKTRACE=1 \
    nice -n -19 \
        "$syncplayer" \
        slave \
        --master "$master" \
        --mute \
        "$program" \
        2>&1 | tee slave.log

    pipeline_status=("${PIPESTATUS[@]}")
    syncplayer_status=${pipeline_status[0]}
    tee_status=${pipeline_status[1]}

    printf 'syncplayer=%s tee=%s\n' \
        "$syncplayer_status" "$tee_status" |
        tee -a slave.log

    return "$syncplayer_status"
}

main "$@"
