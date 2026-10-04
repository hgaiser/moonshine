#!/usr/bin/env bash
# Build the headless GPU-bound "game" probe into ./build/gpuload.
# Needs glslc, xxd, a C compiler and Vulkan headers (VULKAN_INCLUDE=<dir>
# when they are not installed system-wide).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${here}/build"
mkdir -p "${out}"
glslc -O "${here}/fs.frag" -o "${out}/fs.spv"
glslc -O "${here}/vs.vert" -o "${out}/vs.spv"
(cd "${out}" && xxd -i fs.spv > shaders.h && xxd -i vs.spv >> shaders.h)
${CC:-cc} -O2 -I"${out}" ${VULKAN_INCLUDE:+-I"${VULKAN_INCLUDE}"} -o "${out}/gpuload" "${here}/gpuload.c" -l:libvulkan.so.1 -lm
echo "${out}/gpuload"
