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

      # NixOS service: `services.moonshine.*` (see nix/module.nix). The
      # package defaults to this flake's build; no overlay required.
      nixosModules = {
        moonshine =
          { pkgs, lib, ... }:
          {
            imports = [ ./nix/module.nix ];
            services.moonshine.package =
              lib.mkDefault
                self.packages.${pkgs.stdenv.hostPlatform.system}.moonshine;
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
          packages = with pkgs; [
            rustfmt
            clippy
          ];
        };
      });
    };
}
