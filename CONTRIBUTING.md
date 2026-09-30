# Contributing to Pyroshine

Bug fixes, documentation improvements, and focused features are welcome. For bug
reports, include the Pyroshine version, distribution, GPU and driver, client and
codec, reproduction steps, healthcheck results, and relevant service logs.
Remove pairing credentials and other private data before sharing logs.

Keep changes focused and explain the resulting behavior and validation in your
pull request. Preserve upstream license notices and the internal `moonshine`
names used by crates, configuration, state, environment variables, and the WSI
protocol. The public packaged command and service are named `pyroshine`.

## Manually building the app

Run the following from the repository root. Install a current stable Rust
toolchain with Cargo (the workspace uses Rust edition 2024), Git, and the native
build dependencies. On Debian/Ubuntu, the CI dependency set is:

```sh
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential clang cmake libc++-dev libc++abi-dev libclang-dev \
  libdrm-dev libevdev-dev libgbm-dev libopus-dev libwayland-dev \
  libxkbcommon-dev patchelf pkg-config
```

Other distributions need the corresponding development packages. At runtime,
the host also needs systemd with a user D-Bus session, Xwayland, and working
Vulkan/GPU drivers. Nix users can use `nix develop` for the build environment;
see the [NixOS guide](docs/NIXOS.md).

Build the pinned PyroWave library and Rust workspace:

```sh
./scripts/build-pyrowave.sh /tmp/pyrowave-src /tmp/pyrowave-install
cargo build --locked --release --workspace
```

The helper downloads the pinned sources, verifies transitive revisions, runs
packet validation, and installs the library to the given prefix. See
[PyroWave dependency maintenance](docs/PYROWAVE.md#pinned-dependency) before changing pins.

Build outputs:

| Artifact | Purpose |
| --- | --- |
| `target/release/moonshine` | Server; installed as `pyroshine` |
| `target/release/libmoonshine_wsi.so` | Vulkan WSI layer loaded into applications |
| `target/release/moonshine-bench` | Developer benchmark utility |
| `/tmp/pyrowave-install/lib/libpyrowave-shared.so.0` | Optional PyroWave backend |

For a local server healthcheck, explicitly select the built PyroWave library:

```sh
MOONSHINE_PYROWAVE_LIBRARY=/tmp/pyrowave-install/lib/libpyrowave-shared.so.0 \
  ./target/release/moonshine healthcheck
```

A successful compile alone does not install the WSI manifest or device rules.
Use the installation steps below before validating streaming. When the optional
PyroWave library cannot load or has the wrong C API version, conventional codecs
remain available.

## Manual installation and upgrade

These instructions install local build outputs into the same system paths used
by native packages. Use this layout for a manually maintained installation on a
writable Linux system. For SteamOS use the [portable installer](docs/INSTALLATION.md#portable-installer-and-steamos).
Avoid mixing this layout with a package-managed or `/opt/pyroshine` installation:
package upgrades can overwrite local artifacts, and `/etc` units or Vulkan
manifests can take precedence over the files installed here. Choose one method
and remove the old integration with its original package manager or installer
when changing methods.

Before upgrading, end active streams and back up the streaming user's config,
certificates, and pairing state (`~/.config/moonshine` and
`~/.local/share/moonshine` by default). Retain the previous binaries and libraries
if you need to roll back. Build the new revision first, then stop the service:

```sh
sudo systemctl stop "pyroshine@$USER"
```

On a first install there may be no unit to stop. From the repository root,
install the artifacts and integration files:

```sh
sudo install -Dm755 target/release/moonshine /usr/bin/pyroshine
sudo install -Dm755 dist/start-pyroshine.sh /usr/bin/start-pyroshine.sh
sudo install -Dm755 target/release/libmoonshine_wsi.so /usr/lib/pyroshine/vulkan-layers/libmoonshine_wsi.so
sudo install -Dm755 /tmp/pyrowave-install/lib/libpyrowave-shared.so.0 /usr/lib/libpyrowave-shared.so.0
sudo install -Dm644 dist/VkLayer_pyroshine_wsi.json /usr/share/vulkan/implicit_layer.d/VkLayer_pyroshine_wsi.json
sudo install -Dm644 dist/pyroshine@.service /usr/lib/systemd/system/pyroshine@.service
sudo install -Dm644 dist/60-pyroshine.rules /usr/lib/udev/rules.d/60-pyroshine.rules
sudo install -Dm644 dist/pyroshine-modules.conf /usr/lib/modules-load.d/pyroshine.conf
sudo install -Dm644 dist/pyroshine-sysusers.conf /usr/lib/sysusers.d/pyroshine.conf
sudo install -Dm644 dist/50-pyroshine-inhibit-sleep.rules /usr/share/polkit-1/rules.d/50-pyroshine-inhibit-sleep.rules
sudo install -Dm644 LICENSE /usr/share/licenses/pyroshine/LICENSE
sudo ldconfig
sudo systemd-sysusers
sudo systemctl daemon-reload
sudo udevadm control --reload
sudo udevadm trigger
sudo modprobe uinput
sudo modprobe uhid
sudo systemctl reload-or-restart polkit.service
```

The shipped manifest assumes `/usr/lib/pyroshine/vulkan-layers/libmoonshine_wsi.so`.
If you choose a different library directory, update its `library_path` too.
If `/usr/lib` is outside your loader's search path, install PyroWave in your
distribution's library directory and run `ldconfig`, or set
`MOONSHINE_PYROWAVE_LIBRARY` to the installed library path in the service environment.

For headless use, enable lingering and gamepad access:

```sh
sudo loginctl enable-linger "$USER"
sudo usermod -aG input "$USER"
```

Log out and back in after changing group membership. The service grants the
`pyroshine` group for sleep inhibition; direct binary launches need that group
in the user's own membership. If your home is outside `/home/<user>`, update the
[service config path](docs/CONFIGURATION.md#file-location-and-loading) before starting.

Start or restart the service and verify the installed build:

```sh
sudo systemctl enable --now "pyroshine@$USER"
pyroshine --version
pyroshine healthcheck
systemctl status "pyroshine@$USER"
journalctl -u "pyroshine@$USER" -e
```

Repeat the build, stop, install, and start steps for subsequent manual upgrades.
Do not replace your config or pairing state with defaults. If validation fails,
stop the service, restore the previous binaries and matching libraries, then
start it again.

## Validation

CI runs these workspace checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --all-features
```

To verify the pinned PyroWave C API with the library you just built:

```sh
MOONSHINE_TEST_PYROWAVE=1 \
MOONSHINE_PYROWAVE_LIBRARY=/tmp/pyrowave-install/lib/libpyrowave-shared.so.0 \
  cargo test -p moonshine-core session::stream::video::pyrowave::tests::ffi_loads_pinned_api
```

For codec, compositor, or transport changes, also run the relevant
[GPU validation matrix](docs/PYROWAVE.md), [benchmarks](docs/BENCHMARKING.md),
and [reconnect checks](docs/reconnect-validation.md). Document which hardware
and clients you tested and any checks you could not run.

When changing configuration, update [CONFIGURATION.md](docs/CONFIGURATION.md)
together with the Rust fields, defaults, and examples. Keep supporting guides
in `docs/` and link them from the [documentation index](docs/README.md).

## Publishing a release

Record notable changes under `Unreleased` in the [fork changelog](docs/CHANGELOG.md)
as you make them. Keep entries concise and focused on user-visible behavior;
use `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, or `Security` sections.
The [upstream history](docs/UPSTREAM_CHANGELOG.md) is an archive and should not
receive new fork release entries.

Before releasing, update `[workspace.package].version` in `Cargo.toml` and refresh
workspace versions in `Cargo.lock`. Then prepare the changelog using Python 3.11
or newer (no additional packages are needed):

```sh
# Replace the date with the intended release date.
python3 scripts/changelog.py prepare --date YYYY-MM-DD
python3 scripts/changelog.py check
python3 -m unittest discover -s scripts -p 'test_changelog.py'
```

`prepare` moves the `Unreleased` notes into a dated entry for the workspace
version and leaves an empty `Unreleased` section for future changes. It refuses
empty notes or an already documented version. Review the result and commit the
changelog together with the version bump. To inspect the exact release body:

```sh
python3 scripts/changelog.py notes --tag vX.Y.Z
```

Run the workspace checks above and verify the packaged service and PyroWave
library before tagging that commit. Use the same semantic version as the
workspace and changelog:

```sh
# Example only: replace with the version being released.
git tag -a vX.Y.Z -m "Pyroshine vX.Y.Z"
git push origin vX.Y.Z
```

CI checks the changelog against the workspace version. The
[release workflow](.github/workflows/release.yaml) checks the tagged commit
before building: the tag must match `Cargo.toml`, and the latest dated changelog
entry must match that version and contain change bullets. Missing entries,
invalid dates, duplicate headings, or placeholder-only notes stop publication.
The matching entry becomes the GitHub release body, keeping published notes
aligned with the repository.

The workflow builds on Ubuntu 24.04 and publishes a GitHub Release containing the
portable x86_64 archive, installer, Debian/RPM/Arch packages, and `SHA256SUMS`.
It can also be run with `workflow_dispatch` for an existing tag. Check that the
workflow succeeded and all assets are attached, then test installation and an
upgrade from the previous release.

Packaging is defined in [nfpm.yaml](nfpm.yaml) and [dist/](dist/). Keep the
portable and native package integration files consistent when changing service,
udev, Vulkan, or policy paths.
