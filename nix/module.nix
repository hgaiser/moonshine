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

    # Besides the binary, the package carries the Vulkan implicit-layer
    # manifest (share/vulkan/implicit_layer.d) that the loader must be able
    # to find; it only engages for apps moonshine launches
    # (ENABLE_MOONSHINE_WSI=1), so it is inert for everything else.
    environment.systemPackages = [ cfg.package ];

    # Grants the `input` group + active-seat ACLs on /dev/uinput and
    # /dev/uhid, which inputtino uses to create the virtual
    # gamepad/keyboard/mouse.
    services.udev.packages = [ cfg.package ];

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
        # /dev/uinput and /dev/uhid are input-group 0660 via the package's
        # udev rules (a system unit can grant supplementary groups directly,
        # so no extraGroups on the user).
        SupplementaryGroups = [
          "input"
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
