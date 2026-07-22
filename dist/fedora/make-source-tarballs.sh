#!/usr/bin/env bash
#
# Generate the two source tarballs referenced by moonshine.spec:
#
#   moonshine-<version>.tar.gz         pristine source (git archive of HEAD)
#   moonshine-<version>-vendor.tar.xz  vendored Rust dependencies + cargo config
#
# The vendor tarball is required for an offline RPM build because the project
# pins git forks (smithay, pixelforge, ash, inputtino) that cargo cannot
# resolve from crates.io inside a build chroot.
#
# Usage: make-source-tarballs.sh [output-dir]
#   output-dir defaults to ~/rpmbuild/SOURCES (created if missing).

set -euo pipefail

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -n1)
outdir=${1:-"$HOME/rpmbuild/SOURCES"}
mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)

echo "Packaging moonshine $version from $repo_root"

# 1. Pristine source tarball from git.
git -C "$repo_root" archive --format=tar.gz --prefix="moonshine-$version/" \
    -o "$outdir/moonshine-$version.tar.gz" HEAD
echo "Wrote $outdir/moonshine-$version.tar.gz"

# 2. Vendor tarball. Vendor from an extracted copy of the source tarball so the
#    result matches exactly what rpmbuild will unpack.
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

tar -xzf "$outdir/moonshine-$version.tar.gz" -C "$tmpdir"
cd "$tmpdir/moonshine-$version"

mkdir -p .cargo
# `cargo vendor` prints the [source] replacement config needed to use the
# vendor directory (including entries for each git dependency) — capture it.
cargo vendor --locked vendor > .cargo/config.toml

# --- inputtino-sys fixup -----------------------------------------------------
# inputtino-sys lives at bindings/rust/inputtino-sys inside the inputtino git
# repo and its build.rs compiles the C library from "../../../" (the repo
# root). `cargo vendor` only ships the crate subdirectory, so the C sources are
# missing from the vendor tree. Graft the full repo (from cargo's git
# checkout, populated by `cargo vendor` above) into the vendored crate and
# point build.rs at it. Also drop the `-lc++` link line: the RPM build
# compiles inputtino with g++, so libstdc++ is the right (and only needed)
# C++ runtime on Fedora.
inputtino_rev=$(sed -n 's|.*git+https://github.com/games-on-whales/inputtino#\([0-9a-f]*\)".*|\1|p' Cargo.lock | head -n1)
checkout=$(find "$HOME/.cargo/git/checkouts" -maxdepth 2 -type d -name "${inputtino_rev:0:7}*" -path "*inputtino*" | head -n1)
if [ -z "$checkout" ]; then
    echo "ERROR: inputtino git checkout for rev $inputtino_rev not found in ~/.cargo/git/checkouts" >&2
    exit 1
fi
cp -a "$checkout" vendor/inputtino-sys/native
rm -rf vendor/inputtino-sys/native/.git
sed -i 's|PathBuf::from("../../../")|PathBuf::from("native")|' vendor/inputtino-sys/build.rs
sed -i '/rustc-link-lib=c++/d' vendor/inputtino-sys/build.rs
# The crate content changed; blank the per-file checksums so cargo accepts it.
python3 -c "
import json
p = 'vendor/inputtino-sys/.cargo-checksum.json'
d = json.load(open(p))
d['files'] = {}
json.dump(d, open(p, 'w'))
"
# -----------------------------------------------------------------------------

tar -cJf "$outdir/moonshine-$version-vendor.tar.xz" vendor .cargo/config.toml
echo "Wrote $outdir/moonshine-$version-vendor.tar.xz"
