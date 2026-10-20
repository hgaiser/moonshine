#!/usr/bin/env bash
# Build and install ferriscope's Vulkan WSI layer.
#
#   dist/install-wsi.sh [--prefix PREFIX]
#
# Builds the layer from the ferriscope revision in Cargo.lock, so the layer
# matches the compositor linked into moonshine, and installs, under PREFIX
# (default /usr):
#   PREFIX/lib/ferriscope/libferriscope_wsi.so
#   PREFIX/share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json
# The manifest names the library by its final PREFIX path.
#
# Run it as your own user: it builds as you and uses sudo only to install into
# a directory you cannot write. Packaging sets DESTDIR to install into a
# staging directory instead of the system.
#
# FERRISCOPE_SRC overrides the ferriscope checkout (for a local ferriscope
# that is not pushed yet). Needs cargo and jq.
set -euo pipefail

prefix=/usr
while [ $# -gt 0 ]; do
	case $1 in
	--prefix) prefix=$2; shift 2 ;;
	-h | --help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done
root=$(cd "$(dirname "$0")/.." && pwd)

# ferriscope's installer resolves DESTDIR against the current directory; make
# it absolute so a relative staging directory works from anywhere.
if [ -n "${DESTDIR:-}" ]; then
	DESTDIR=$(realpath -m "$DESTDIR")
	export DESTDIR
fi

ferriscope=${FERRISCOPE_SRC:-}
if [ -z "$ferriscope" ]; then
	manifest=$(cd "$root" && cargo metadata --format-version 1 --locked |
		jq -r '.packages[] | select(.name == "ferriscope") | .manifest_path')
	if [ -z "$manifest" ]; then
		echo "ferriscope is not in Cargo.lock" >&2
		exit 1
	fi
	# .../ferriscope/Cargo.toml -> the ferriscope checkout.
	ferriscope=$(dirname "$(dirname "$manifest")")
fi

CARGO_TARGET_DIR=$root/target/ferriscope-wsi "$ferriscope/tools/install-wsi.sh" --prefix "$prefix"
