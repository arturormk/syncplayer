#!/usr/bin/env bash

set -Eeuo pipefail

readonly PROJECT_ROOT="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

die() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

prompt_yes_no() {
    local prompt=$1
    local answer

    printf '%s [y/N] ' "$prompt"
    if ! IFS= read -r answer; then
        printf '\n' >&2
        return 1
    fi

    case "$answer" in
        y | Y | yes | YES | Yes) return 0 ;;
        *) return 1 ;;
    esac
}

detect_native_target() {
    case "$(uname -m)" in
        aarch64 | arm64)
            TARGET_TRIPLE='aarch64-unknown-linux-gnu'
            TARGET_NAME='Raspberry Pi 64-bit / AArch64 Linux'
            ;;
        x86_64 | amd64)
            TARGET_TRIPLE='x86_64-unknown-linux-gnu'
            TARGET_NAME='x86-64 Linux'
            ;;
        armv6* | armv7* | armhf)
            die 'This is a 32-bit ARM system. Syncplayer supports Raspberry Pi with a 64-bit OS.'
            ;;
        *) die "Unsupported CPU architecture: $(uname -m)" ;;
    esac

    printf 'Detected native platform: %s (%s)\n' "$TARGET_NAME" "$TARGET_TRIPLE"
}

add_package() {
    local package=$1
    local existing

    for existing in "${MISSING_PACKAGES[@]-}"; do
        if [[ "$existing" == "$package" ]]; then
            return
        fi
    done
    MISSING_PACKAGES+=("$package")
}

apt_package_is_installed() {
    local status

    command -v dpkg-query >/dev/null 2>&1 || return 1
    status=$(dpkg-query --show --showformat='${Status}' "$1" 2>/dev/null) || return 1
    [[ "$status" == 'install ok installed' ]]
}

find_missing_apt_packages() {
    MISSING_PACKAGES=()

    if ! command -v cc >/dev/null 2>&1; then
        add_package build-essential
    fi
    if ! command -v pkg-config >/dev/null 2>&1; then
        add_package pkg-config
        add_package libgstreamer1.0-dev
        add_package libgstreamer-plugins-base1.0-dev
    else
        if ! pkg-config --atleast-version=1.20 gstreamer-1.0; then
            add_package libgstreamer1.0-dev
        fi
        if ! pkg-config --atleast-version=1.20 gstreamer-base-1.0 \
            || ! pkg-config --atleast-version=1.20 gstreamer-net-1.0; then
            add_package libgstreamer-plugins-base1.0-dev
        fi
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        if ! command -v curl >/dev/null 2>&1; then
            add_package curl
        fi
        if ! apt_package_is_installed ca-certificates; then
            add_package ca-certificates
        fi
    fi
}

install_apt_packages() {
    local -a privilege=()

    if ((EUID != 0)); then
        command -v sudo >/dev/null 2>&1 \
            || die 'sudo is required to install the missing system packages.'
        privilege=(sudo)
    fi

    printf 'Missing system packages: %s\n' "${MISSING_PACKAGES[*]}"
    prompt_yes_no 'Install them with apt-get?' \
        || die 'Required system packages were not installed.'

    "${privilege[@]}" apt-get update
    "${privilege[@]}" apt-get install -y "${MISSING_PACKAGES[@]}"
}

verify_build_dependencies() {
    local -a missing=()

    command -v cc >/dev/null 2>&1 || missing+=('a C compiler')
    command -v pkg-config >/dev/null 2>&1 || missing+=('pkg-config')

    if command -v pkg-config >/dev/null 2>&1; then
        pkg-config --atleast-version=1.20 gstreamer-1.0 \
            || missing+=('GStreamer 1.20+ development files')
        pkg-config --atleast-version=1.20 gstreamer-base-1.0 \
            || missing+=('GStreamer base 1.20+ development files')
        pkg-config --atleast-version=1.20 gstreamer-net-1.0 \
            || missing+=('GStreamer network 1.20+ development files')
    fi

    if ((${#missing[@]} > 0)); then
        printf 'Missing build dependencies:\n' >&2
        printf '  - %s\n' "${missing[@]}" >&2
        die 'Install the missing dependencies with your distribution package manager and run this script again.'
    fi
}

ensure_system_dependencies() {
    find_missing_apt_packages
    if ((${#MISSING_PACKAGES[@]} > 0)); then
        if command -v apt-get >/dev/null 2>&1; then
            install_apt_packages
        else
            printf 'Automatic system-package installation is supported on apt-based distributions only.\n' >&2
        fi
    fi
    verify_build_dependencies
}

ensure_rust() {
    local cargo_env
    local installer

    if ! command -v rustup >/dev/null 2>&1; then
        command -v curl >/dev/null 2>&1 \
            || die 'curl is required to install Rust with rustup.'
        prompt_yes_no 'Rustup is missing. Install it with the official minimal-profile installer?' \
            || die 'Rustup is required to select the project toolchain.'

        installer=$(mktemp)
        if ! curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
            https://sh.rustup.rs --output "$installer"; then
            rm -f -- "$installer"
            die 'Could not download the rustup installer.'
        fi
        if ! sh "$installer" -y --profile minimal; then
            rm -f -- "$installer"
            die 'The rustup installer failed.'
        fi
        rm -f -- "$installer"

        cargo_env="${CARGO_HOME:-$HOME/.cargo}/env"
        if [[ -f "$cargo_env" ]]; then
            # shellcheck disable=SC1090
            source "$cargo_env"
        fi
    fi

    command -v rustup >/dev/null 2>&1 || die 'rustup is not available after installation.'
    command -v cargo >/dev/null 2>&1 || die 'cargo is not available after installing rustup.'
}

main() {
    local artifact

    [[ "$(uname -s)" == 'Linux' ]] || die 'Syncplayer quickstart supports Linux only.'
    detect_native_target

    cd -- "$PROJECT_ROOT"
    ensure_system_dependencies
    ensure_rust

    printf 'Preparing the Rust toolchain selected by rust-toolchain.toml...\n'
    rustup show active-toolchain >/dev/null
    rustup target add "$TARGET_TRIPLE"

    printf 'Building Syncplayer for %s...\n' "$TARGET_TRIPLE"
    cargo build --locked --release --target "$TARGET_TRIPLE"

    artifact="$PROJECT_ROOT/target/$TARGET_TRIPLE/release/syncplayer"
    [[ -x "$artifact" ]] || die "The build completed, but no executable was found at $artifact"
    printf '\nSyncplayer executable: %s\n' "$artifact"
}

main "$@"
