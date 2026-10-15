#!/usr/bin/env bash
# Rebuilds the small media fixtures in tests/fixtures/.
#
# Requirements: python3 with Pillow (pip install pillow, or PYTHON=/path/to/venv/bin/python),
# optionally ffmpeg (FFMPEG=/path/to/ffmpeg) for real MP3/OGG/MP4 files.
# The FAT images used by the integration tests are NOT stored here: the tests
# build them on demand with mkfs.vfat and mtools.
set -euo pipefail
cd "$(dirname "$0")"
PYTHON="${PYTHON:-python3}"
"$PYTHON" make_fixtures.py
