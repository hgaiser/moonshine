#!/usr/bin/env bash
# Regenerate the precompiled SPIR-V embedded by moonshine-core. Run after
# editing a shader and commit the .spv together with its source.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
shaders="${root}/moonshine-core/src/session/stream/video/pipeline/shaders"
for source in "${shaders}"/*.comp; do
  glslc --target-env=vulkan1.1 -O "${source}" -o "${source%.comp}.spv"
done
