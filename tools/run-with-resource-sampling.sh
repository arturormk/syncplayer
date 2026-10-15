#!/usr/bin/env bash

set -o pipefail

readonly SCRIPT_NAME="${0##*/}"

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME <OUTPUT.csv> <INTERVAL_SECONDS> -- <COMMAND> [ARGUMENT...]

Run a command while sampling its Linux process metrics from /proc. The CSV is
rewritten for each run and flushed after every sample.
EOF
}

die() {
    printf 'Error: %s\n\n' "$*" >&2
    usage >&2
    exit 2
}

forward_signal() {
    local signal=$1

    if [[ -n "${child_pid:-}" ]] && kill -0 "$child_pid" 2>/dev/null; then
        kill -s "$signal" "$child_pid" 2>/dev/null || true
    fi
}

read_status_value() {
    local key=$1
    local status_file=$2
    local value

    value=$(awk -v key="$key" '$1 == key ":" { print $2; exit }' "$status_file" 2>/dev/null) || true
    printf '%s' "$value"
}

read_io_value() {
    local key=$1
    local io_file=$2
    local value

    value=$(awk -v key="$key" '$1 == key ":" { print $2; exit }' "$io_file" 2>/dev/null) || true
    printf '%s' "$value"
}

main() {
    (($# >= 4)) || die 'Missing output, interval, separator, or command.'

    local output=$1
    local interval=$2
    shift 2
    [[ "$1" == -- ]] || die "Expected '--' before the command."
    shift
    (($# > 0)) || die 'Missing command.'
    [[ "$interval" =~ ^[0-9]+([.][0-9]+)?$ ]] || die 'Interval must be a positive number.'
    awk -v interval="$interval" 'BEGIN { exit !(interval > 0) }' \
        || die 'Interval must be greater than zero.'

    local clock_ticks
    clock_ticks=$(getconf CLK_TCK) || die 'Unable to determine the system clock tick rate.'

    printf '%s\n' \
        'timestamp_utc,elapsed_seconds,cpu_percent,rss_kib,rss_peak_kib,virtual_kib,threads,file_descriptors,read_bytes,write_bytes' \
        >"$output" || die "Unable to write resource log: $output"

    trap 'forward_signal INT' INT
    trap 'forward_signal TERM' TERM
    trap 'forward_signal HUP' HUP

    "$@" &
    child_pid=$!

    local started_at
    local previous_at=
    local previous_ticks=
    started_at=$(awk '{ print $1 }' /proc/uptime)

    while kill -0 "$child_pid" 2>/dev/null; do
        local now
        local stat_line
        local stat_fields
        local user_ticks
        local system_ticks
        local total_ticks
        local cpu_percent=0.000
        local elapsed_seconds
        local rss_kib
        local rss_peak_kib
        local virtual_kib
        local threads
        local file_descriptors
        local read_bytes
        local write_bytes
        local timestamp

        now=$(awk '{ print $1 }' /proc/uptime)
        stat_line=$(<"/proc/$child_pid/stat") || break
        stat_fields=${stat_line#*) }
        read -r -a stat_parts <<<"$stat_fields"
        user_ticks=${stat_parts[11]}
        system_ticks=${stat_parts[12]}
        total_ticks=$((user_ticks + system_ticks))

        if [[ -n "$previous_at" ]] && [[ -n "$previous_ticks" ]]; then
            cpu_percent=$(awk \
                -v tick_delta="$((total_ticks - previous_ticks))" \
                -v time_delta="$(awk -v now="$now" -v before="$previous_at" 'BEGIN { print now - before }')" \
                -v clock_ticks="$clock_ticks" \
                'BEGIN { if (time_delta > 0) printf "%.3f", 100 * tick_delta / clock_ticks / time_delta; else print "0.000" }')
        fi

        elapsed_seconds=$(awk -v now="$now" -v started="$started_at" 'BEGIN { printf "%.3f", now - started }')
        rss_kib=$(read_status_value VmRSS "/proc/$child_pid/status")
        rss_peak_kib=$(read_status_value VmHWM "/proc/$child_pid/status")
        virtual_kib=$(read_status_value VmSize "/proc/$child_pid/status")
        threads=$(read_status_value Threads "/proc/$child_pid/status")
        file_descriptors=$(find "/proc/$child_pid/fd" -mindepth 1 -maxdepth 1 -printf . 2>/dev/null | wc -c)
        read_bytes=$(read_io_value read_bytes "/proc/$child_pid/io")
        write_bytes=$(read_io_value write_bytes "/proc/$child_pid/io")
        timestamp=$(date --utc '+%Y-%m-%dT%H:%M:%S.%3NZ')

        printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
            "$timestamp" "$elapsed_seconds" "$cpu_percent" "${rss_kib:-}" \
            "${rss_peak_kib:-}" "${virtual_kib:-}" "${threads:-}" \
            "$file_descriptors" "${read_bytes:-}" "${write_bytes:-}" \
            >>"$output"

        previous_at=$now
        previous_ticks=$total_ticks
        sleep "$interval"
    done

    local child_status
    wait "$child_pid"
    child_status=$?
    return "$child_status"
}

main "$@"
