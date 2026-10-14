#!/usr/bin/env bash

set -o pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS] <MASTER_IP:PORT> <PROGRAM>

Run a local Syncplayer master with a repeatable multi-host test configuration.

Arguments:
  MASTER_IP:PORT  Address on which the master listens, for example 192.168.1.21:46000
  PROGRAM         Path to the program.toml file

Options:
      --debug     Enable debug-level Syncplayer logging
  -h, --help      Show this help text and exit

The clock port is the master port plus one. The installation console listens
on 127.0.0.1:2323. The tool writes GStreamer logs to gstreamer.log, combined
player output to master.log, and synchronization diagnostics to output.csv in
the current directory.

The tool automatically selects a release build for the native target, falling
back to a native debug build.
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
    local debug=false
    local rust_log=syncplayer=warn
    local port
    local port_number
    local clock_port
    local syncplayer
    local -a pipeline_status
    local syncplayer_status
    local tee_status
    local -a positional=()

    while (($# > 0)); do
        case "$1" in
            --debug) debug=true ;;
            -h | --help)
                usage
                return 0
                ;;
            --)
                shift
                positional+=("$@")
                break
                ;;
            -*) die "Unknown option: $1" ;;
            *) positional+=("$1") ;;
        esac
        shift
    done

    set -- "${positional[@]}"

    (($# >= 1)) || die 'Missing required MASTER_IP:PORT argument.'
    (($# >= 2)) || die 'Missing required PROGRAM argument.'
    (($# == 2)) || die "Unexpected argument: $3"

    master=$1
    program=$2

    if [[ "$debug" == true ]]; then
        rust_log=syncplayer=debug
    fi

    if [[ "$master" =~ ^.+:([0-9]{1,5})$ ]]; then
        port=${BASH_REMATCH[1]}
    else
        die "Invalid master address '$master'; expected IP:PORT."
    fi

    port_number=$((10#$port))
    ((port_number >= 1 && port_number <= 65534)) \
        || die "Master port must be between 1 and 65534 so the clock can use port + 1."
    clock_port=$((port_number + 1))

    [[ -f "$program" ]] \
        || die "Program file does not exist or is not a regular file: $program"

    if ! syncplayer=$(find_syncplayer); then
        die "No native Syncplayer executable found. Build one with 'cargo build --release'."
    fi

    printf 'Executable:  %s\n' "$syncplayer"
    printf 'Master bind: %s\n' "$master"
    printf 'Clock port:  %s\n' "$clock_port"
    printf 'Program:     %s\n' "$program"
    printf 'Console:     127.0.0.1:2323\n'
    printf 'Rust log:    %s\n' "$rust_log"
    printf 'Logs:        %s/gstreamer.log, %s/master.log, and %s/output.csv\n\n' \
        "$PWD" "$PWD" "$PWD"

    GST_DEBUG_NO_COLOR=1 \
    GST_DEBUG_FILE="$PWD/gstreamer.log" \
    GST_DEBUG='2' \
    RUST_LOG="$rust_log" \
    RUST_BACKTRACE=1 \
    "$syncplayer" \
        master \
        --bind "$master" \
        --clock-port "$clock_port" \
        --startup-delay 0 \
        --console-bind 127.0.0.1:2323 \
        --diagnostics-log output.csv \
        "$program" \
        2>&1 | tee master.log

    pipeline_status=("${PIPESTATUS[@]}")
    syncplayer_status=${pipeline_status[0]}
    tee_status=${pipeline_status[1]}

    printf 'syncplayer=%s tee=%s\n' \
        "$syncplayer_status" "$tee_status" |
        tee -a master.log

    return "$syncplayer_status"
}

main "$@"
