#!/usr/bin/env bash
set -euo pipefail

SOURCE_DIR="${1:?source directory required}"
OUTPUT_DMG="${2:?output DMG required}"

# Compressed sidecars can occupy fewer source blocks than their copied bytes.
# Size the destination from logical file lengths, with room for filesystem
# metadata, instead of letting hdiutil infer capacity from allocated blocks.
SIZE_MB=$(python3 - "$SOURCE_DIR" <<'PY'
import os
import sys

total = 0
for directory, _, files in os.walk(sys.argv[1]):
    for name in files:
        path = os.path.join(directory, name)
        if not os.path.islink(path):
            total += os.stat(path).st_size
print((total * 2 + 1024 * 1024 - 1) // (1024 * 1024) + 64)
PY
)

hdiutil create -volname "OCvoice Audio Router" \
  -srcfolder "$SOURCE_DIR" -size "${SIZE_MB}m" -fs HFS+ \
  -ov -format UDZO "$OUTPUT_DMG"
