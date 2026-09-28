#!/usr/bin/env bash
set -euo pipefail

# Reproducible build of Pyroshine's authoritative PyroWave dependency.
SOURCE_URL="https://github.com/karsyboy/pyrowave"
SOURCE_REVISION="e344479d6c0439e346c788a918ad5645713f7573"
GRANITE_REVISION="1b2d1801d2910fb09ebcded2f0bb3a3a781103b5"
VOLK_REVISION="47cddf7ed97b94118a08aacb548a411188e016cc"
VULKAN_HEADERS_REVISION="6802bb4733b63ed5efd3adb308a6c885ef180ea1"

workdir="${1:?usage: build-pyrowave.sh WORKDIR PREFIX}"
prefix="${2:?usage: build-pyrowave.sh WORKDIR PREFIX}"
src="${workdir}/src"

mkdir -p "${workdir}"
if [[ ! -d "${src}/.git" ]]; then
  git clone "${SOURCE_URL}" "${src}"
fi
git -C "${src}" fetch origin "${SOURCE_REVISION}"
git -C "${src}" checkout --detach "${SOURCE_REVISION}"

# This fork-owned helper checks out only Granite plus the two submodules used
# by the standalone library. Verify every transitive revision after it runs.
(cd "${src}" && ./checkout_granite.sh)
[[ "$(git -C "${src}" rev-parse HEAD)" == "${SOURCE_REVISION}" ]]
[[ "$(git -C "${src}/Granite" rev-parse HEAD)" == "${GRANITE_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/volk" rev-parse HEAD)" == "${VOLK_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/khronos/vulkan-headers" rev-parse HEAD)" == "${VULKAN_HEADERS_REVISION}" ]]

cmake -S "${src}" -B "${workdir}/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="${prefix}" \
  -DPYROWAVE_DEVEL=OFF \
  -DPYROWAVE_UTILS=OFF \
  -DBUILD_TESTING=OFF
cmake --build "${workdir}/build" --parallel
ctest --test-dir "${workdir}/build" --output-on-failure -R '^packet-validation$'
cmake --install "${workdir}/build"

test -e "${prefix}/lib/libpyrowave-shared.so.0"
