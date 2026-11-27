{
  description = "Moonshine — stream games to Moonlight clients from a headless Linux host";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # For consumers who want `pkgs.moonshine` available everywhere
      # (e.g. added next to a host in a NixOS flake's nixpkgs.overlays).
      overlays.default = final: _prev: {
        moonshine = final.callPackage ./nix/package.nix { };
      };

      packages = forAllSystems (pkgs: rec {
        moonshine = pkgs.callPackage ./nix/package.nix { };
        default = moonshine;
      });

      # NixOS service: `services.moonshine.*` (see nix/module.nix). No
      # overlay required. The default package is built against the host's
      # pkgs, not this flake's pinned nixpkgs: mesa and Vulkan drivers are
      # loaded at runtime from the host's /run/opengl-driver, and they break
      # if moonshine was linked against an older glibc than they were.
      nixosModules = {
        moonshine =
          { pkgs, lib, ... }:
          {
            imports = [ ./nix/module.nix ];
            services.moonshine.package = lib.mkDefault (pkgs.callPackage ./nix/package.nix { });
          };
        default = self.nixosModules.moonshine;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.moonshine ];
          # inputtino-sys's build.rs unconditionally links libc++. The package
          # build strips that from the vendored crate (see nix/package.nix),
          # but plain cargo in this shell builds the real inputtino checkout,
          # so `cargo test`/`cargo build` need libc++ to link — same reason
          # upstream's Arch build deps list libc++ next to gcc-libs.
          buildInputs = [ pkgs.llvmPackages.libcxx ];
          # Cargo-built binaries dlopen these before package RUNPATH setup.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.libglvnd
            pkgs.vulkan-loader
          ];
          # Share compiled crate artifacts across worktrees without sharing target/.
          RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
          packages = with pkgs; [
            rustfmt
            clippy
            sccache
            python3 # Hardware-independent smoke verifier and CI tests
            vulkan-tools
            xwayland
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        verification-helper-tests = pkgs.runCommand "moonshine-verification-helper-tests" { } ''
          export PYTHONPYCACHEPREFIX="$TMPDIR/pycache"
          ${pkgs.python3}/bin/python3 -m py_compile ${self}/tools/verification/*.py
          ${pkgs.python3}/bin/python3 -B -m unittest discover -s ${self}/tools/verification -p 'test_*.py'
          touch "$out"
        '';
        tv-helper-tests = pkgs.runCommand "moonshine-tv-helper-tests" { } ''
          export PYTHONPYCACHEPREFIX="$TMPDIR/pycache"
          ${pkgs.python3}/bin/python3 -m py_compile ${self}/tools/tv/*.py
          ${pkgs.python3}/bin/python3 -B -m unittest discover -s ${self}/tools/tv -p 'test_*.py'
          touch "$out"
        '';
      });
    };
}
