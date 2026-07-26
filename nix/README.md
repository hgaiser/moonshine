# Moonshine on NixOS ❄️

This directory contains a [nix flake](https://wiki.nixos.org/wiki/Flakes) that builds Moonshine and provides a NixOS module for running it as a service.

## What you get

- **A package**: the `moonshine` binary, the moonshine-wsi Vulkan layer, and the udev rules, built from this repository.
- **A NixOS module**: a `services.moonshine` service that takes care of everything from the [installation steps](../README.md#installation): lingering, kernel modules, device permissions, and the systemd service.
- **A dev shell**: the full build environment for working on Moonshine.

## Building

```sh
nix build github:hgaiser/moonshine
./result/bin/moonshine --help
```

## Running as a service

Add the flake to the inputs of your system flake:

```nix
inputs.moonshine.url = "github:hgaiser/moonshine";
```

Then import the module and enable the service in your configuration:

```nix
{ inputs, ... }:
{
  imports = [ inputs.moonshine.nixosModules.default ];

  services.moonshine = {
    enable = true;

    # The user whose applications you want to stream.
    user = "alice";
    # Only needed when the user's uid is not declared in your
    # configuration. Check with `id -u alice`.
    uid = 1000;

    # Opens the GameStream ports. Only do this on a LAN or VPN-facing
    # firewall. See Security in the main README.
    openFirewall = true;

    # Everything from the Configuration section of the main README goes
    # here, written as nix instead of TOML.
    settings = {
      application = [
        {
          title = "Steam";
          command = [
            "/run/current-system/sw/bin/steam"
            "steam://open/bigpicture"
          ];
        }
      ];
    };
  };
}
```

After `nixos-rebuild switch` the service is running. There is no `systemctl enable` step, and user lingering is enabled automatically. Pair with a Moonlight client as usual via http://localhost:47989/pin .

Settings you leave out fall back to Moonshine's defaults. Note that the default application list points at `/usr/bin/steam`, which doesn't exist on NixOS, so you will want to set `application` as shown above.

## Development

```sh
nix develop
cargo build
```

The shell contains the full build environment, plus `clippy` and `rustfmt` as used by CI.

## Maintenance

There is nothing here to keep up to date when dependencies change: the nix build derives everything from `Cargo.lock`. The one exception is a pinned hash for `ash` in [package.nix](package.nix). See the comment there.
