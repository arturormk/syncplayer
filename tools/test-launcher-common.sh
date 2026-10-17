#!/usr/bin/env bash

# Shared support for the physical-test launchers. This file is sourced rather
# than executed directly.

initialize_test_run() {
    local role=$1
    local output_root=$2
    local syncplayer=$3
    local program=$4

    local hostname_value
    local safe_hostname
    local timestamp
    local core_limit
    local core_pattern=unknown
    local git_revision=unknown

    TEST_LAUNCH_DIRECTORY=$PWD
    mkdir -p -- "$output_root" \
        || die "Unable to create test output root: $output_root"
    output_root=$(CDPATH= cd -- "$output_root" && pwd) \
        || die "Unable to resolve test output root: $output_root"

    hostname_value=$(hostname 2>/dev/null || printf '%s' unknown-host)
    safe_hostname=${hostname_value//[^A-Za-z0-9._-]/_}
    timestamp=$(date --utc '+%Y%m%dT%H%M%S.%NZ') \
        || die 'Unable to generate a UTC run timestamp.'
    TEST_RUN_DIRECTORY=$(mktemp -d -- \
        "$output_root/${timestamp}-${safe_hostname}-${role}.XXXXXX") \
        || die "Unable to create a unique test run directory under: $output_root"
    TEST_RUN_MARKER=$TEST_RUN_DIRECTORY/.run-started
    TEST_RUN_PID_FILE=$TEST_RUN_DIRECTORY/syncplayer.pid
    TEST_RUN_MANIFEST=$TEST_RUN_DIRECTORY/run-manifest.txt
    touch -- "$TEST_RUN_MARKER" \
        || die "Unable to create the run marker: $TEST_RUN_MARKER"

    if ulimit -c unlimited 2>/dev/null; then
        core_limit=$(ulimit -c)
    else
        core_limit=$(ulimit -c)
        printf 'Warning: unable to raise the core-size limit; current limit is %s.\n' \
            "$core_limit" >&2
    fi

    if [[ -r /proc/sys/kernel/core_pattern ]]; then
        core_pattern=$(</proc/sys/kernel/core_pattern)
    fi
    git_revision=$(git -C "$PROJECT_ROOT" rev-parse HEAD 2>/dev/null) || true

    {
        printf 'role=%s\n' "$role"
        printf 'started_utc=%s\n' "$(date --utc '+%Y-%m-%dT%H:%M:%S.%NZ')"
        printf 'launch_directory=%s\n' "$TEST_LAUNCH_DIRECTORY"
        printf 'run_directory=%s\n' "$TEST_RUN_DIRECTORY"
        printf 'hostname=%s\n' "$hostname_value"
        printf 'uname='; uname -a
        if [[ -r /etc/os-release ]]; then
            printf 'os_release='; tr '\n' ' ' </etc/os-release; printf '\n'
        fi
        printf 'git_revision=%s\n' "$git_revision"
        printf 'executable=%s\n' "$syncplayer"
        if command -v sha256sum >/dev/null 2>&1; then
            printf 'executable_sha256=%s\n' "$(sha256sum -- "$syncplayer" | awk '{ print $1 }')"
            printf 'program_sha256=%s\n' "$(sha256sum -- "$program" | awk '{ print $1 }')"
        fi
        printf 'program=%s\n' "$program"
        printf 'gstreamer_version=%s\n' \
            "$(gst-inspect-1.0 --version 2>/dev/null | tr '\n' ' ' || printf '%s' unavailable)"
        printf 'core_size_limit=%s\n' "$core_limit"
        printf 'core_pattern=%s\n' "$core_pattern"
        if command -v coredumpctl >/dev/null 2>&1; then
            printf 'coredumpctl=available\n'
        else
            printf 'coredumpctl=unavailable\n'
        fi
    } >"$TEST_RUN_MANIFEST" \
        || die "Unable to write the run manifest: $TEST_RUN_MANIFEST"
}

record_test_command() {
    {
        printf 'command='
        printf '%q ' "$@"
        printf '\n'
    } >>"$TEST_RUN_MANIFEST"
}

preserve_test_core_dumps() {
    local process_status=$1
    local core_directory=$TEST_RUN_DIRECTORY/core
    local capture_log=$TEST_RUN_DIRECTORY/core-capture.txt
    local pid=
    local found=false
    local source

    mkdir -p -- "$core_directory" || return 1
    if [[ -r "$TEST_RUN_PID_FILE" ]]; then
        read -r pid <"$TEST_RUN_PID_FILE" || true
    fi

    {
        printf 'process_status=%s\n' "$process_status"
        printf 'pid=%s\n' "${pid:-unknown}"

        if ((process_status < 128)); then
            printf 'core_preserved=not-applicable\n'
            printf 'note=Process did not report a signal-style exit status.\n'
            return 0
        fi

        # A relative kernel.core_pattern writes in the process working
        # directory. Copy newly-created traditional core files before another
        # invocation has an opportunity to replace them.
        while IFS= read -r -d '' source; do
            if cp --reflink=auto --sparse=always --preserve=all -- \
                "$source" "$core_directory/"; then
                printf 'preserved_traditional_core=%s\n' "$source"
                found=true
            else
                printf 'failed_to_preserve_traditional_core=%s\n' "$source"
            fi
        done < <(find "$TEST_LAUNCH_DIRECTORY" -maxdepth 1 -type f \
            \( -name core -o -name 'core.*' \) -newer "$TEST_RUN_MARKER" \
            -print0 2>/dev/null)

        # Ubuntu commonly sends crashes to Apport. Its .crash report contains
        # the compressed core and identifies the process PID, so retain only a
        # new report belonging to this invocation and unpack it when possible.
        if [[ "$pid" =~ ^[0-9]+$ ]] && [[ -d /var/crash ]]; then
            while IFS= read -r -d '' source; do
                if ! grep -a -E -m 1 -q \
                    "^[[:space:]]*Pid:[[:space:]]*$pid[[:space:]]*$" \
                    "$source" 2>/dev/null; then
                    continue
                fi

                local apport_report=$core_directory/$(basename -- "$source")
                if cp --reflink=auto --sparse=always --preserve=all -- \
                    "$source" "$apport_report"; then
                    printf 'preserved_apport_report=%s\n' "$source"
                    found=true
                    if command -v apport-unpack >/dev/null 2>&1; then
                        local apport_directory=$core_directory/apport-unpacked
                        mkdir -p -- "$apport_directory"
                        if apport-unpack "$apport_report" "$apport_directory"; then
                            printf 'unpacked_apport_report=%s\n' "$apport_directory"
                        else
                            printf 'apport_unpack=failed\n'
                        fi
                    fi
                else
                    printf 'failed_to_preserve_apport_report=%s\n' "$source"
                fi
            done < <(find /var/crash -maxdepth 1 -type f -newer \
                "$TEST_RUN_MARKER" -print0 2>/dev/null)
        fi

        # Many current distributions pipe cores to systemd-coredump. Export
        # the entry by exact PID when the tool and permissions allow it.
        if [[ "$pid" =~ ^[0-9]+$ ]] && command -v coredumpctl >/dev/null 2>&1; then
            if coredumpctl --no-pager --quiet info "$pid" \
                >"$core_directory/coredumpctl-info.txt" 2>&1; then
                if coredumpctl --quiet dump "$pid" \
                    --output="$core_directory/core.$pid"; then
                    printf 'preserved_systemd_core=%s\n' \
                        "$core_directory/core.$pid"
                    found=true
                else
                    printf 'systemd_core_export=failed\n'
                fi
            else
                printf 'systemd_core_entry=not-found-or-not-readable\n'
            fi
        fi

        if [[ "$found" == false ]]; then
            printf 'core_preserved=no\n'
            printf '%s\n' \
                'note=No core or crash report was found; inspect core_pattern and the host crash-storage policy.'
        else
            printf 'core_preserved=yes\n'
        fi
    } >"$capture_log" 2>&1
}

finish_test_run() {
    local process_status=$1
    local tee_status=$2

    preserve_test_core_dumps "$process_status" || true
    {
        printf 'finished_utc=%s\n' "$(date --utc '+%Y-%m-%dT%H:%M:%S.%NZ')"
        printf 'syncplayer_status=%s\n' "$process_status"
        printf 'tee_status=%s\n' "$tee_status"
    } >>"$TEST_RUN_MANIFEST"
}
