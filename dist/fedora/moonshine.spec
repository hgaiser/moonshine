# Build-time tests are disabled by default: some unit tests exercise
# GPU/compositor paths that are not available in a build chroot.
# Enable with: rpmbuild --with check ...
%bcond_with check

Name:           moonshine
Version:        0.12.0
Release:        1%{?dist}
Summary:        Game streaming host for Moonlight clients

License:        BSD-2-Clause
URL:            https://github.com/hgaiser/moonshine
# Created with dist/fedora/make-source-tarballs.sh:
Source0:        %{name}-%{version}.tar.gz
# Vendored Rust dependencies (vendor/ + .cargo/config.toml). Required because
# the project pins git forks of smithay, pixelforge, ash and inputtino, which
# cannot be resolved offline from crates.io.
Source1:        %{name}-%{version}-vendor.tar.xz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  clang
BuildRequires:  cmake
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  make
# aws-lc-sys (pulled in via aws-lc-rs) needs perl to build.
BuildRequires:  perl
BuildRequires:  pkgconfig(egl)
BuildRequires:  pkgconfig(gbm)
BuildRequires:  pkgconfig(libdrm)
BuildRequires:  pkgconfig(libevdev)
BuildRequires:  pkgconfig(libpulse)
BuildRequires:  pkgconfig(opus)
BuildRequires:  pkgconfig(wayland-client)
BuildRequires:  pkgconfig(wayland-server)
BuildRequires:  pkgconfig(xkbcommon)
BuildRequires:  libshaderc-devel
BuildRequires:  vulkan-headers
BuildRequires:  vulkan-loader-devel
BuildRequires:  systemd-rpm-macros

# The compositor spawns Xwayland for X11 clients.
Requires:       xorg-x11-server-Xwayland
Requires:       vulkan-loader
# Applications are launched via systemd-run --user.
Requires:       systemd

%description
Moonshine lets you stream games from your PC to any device running Moonlight.
Each stream runs in its own isolated headless compositor, separate from your
desktop environment, so no monitor or active desktop session is required.
Video is encoded on the GPU (H.264, H.265, AV1) with HDR support, audio is
streamed with low-latency Opus, and full mouse/keyboard/gamepad input is
forwarded to the host.

%package tools
Summary:        Benchmarking utilities for Moonshine
Requires:       %{name}%{?_isa} = %{version}-%{release}

%description tools
Utilities for testing and benchmarking Moonshine, including moonshine-bench,
which benchmarks the video encoding pipeline by running an application inside
a headless compositor and collecting per-frame timing statistics.

%prep
%autosetup
# Unpack vendored dependencies (vendor/ and .cargo/config.toml).
tar -xJf %{SOURCE1}

%build
export RUSTFLAGS="%{?build_rustflags}"
cargo build --release --offline --workspace

%install
install -Dm755 target/release/moonshine %{buildroot}%{_bindir}/moonshine
install -Dm755 target/release/moonshine-bench %{buildroot}%{_bindir}/moonshine-bench
install -Dm755 dist/start-moonshine.sh %{buildroot}%{_bindir}/start-moonshine.sh

# Vulkan implicit layer that routes game frames to the Moonshine compositor.
install -Dm755 target/release/libmoonshine_wsi.so \
    %{buildroot}%{_libdir}/moonshine/vulkan-layers/libmoonshine_wsi.so
install -Dm644 dist/VkLayer_moonshine_wsi.json \
    %{buildroot}%{_datadir}/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json
# The manifest ships with a hardcoded /usr/lib path; point it at %%{_libdir}.
sed -i 's|/usr/lib/moonshine/vulkan-layers|%{_libdir}/moonshine/vulkan-layers|' \
    %{buildroot}%{_datadir}/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json

install -Dm644 dist/moonshine@.service %{buildroot}%{_unitdir}/moonshine@.service
install -Dm644 dist/60-moonshine.rules %{buildroot}%{_udevrulesdir}/60-moonshine.rules
install -Dm644 dist/moonshine-modules.conf %{buildroot}%{_modulesloaddir}/moonshine.conf

%if %{with check}
%check
cargo test --release --offline --workspace
%endif

%post
%systemd_post moonshine@.service

%preun
%systemd_preun moonshine@.service

%postun
%systemd_postun_with_restart moonshine@.service

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/moonshine
%{_bindir}/start-moonshine.sh
%dir %{_libdir}/moonshine
%dir %{_libdir}/moonshine/vulkan-layers
%{_libdir}/moonshine/vulkan-layers/libmoonshine_wsi.so
%{_datadir}/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json
%{_unitdir}/moonshine@.service
%{_udevrulesdir}/60-moonshine.rules
%{_modulesloaddir}/moonshine.conf

%files tools
%{_bindir}/moonshine-bench

%changelog
* Mon Jul 20 2026 Alexander Kvartz <917344+akvartz@users.noreply.github.com> - 0.12.0-1
- Initial Fedora package
