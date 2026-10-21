{
  lib,
  stdenv,
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
      ../tools
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

  # ferriscope's Vulkan WSI layer routes a game's swapchain frames straight
  # into the compositor (bypassing Xwayland) and carries HDR and pacing. It
  # lives in the ferriscope repository next to the ferriscope crate, but only
  # the ferriscope crate is a dependency, so the vendored sources above do not
  # contain it. Build it from the same ferriscope revision Cargo.lock pins, so
  # the layer always matches the compositor linked into moonshine.
  ferriscopeLockEntry =
    lib.findFirst (p: p.name == "ferriscope")
      (throw "ferriscope not found in Cargo.lock")
      (lib.importTOML ../Cargo.lock).package;
  # e.g. "git+https://github.com/hgaiser/ferriscope?rev=<rev>#<rev>"
  ferriscopeMatch = builtins.match "git\\+([^?#]+)(\\?[^#]*)?#(.+)" ferriscopeLockEntry.source;
  ferriscopeRepo = builtins.fetchGit {
    url = builtins.elemAt ferriscopeMatch 0;
    rev = builtins.elemAt ferriscopeMatch 2;
    allRefs = true;
  };

  # The architecture name the layer's manifest carries (ferriscope's
  # tools/install-wsi.sh fills in the same @ARCH@).
  wsiArch = stdenv.hostPlatform.uname.processor;

  ferriscope-wsi = rustPlatform.buildRustPackage {
    pname = "ferriscope-wsi";
    inherit (ferriscopeLockEntry) version;
    src = ferriscopeRepo;

    # ferriscope's own lockfile; its git dependencies are fetched by the rev it
    # pins, as for moonshine's.
    cargoLock = {
      lockFile = "${ferriscopeRepo}/Cargo.lock";
      allowBuiltinFetchGit = true;
    };

    # Only the layer: the compositor itself is already part of moonshine.
    cargoBuildFlags = [
      "-p"
      "ferriscope-wsi"
    ];

    # The layer's tests need a GPU and a compositor.
    doCheck = false;

    # cargoInstallHook has placed the cdylib in $out/lib; fail loudly if it is
    # missing. The implicit-layer manifest names the library by its store path.
    postInstall = ''
      test -f $out/lib/libferriscope_wsi.so
      install -d $out/share/vulkan/implicit_layer.d
      sed -e "s|@ARCH@|${wsiArch}|g" \
        -e "s|@LIBRARY_PATH@|$out/lib/libferriscope_wsi.so|g" \
        tools/wsi/VkLayer_FERRISCOPE_wsi.json.in \
        > $out/share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.${wsiArch}.json
    '';

    meta = {
      description = "ferriscope's Vulkan WSI layer";
      homepage = "https://github.com/hgaiser/ferriscope";
      license = lib.licenses.bsd2;
    };
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

  # A bare `cargo build` only builds the root package.
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
    # ferriscope's Vulkan implicit-layer manifest. It names the layer library
    # by its ferriscope-wsi store path, which keeps that path alive, so the
    # manifest is all moonshine needs to carry.
    install -Dm644 -t $out/share/vulkan/implicit_layer.d \
      ${ferriscope-wsi}/share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.${wsiArch}.json

    # Input-group access + active-seat ACLs for /dev/uinput and /dev/uhid,
    # picked up by services.udev.packages.
    install -Dm644 dist/60-moonshine.rules $out/lib/udev/rules.d/60-moonshine.rules

    # Polkit rule that lets the 'moonshine' group hold a block-type sleep
    # inhibitor (used by the suspend-inhibit tip in TIPS.md). The group itself
    # is declared in the NixOS module; the rule is referenced from there.
    install -Dm644 dist/50-moonshine-inhibit-sleep.rules \
      $out/share/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules
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

  passthru = {
    inherit ferriscope-wsi;
  };

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
