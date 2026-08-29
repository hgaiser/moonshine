# NixOS module for the Moonshine game streaming server.
#
# Runs moonshine as a *system* service on behalf of a regular user (upstream's
# moonshine@.service template, with start-moonshine.sh's env bootstrapping done
# declaratively), NOT as a systemd user unit: nixos-rebuild switch only manages
# the lifecycle of system units — user units merely get a daemon-reload — so as
# a user unit every change (including a new config store path in ExecStart)
# would need a manual `systemctl --user restart moonshine`. As a system unit
# the switch restarts it automatically whenever the unit or config changes.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.moonshine;

  settingsFormat = pkgs.formats.toml { };

  # The store-managed source of truth, passed straight to the unit. That is
  # safe because moonshine only ever *writes* a config when the given path
  # doesn't exist; all mutable state lives elsewhere and stays out of the
  # store: pairing state in ~/.local/share/moonshine/state.toml and the
  # self-signed TLS cert/key at the $HOME paths in the webserver config
  # ($HOME et al. are expanded by moonshine at runtime via shellexpand).
  configFile = settingsFormat.generate "moonshine-config.toml" cfg.settings;

  runtimeDir = "/run/user/${toString cfg.uid}";

  # The Vulkan implicit-layer manifest on its own, for the driver path below.
  # Everything in hardware.graphics.extraPackages is merged into
  # /run/opengl-driver, and programs.steam puts that lib/ on Steam's FHS
  # library path, so the whole package has no business being in there. The
  # manifest names the layer library by absolute store path, so it is all that
  # needs to go.
  wsiLayer = pkgs.runCommand "moonshine-wsi-layer" { } ''
    install -Dm644 \
      ${cfg.package}/share/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json \
      $out/share/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json
  '';
in
{
  options.services.moonshine = {
    enable = lib.mkEnableOption "Moonshine, a game streaming server for Moonlight clients";

    package = lib.mkPackageOption pkgs "moonshine" { };

    user = lib.mkOption {
      type = lib.types.str;
      example = "alice";
      description = ''
        User to run moonshine as. Streamed applications are launched as
        transient units inside this user's systemd instance, so it should be
        the user whose Steam library / applications you want to stream.
        Lingering is enabled for this user automatically.
      '';
    };

    uid = lib.mkOption {
      type = lib.types.nullOr lib.types.int;
      default = config.users.users.${cfg.user}.uid or null;
      defaultText = lib.literalExpression "config.users.users.<user>.uid";
      example = 1000;
      description = ''
        Numeric uid of {option}`services.moonshine.user`, used to locate the
        user's runtime dir (`/run/user/<uid>`) and session D-Bus socket, and
        to order the service after `user@<uid>.service`. Only needs to be set
        explicitly when the user's uid is allocated rather than declared
        (check with `id -u <user>`).
      '';
    };

    settings = lib.mkOption {
      inherit (settingsFormat) type;
      default = { };
      example = lib.literalExpression ''
        {
          name = "Moonshine";
          application = [
            {
              title = "Steam";
              command = [ "/run/current-system/sw/bin/steam" "steam://open/bigpicture" ];
            }
          ];
        }
      '';
      description = ''
        Moonshine configuration, generated into a TOML file in the store and
        passed to the daemon. Settings left out fall back to upstream's
        defaults (note: the default application list points at
        `/usr/bin/steam`, so you will want to at least set `application`).
        See <https://github.com/hgaiser/moonshine> for the format.
      '';
    };

    logFilter = lib.mkOption {
      type = lib.types.str;
      default = "moonshine=info";
      example = "moonshine=info,moonshine_core::tls=error";
      description = ''
        Value for `MOONSHINE_LOG` (env_logger-style filter). The example
        additionally silences the WARN logged for every dropped TLS probe —
        a Moonlight client idling on its Computers screen polls the HTTPS
        port every 5s, spamming "TLS handshake failed" into the journal.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Open the GameStream ports in the firewall: HTTP pairing/discovery,
        HTTPS, and RTSP over TCP; video/control/audio over UDP. Port numbers
        follow {option}`services.moonshine.settings` where set. Moonshine is
        not designed for public networks — only enable this on a LAN or
        VPN-facing firewall.
      '';
    };
  };

  disabledModules = [ "services/networking/moonshine.nix" ];

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.uid != null;
        message = ''
          services.moonshine.uid could not be derived: users.users.${cfg.user}.uid
          is not declared. Set services.moonshine.uid to `id -u ${cfg.user}`
          (or declare a fixed uid for the user).
        '';
      }
    ];

    # Puts `moonshine` on PATH for the healthcheck and pairing subcommands, and
    # is how the polkit rule below gets picked up.
    environment.systemPackages = [ cfg.package ];

    # The moonshine-wsi Vulkan layer routes a game's swapchain frames into
    # moonshine's compositor. It is an *implicit* layer, so the loader has to
    # find its manifest by scanning, and a store path is only scanned when it
    # is listed in XDG_DATA_DIRS. Nothing here has that: a systemd system
    # service gets no XDG_DATA_DIRS, and neither does the user instance that
    # launches the games when it was started by lingering rather than by a
    # graphical login. Without it everything silently falls back to rendering
    # through XWayland.
    #
    # nixpkgs builds the loader with SYSCONFDIR=/run/opengl-driver/share, which
    # it always scans, environment or not. So install the layer there: the
    # NixOS counterpart of the /etc/vulkan/implicit_layer.d drop that upstream's
    # install script does on atomic distributions. Only the 64-bit driver path,
    # because the layer is built for the host architecture and there is no
    # 32-bit build to put in extraPackages32. Nothing is lost: where the
    # manifest sits in an arch-agnostic directory (/usr/share on deb/rpm), a
    # 32-bit loader finds it and skips it over the ELF class anyway.
    hardware.graphics = {
      # Needed regardless of the layer: this option is what creates
      # /run/opengl-driver, which is where the package resolves its Vulkan
      # loader and the NVIDIA userspace driver from.
      enable = true;
      extraPackages = [ wsiLayer ];
    };

    # Grants the `input` group + active-seat ACLs on /dev/uinput and
    # /dev/uhid, which inputtino uses to create the virtual
    # gamepad/keyboard/mouse.
    services.udev.packages = [ cfg.package ];

    # Group that opt-in users join so the suspend-inhibit polkit rule
    # (shipped in the package) lets Moonshine hold a block-type sleep
    # inhibitor. See TIPS.md. The rule itself needs no wiring here: nixpkgs
    # builds polkit with --datadir=/run/current-system/sw/share and its module
    # links /share/polkit-1 into the system path, so
    # share/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules is picked up
    # from environment.systemPackages above.
    users.groups.moonshine = { };

    # Virtual input backends used by inputtino (upstream ships this as a
    # modules-load.d drop-in).
    boot.kernelModules = [
      "uinput"
      "uhid"
    ];

    # Keep the user's systemd instance and its session D-Bus alive from boot,
    # independent of any graphical login — moonshine needs both. This is what
    # upstream's `loginctl enable-linger` install step does.
    users.users.${cfg.user}.linger = true;

    systemd.services.moonshine = {
      description = "Moonshine game streaming server (Moonlight protocol)";
      wantedBy = [ "multi-user.target" ];
      # The user manager owns the runtime dir, session bus, and the transient
      # units moonshine launches apps as (moonshine-session.service).
      requires = [ "user@${toString cfg.uid}.service" ];
      after = [ "user@${toString cfg.uid}.service" ];
      # The compositor spawns Xwayland (X11 games, i.e. most of Steam, run
      # under it) from the unit's PATH.
      path = [ pkgs.xwayland ];
      environment = {
        MOONSHINE_LOG = cfg.logFilter;
        # What the user manager would have provided, set by hand as in
        # upstream's start-moonshine.sh: the Wayland socket goes in the
        # runtime dir, and apps are launched via the user manager's D-Bus API.
        XDG_RUNTIME_DIR = runtimeDir;
        DBUS_SESSION_BUS_ADDRESS = "unix:path=${runtimeDir}/bus";
      };
      serviceConfig = {
        User = cfg.user;
        # /dev/dri render nodes are video-group on NixOS. The `input` group is
        # deliberately NOT granted here: it only helps moonshine's own access
        # to /dev/uinput, not the streamed games, which moonshine launches via
        # the user's systemd manager and which get only the user's real groups.
        # Headless users who want gamepads must add the user to `input`.
        SupplementaryGroups = [
          "video"
        ];
        ExecStart = "${lib.getExe cfg.package} ${configFile}";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [
        (cfg.settings.webserver.port_https or 47984)
        (cfg.settings.webserver.port or 47989)
        (cfg.settings.stream.port or 48010)
      ];
      allowedUDPPorts = [
        (cfg.settings.stream.video.port or 47998)
        (cfg.settings.stream.control.port or 47999)
        (cfg.settings.stream.audio.port or 48000)
      ];
    };
  };
}
