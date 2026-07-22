# Fedora packaging

RPM packaging for Moonshine. Produces:

- `moonshine` — the streaming server, start script, systemd unit
  (`moonshine@.service`), udev rules, modules-load config, and the
  `VkLayer_moonshine_wsi` Vulkan implicit layer.
- `moonshine-tools` — the `moonshine-bench` encoding-pipeline benchmark.

## Building locally

Install the build dependencies:

```sh
sudo dnf install rust cargo gcc gcc-c++ clang cmake make perl git \
    libevdev-devel pulseaudio-libs-devel opus-devel libxkbcommon-devel \
    wayland-devel mesa-libgbm-devel mesa-libEGL-devel libdrm-devel \
    libshaderc-devel vulkan-headers vulkan-loader-devel \
    rpm-build rpmdevtools
```

Generate the source tarballs (this runs `cargo vendor`, so it needs network
access — the RPM build itself is fully offline afterwards):

```sh
rpmdev-setuptree
./dist/fedora/make-source-tarballs.sh
```

Build the RPMs:

```sh
rpmbuild -ba dist/fedora/moonshine.spec
```

The packages land in `~/rpmbuild/RPMS/$(uname -m)/`.

## Installing

```sh
sudo dnf install ~/rpmbuild/RPMS/$(uname -m)/moonshine-*.rpm
sudo loginctl enable-linger $USER   # optional, for streaming while logged out
sudo systemctl enable --now moonshine@$USER
```

The service reads its configuration from
`/home/<user>/.config/moonshine/config.toml` (created on first start).

## Notes

- **Vendored dependencies**: the project pins git forks of `smithay`,
  `pixelforge`, `ash`, and `inputtino`, so dependencies are vendored into a
  tarball (`Source1`) rather than resolved at build time. This also means the
  package is not eligible for the official Fedora repos as-is (Fedora requires
  crates.io releases); it is suitable for COPR or local use.
- **COPR**: build the two tarballs locally with `make-source-tarballs.sh`,
  then upload the SRPM produced by
  `rpmbuild -bs dist/fedora/moonshine.spec`.
- **Tests**: `%check` is disabled by default because some unit tests exercise
  GPU/compositor code paths unavailable in a chroot. Enable with
  `rpmbuild --with check`.
