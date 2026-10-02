# Pyroshine on NixOS

This repository contains a [Nix flake](https://wiki.nixos.org/wiki/Flakes) that
builds Pyroshine and provides a NixOS module for running it as a service. The
package, executable, and module retain the upstream `moonshine` names.

## What you get

- **A package**: the `moonshine` binary, the moonshine-wsi Vulkan layer, and the udev rules, built from this repository.
- **A NixOS module**: a `services.moonshine` service that takes care of lingering, kernel modules, device permissions, and the systemd service described in the [installation guide](INSTALLATION.md).
- **A dev shell**: the full build environment for working on Pyroshine.

## Building

```sh
nix build github:karsyboy/pyroshine
./result/bin/moonshine --help
```

## Running as a service

Add the flake to the inputs of your system flake:

```nix
inputs.pyroshine.url = "github:karsyboy/pyroshine";
```

Then import the module and enable the service in your configuration:

```nix
{ inputs, ... }:
{
  imports = [ inputs.pyroshine.nixosModules.default ];

  services.moonshine = {
    enable = true;

    # The user whose applications you want to stream.
    user = "alice";
    # Only needed when the user's uid is not declared in your
    # configuration. Check with `id -u alice`.
    uid = 1000;

    # Opens the GameStream ports. Only do this on a LAN or VPN-facing
    # firewall. See the network guidance in docs/INSTALLATION.md.
    openFirewall = true;

    # Settings from docs/CONFIGURATION.md go
    # here, written as nix instead of TOML.
    settings = {
      # Disable the default scanner, whose command points at /usr/bin/steam.
      application_scanner = [];
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

After `nixos-rebuild switch` the service is running. There is no `systemctl enable` step, and user lingering is enabled automatically. Pair with a Moonlight client as usual via http://localhost:47989/pin on the host (loopback only; use an SSH port forward on headless machines).

If you stream headless (no active desktop session) and want gamepad support, the streaming user must be a member of the `input` group so streamed games can read the virtual gamepads moonshine creates. The service does not grant it. Add it to the user's `extraGroups`:

```nix
users.users.alice.extraGroups = [ "input" ];
```

When streaming while a desktop session is active this is not required — the active seat user is granted access to input devices via ACLs.

Settings you leave out fall back to Pyroshine's inherited defaults. Note that
the default application list points at `/usr/bin/steam`, which does not exist
on NixOS, so set `application` as shown above.

## Development

```sh
nix develop
cargo build
```

The shell contains the full build environment, plus `clippy` and `rustfmt` as used by CI.

## Maintenance

The Rust dependency set comes from `Cargo.lock`. When updating dependencies,
also check Git dependency hashes in [package.nix](../nix/package.nix) and the
separate PyroWave pins in [pyrowave.nix](../nix/pyrowave.nix). See the
[PyroWave dependency guide](PYROWAVE.md#pinned-dependency) for the matching
build-script and runtime API requirements.
