#!/usr/bin/env bash
# Prepare an H.264/MP4 sequence item for Raspberry Pi hardware decoding.

PREP_PROFILE=rpi
readonly PREP_PROFILE
source "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/prepare-video-common.sh"
