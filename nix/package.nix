{
  lib,
  rustPlatform,
  addDriverRunpath,
  cmake,
  pkg-config,
  libdrm,
  libevdev,
  libpulseaudio,
  libxkbcommon,
  libgbm,
  libopus,
  vulkan-loader,
  wayland,
  libglvnd,
}:

let
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  # Only what the build actually consumes, so touching flake.nix, the README,
  # or CI config doesn't rebuild the world. assets/ is required: the webserver
  # embeds assets/pin.html via include_bytes!.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../moonshine-core
      ../moonshine-tools
      ../moonshine-wsi
      ../assets
      ../dist
    ];
  };

  # inputtino-sys's build.rs compiles the C++ libinputtino with cmake from
  # `../../../` — the root of the inputtino git repo, of which the vendored
  # crate is only the bindings/rust/inputtino-sys subdirectory. Plain cargo
  # keeps full git checkouts so that works everywhere else, but nix vendoring
  # extracts just the crate, so the path escapes the vendor tree and cmake
  # finds no CMakeLists.txt. Graft the full repo into the vendored crate (see
  # postPatch) and aim build.rs at it. The rev is parsed out of Cargo.lock so
  # dependency bumps upstream are picked up without touching the nix code.
  inputtinoLockEntry =
    lib.findFirst (p: p.name == "inputtino-sys")
      (throw "inputtino-sys not found in Cargo.lock; drop the graft in nix/package.nix")
      (lib.importTOML ../Cargo.lock).package;
  # e.g. "git+https://github.com/games-on-whales/inputtino#<rev>"
  inputtinoMatch = builtins.match "git\\+([^?#]+)(\\?[^#]*)?#(.+)" inputtinoLockEntry.source;
  inputtinoRepo = builtins.fetchGit {
    url = builtins.elemAt inputtinoMatch 0;
    rev = builtins.elemAt inputtinoMatch 2;
    allRefs = true;
  };
in
rustPlatform.buildRustPackage {
  pname = "moonshine";
  inherit version src;

  # No vendor hash to maintain: crates.io checksums come straight from
  # Cargo.lock, and git dependencies are fetched at eval time by the rev the
  # lockfile pins (allowBuiltinFetchGit), so the lockfile the maintainers
  # already keep up to date is the single source of truth.
  cargoLock = {
    lockFile = ../Cargo.lock;
    allowBuiltinFetchGit = true;
    outputHashes = {
      # Exception: ash's pinned rev sits on an unmerged PR branch, unreachable
      # from any ref, so the builtin git fetcher (which only fetches refs)
      # cannot get it and this fixed hash is needed. If the ash pin changes
      # this fails loudly: a rev bump prints the correct new hash in the
      # mismatch error, and once the pin moves to a rev on a normal branch or
      # tag this entry can simply be deleted.
      "ash-0.38.0+1.4.329" = "sha256-apzc//AZqS3F4e4Epm3Dl20ZkkMKUvLjyxv7ZwJh1Jw=";
    };
  };

  # The inputtino graft described above. The cargo setup hook has already
  # copied the vendor dir into the build tree as $cargoDepsCopy (writable,
  # symlinks dereferenced); this attr runs before the hook that unsets it.
  # The crate's .cargo-checksum.json lists no files, so cargo accepts the
  # edits. While in there, also drop the unconditional `-lc++` (LLVM libc++)
  # link — redundant next to `-lstdc++` and absent from a gcc stdenv.
  postPatch = ''
    sysdir=$(echo "$cargoDepsCopy"/inputtino-sys-*)
    cp -r ${inputtinoRepo} "$sysdir/inputtino-repo"
    substituteInPlace "$sysdir/build.rs" \
      --replace-fail '"../../../"' '"inputtino-repo"' \
      --replace-fail 'println!("cargo:rustc-link-lib=c++");' ""
  '';

  # A bare `cargo build` only builds the root package; the workspace also
  # produces libmoonshine_wsi.so, the Vulkan implicit layer that routes a
  # game's swapchain frames into moonshine's headless compositor.
  cargoBuildFlags = [ "--workspace" ];

  nativeBuildInputs = [
    addDriverRunpath # see postFixup
    cmake # inputtino-sys and aws-lc-sys build their C/C++ via cmake
    pkg-config
    rustPlatform.bindgenHook # inputtino-sys generates its bindings at build time
  ];
  # cmake above is only for vendored crates' build scripts; the workspace
  # itself is plain cargo, so don't let cmake's setup hook take over
  # configurePhase.
  dontUseCmakeConfigure = true;

  buildInputs = [
    libdrm
    libevdev
    libpulseaudio
    libxkbcommon
    libgbm # smithay's GPU buffer allocation (split out of mesa in nixpkgs)
    libopus
    vulkan-loader
    wayland
  ];

  # The tests exercise the compositor/encoder paths and expect devices
  # (GPU, uinput) that the build sandbox does not have.
  doCheck = false;

  postInstall = ''
    # cargoInstallHook has placed the moonshine binary in $out/bin and the
    # moonshine-wsi cdylib in $out/lib; fail loudly if the layer is missing
    # (e.g. --workspace stopped covering it).
    test -f $out/lib/libmoonshine_wsi.so

    # Vulkan implicit-layer manifest, pointed at the store path.
    install -Dm644 dist/VkLayer_moonshine_wsi.json \
      $out/share/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json
    substituteInPlace $out/share/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json \
      --replace-fail /usr/lib/moonshine/vulkan-layers/libmoonshine_wsi.so \
      $out/lib/libmoonshine_wsi.so

    # Input-group access + active-seat ACLs for /dev/uinput and /dev/uhid,
    # picked up by services.udev.packages.
    install -Dm644 dist/60-moonshine.rules $out/lib/udev/rules.d/60-moonshine.rules
  '';

  postFixup = ''
    # NVIDIA (and GPU generally): pixelforge opens libvulkan.so.1 with
    # dlopen (ash's "loaded" feature) and smithay's GL renderer does the
    # same for libEGL, so neither lands in DT_NEEDED and the automatic
    # buildInputs RUNPATH doesn't cover them. Add the Vulkan loader + glvnd
    # dispatch explicitly, and /run/opengl-driver/lib (addDriverRunpath) so
    # the NVIDIA userspace driver and its Vulkan ICD resolve at runtime.
    patchelf --add-rpath ${
      lib.makeLibraryPath [
        vulkan-loader
        libglvnd
      ]
    } $out/bin/moonshine
    addDriverRunpath $out/bin/moonshine
  '';

  meta = {
    description = "Streaming server for Moonlight clients, written in Rust";
    homepage = "https://github.com/hgaiser/moonshine";
    license = lib.licenses.bsd2;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    mainProgram = "moonshine";
  };
}
