#!/usr/bin/env bash
# Prepare a normalized H.264/MP4 sequence item for a Linux PC.

PREP_PROFILE=pc
readonly PREP_PROFILE
source "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/prepare-video-common.sh"
