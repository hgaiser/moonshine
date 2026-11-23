#!/usr/bin/env bash
# Install, upgrade or remove moonshine. It picks the installation method for the system it runs on:
#
#   aur     Arch and Arch-based distros: the moonshine-bin AUR package
#   deb     Debian, Ubuntu and derivatives: the release .deb
#   rpm     Fedora, RHEL and derivatives: the release .rpm
#   ostree  Fedora Atomic (Bazzite, Silverblue, Kinoite, ...): the release .rpm, layered
#   opt     SteamOS and any other distro: the release tarball, unpacked into /opt/moonshine
#
# An existing installation keeps its method. NixOS is not installed by this script: it uses the
# services.moonshine module.
#
# Usage: curl -fsSL https://github.com/hgaiser/moonshine/releases/latest/download/moonshine-install.sh | bash
set -euo pipefail

# The release workflow writes the release tag here. Otherwise the script uses the latest release.
VERSION="__VERSION__"
REPO="hgaiser/moonshine"
MIN_GLIBC="2.39"   # the glibc of the release build (ubuntu-24.04 in .github/workflows/release.yaml)
MOONSHINE_HOME="/opt/moonshine"
USER="${USER:-$(id -un)}"

# --- terminal styling ---

if [[ -t 1 ]]; then
  BOLD='\033[1m'
  DIM='\033[2m'
  CYAN='\033[36m'
  GREEN='\033[32m'
  YELLOW='\033[33m'
  RED='\033[31m'
  RESET='\033[0m'
else
  BOLD='' DIM='' CYAN='' GREEN='' YELLOW='' RED='' RESET=''
fi

info()  { echo -e "${BOLD}${CYAN}::${RESET} ${BOLD}$*${RESET}"; }
step()  { echo -e "  ${DIM}->${RESET} $*"; }
ok()    { echo -e " ${BOLD}${GREEN}✓${RESET} $*"; }
warn()  { echo -e " ${BOLD}${YELLOW} !${RESET} $*" >&2; }
die()   { echo -e " ${BOLD}${RED}✗${RESET} $*" >&2; exit 1; }

# When the script is piped into bash, stdin is the script itself. Interactive commands read the
# terminal instead, or nothing when there is no terminal.
has_tty() { (</dev/tty true) 2>/dev/null; }
with_tty() { if has_tty; then "$@" </dev/tty; else "$@" </dev/null; fi; }

prompt() {
  local question="$1" var="$2" default="$3"
  local yn prompt_str

  if [[ "$default" == "Y" ]]; then
    prompt_str="  ${question} [Y/n] "
  else
    prompt_str="  ${question} [y/N] "
  fi

  if has_tty; then
    read -r -p "$prompt_str" yn </dev/tty
    yn="${yn:-$default}"
  else
    echo -e "${prompt_str}${DIM}→ ${default} (non-interactive)${RESET}"
    yn="$default"
  fi

  if [[ "$yn" =~ ^[Yy] ]]; then printf -v "$var" true; else printf -v "$var" false; fi
}

prompt_text() {
  local question="$1" var="$2" default="$3"
  local val prompt_str="  ${question} [${default}] "

  if has_tty; then
    read -r -p "$prompt_str" val </dev/tty
    val="${val:-$default}"
  else
    echo -e "${prompt_str}${DIM}→ ${default} (non-interactive)${RESET}"
    val="$default"
  fi

  printf -v "$var" '%s' "$val"
}

# --- help ---

print_help() {
  cat <<EOF
moonshine-install.sh — install, upgrade or remove moonshine

The script picks the installation method for this system. Run it again to upgrade.

Options:
  --method METHOD   Use this method: aur, deb, rpm, ostree or opt (default: detect)
  --dry-run         Show the method and the commands, but change nothing
  --uninstall       Remove moonshine completely
  --enable          Enable the service on boot (default: prompt)
  --no-enable       Do not enable on boot
  --linger          Enable lingering for headless use (default: prompt)
  --no-linger       Do not enable lingering
  --input-group     Add the user to the input group, for gamepads when headless (default: prompt)
  --no-input-group  Do not change the user's groups
  --start           Start the service after install (default: prompt)
  --no-start        Do not start after install
  --healthcheck     Run a health check after install (default: prompt)
  --no-healthcheck  Skip the health check
  --user USER       Install for this user (default: current user)
  --help            Show this message

Run as your normal user. The script asks for sudo when needed.
EOF
  exit 0
}

# --- system detection ---

# shellcheck disable=SC1091
os_field() { ( . /etc/os-release 2>/dev/null; eval "printf '%s' \"\${$1:-}\"" ); }
OS_ID="$(os_field ID)"
OS_LIKE="$(os_field ID_LIKE)"
OS_NAME="$(os_field PRETTY_NAME)"
OS_NAME="${OS_NAME:-${OS_ID:-this system}}"

# True when the distro is, or derives from, one of the given IDs.
os_is() {
  local id
  for id in "$@"; do
    [[ " $OS_ID $OS_LIKE " == *" $id "* ]] && return 0
  done
  return 1
}

is_ostree() { [[ -e /run/ostree-booted ]]; }

# The method of an existing installation, if any.
installed_method() {
  if [[ -x "${MOONSHINE_HOME}/bin/moonshine" ]]; then
    echo opt
  elif command -v pacman &>/dev/null && pacman -Q moonshine-bin &>/dev/null; then
    echo aur
  elif is_ostree && rpm-ostree status 2>/dev/null | grep -qE '(^|[[:space:]])moonshine(-[0-9][^[:space:]]*)?([[:space:]]|$)'; then
    echo ostree
  elif command -v dpkg-query &>/dev/null && dpkg-query -W -f='${Status}' moonshine 2>/dev/null | grep -q 'install ok installed'; then
    echo deb
  elif ! is_ostree && command -v rpm &>/dev/null && rpm -q moonshine &>/dev/null; then
    echo rpm
  fi
}

# Other packages (such as a moonshine you built from the AUR or nixpkgs) that this script must not touch.
foreign_package() {
  if command -v pacman &>/dev/null; then
    # Not 'pacman -Q moonshine': that also finds moonshine-bin, which provides moonshine.
    pacman -Qq 2>/dev/null | grep -xE 'moonshine|moonshine-git' | head -n1 || true
  fi
}

system_method() {
  if [[ "$OS_ID" == "nixos" ]]; then echo nixos
  elif [[ "$OS_ID" == "steamos" ]]; then echo opt          # read-only Arch: use /opt, not pacman
  elif is_ostree; then echo ostree
  elif os_is arch; then echo aur
  elif os_is debian ubuntu; then echo deb
  elif os_is fedora rhel centos; then echo rpm
  else echo opt
  fi
}

# --- download ---

need_curl() { command -v curl &>/dev/null || die "curl not found. Install curl first."; }

resolve_version() {
  if [[ "$VERSION" == "__VERSION__" ]]; then
    need_curl
    step "Resolving the latest release..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep -oP '"tag_name":\s*"\K[^"]+' || true)
    [[ -n "$VERSION" ]] || die "could not resolve the latest version from the GitHub API"
  fi
}

# Download a release asset into $WORKDIR and verify it against the release's SHA256SUMS.
fetch() {
  local asset="$1" base="https://github.com/${REPO}/releases/download/${VERSION}"
  need_curl

  if [[ ! -f "$WORKDIR/SHA256SUMS" ]]; then
    if ! curl -fsSL --retry 3 -o "$WORKDIR/SHA256SUMS" "$base/SHA256SUMS"; then
      rm -f "$WORKDIR/SHA256SUMS"
      die "could not download SHA256SUMS for ${VERSION}. Releases before checksums: install manually (see the README)."
    fi
  fi

  step "Downloading ${DIM}${base}/${asset}${RESET}"
  curl -fsSL --retry 3 -o "$WORKDIR/$asset" "$base/$asset" || die "could not download ${asset}"

  local line="" sum name
  while read -r sum name; do
    [[ "${name#\*}" == "$asset" ]] && line="$sum  $asset"
  done <"$WORKDIR/SHA256SUMS"
  [[ -n "$line" ]] || die "SHA256SUMS has no checksum for ${asset}"
  (cd "$WORKDIR" && sha256sum --quiet -c - <<<"$line") || die "checksum mismatch for ${asset}"
  step "Verified $(du -h "$WORKDIR/$asset" | cut -f1) against SHA256SUMS"
}

# --- methods ---
#
# Each install_<method> adds its root commands to CMDS, except aur: the AUR helper runs as the user.

install_aur() {
  local helper
  for helper in paru yay ""; do
    [[ -z "$helper" ]] || command -v "$helper" &>/dev/null && break
  done

  if [[ -n "$helper" ]]; then
    step "Running: ${BOLD}${helper} -S --needed moonshine-bin${RESET}"
    $DRY_RUN || with_tty "$helper" -S --needed moonshine-bin
  else
    step "No AUR helper found: building moonshine-bin with makepkg"
    step "Running: ${BOLD}sudo pacman -S --needed git base-devel${RESET}"
    step "Running: ${BOLD}git clone https://aur.archlinux.org/moonshine-bin.git && makepkg -si${RESET}"
    if ! $DRY_RUN; then
      with_tty sudo pacman -S --needed git base-devel
      git clone --quiet https://aur.archlinux.org/moonshine-bin.git "$WORKDIR/moonshine-bin"
      (cd "$WORKDIR/moonshine-bin" && with_tty makepkg -si)
    fi
  fi
}

install_deb() {
  local deb="moonshine_${VERSION#v}-1_amd64.deb"
  $DRY_RUN || fetch "$deb"
  CMDS+=("DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall '$WORKDIR/$deb'")
}

install_rpm() {
  local rpm="moonshine-${VERSION#v}-1.x86_64.rpm"
  $DRY_RUN || fetch "$rpm"
  CMDS+=("dnf install -y '$WORKDIR/$rpm'")
}

install_ostree() {
  local rpm="moonshine-${VERSION#v}-1.x86_64.rpm"
  $DRY_RUN || fetch "$rpm"
  if [[ "$INSTALLED" == "ostree" ]]; then
    # A layered package cannot be installed again: swap the old one for the new one.
    CMDS+=("rpm-ostree uninstall moonshine --install '$WORKDIR/$rpm'")
  else
    CMDS+=("rpm-ostree install '$WORKDIR/$rpm'")
  fi
}

install_opt() {
  local tarball="moonshine-${VERSION}-linux-amd64.tar.zst" S="$WORKDIR/moonshine"
  command -v zstd &>/dev/null || die "zstd not found. Install zstd first."
  if ! $DRY_RUN; then
    fetch "$tarball"
    step "Extracting..."
    tar --zstd -xf "$WORKDIR/$tarball" -C "$WORKDIR"
  fi

  CMDS+=(
    # Create directories
    "mkdir -p '${MOONSHINE_HOME}/bin' /etc/udev/rules.d /etc/modules-load.d /etc/sysusers.d /etc/polkit-1/rules.d /etc/vulkan/implicit_layer.d"

    # Deploy moonshine binary
    "install -m755 '${S}/bin/moonshine' '${MOONSHINE_HOME}/bin/moonshine'"

    # Remove the moonshine-wsi layer of older releases; ferriscope-wsi replaces it
    "rm -f /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json '${MOONSHINE_HOME}/lib/libmoonshine_wsi.so'"

    # Deploy the ferriscope WSI layer; its manifest names the library under /usr,
    # so point it at the copy here
    "install -Dm755 '${S}/lib/ferriscope/libferriscope_wsi.so' '${MOONSHINE_HOME}/lib/ferriscope/libferriscope_wsi.so'"
    "sed 's|/usr/lib/ferriscope/|${MOONSHINE_HOME}/lib/ferriscope/|' '${S}/share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json' > /etc/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json"

    # Deploy the gamescope wrapper (see TIPS.md)
    "install -Dm755 '${S}/bin/moonshine-gamescope' '${MOONSHINE_HOME}/bin/moonshine-gamescope'"

    # Deploy the install script itself for future upgrades/uninstall
    "install -m755 '${S}/bin/moonshine-install.sh' '${MOONSHINE_HOME}/bin/moonshine-install.sh'"

    # Deploy start script with path patched
    "sed 's|/usr/bin/moonshine|${MOONSHINE_HOME}/bin/moonshine|g' '${S}/share/moonshine/start-moonshine.sh' > '${MOONSHINE_HOME}/start-moonshine.sh'"
    "chmod 755 '${MOONSHINE_HOME}/start-moonshine.sh'"

    # Deploy service unit with path patched
    "sed 's|/usr/bin/start-moonshine.sh|${MOONSHINE_HOME}/start-moonshine.sh|' '${S}/share/moonshine/moonshine@.service' > /etc/systemd/system/moonshine@.service"

    # Deploy udev rules, modules-load config, sysusers config and polkit sleep-inhibit rules
    "cp '${S}/share/moonshine/60-moonshine.rules' /etc/udev/rules.d/60-moonshine.rules"
    "cp '${S}/share/moonshine/moonshine-modules.conf' /etc/modules-load.d/moonshine.conf"
    "cp '${S}/share/moonshine/moonshine-sysusers.conf' /etc/sysusers.d/moonshine.conf"
    "cp '${S}/share/moonshine/50-moonshine-inhibit-sleep.rules' /etc/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules"

    # Create profile.d for PATH
    "echo 'export PATH=\"\${PATH}:${MOONSHINE_HOME}/bin\"' > /etc/profile.d/moonshine.sh"

    # Preserve /etc integration across SteamOS atomic updates
    "if [[ -d /etc/atomic-update.conf.d ]]; then cp '${S}/share/moonshine/moonshine-atomic-update.conf' /etc/atomic-update.conf.d/moonshine.conf; fi"

    # The packages do these steps in their post-install scripts.
    "systemctl daemon-reload"
    "systemd-sysusers || true"
    "udevadm control --reload || true"
    "udevadm trigger || true"
    "modprobe uinput || true"
    "modprobe uhid || true"
    "systemctl reload-or-restart polkit.service || true"
  )
}

uninstall_cmds() {
  case "$1" in
    aur)    CMDS+=("pacman -Rns --noconfirm moonshine-bin") ;;
    deb)    CMDS+=("apt-get remove -y moonshine") ;;
    rpm)    CMDS+=("dnf remove -y moonshine") ;;
    ostree) CMDS+=("rpm-ostree uninstall moonshine") ;;
    opt)
      CMDS+=(
        "rm -rf '${MOONSHINE_HOME}'"
        "rm -f /etc/systemd/system/moonshine@.service"
        "rm -f /etc/udev/rules.d/60-moonshine.rules"
        "rm -f /etc/modules-load.d/moonshine.conf"
        "rm -f /etc/sysusers.d/moonshine.conf"
        "rm -f /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json"
        "rm -f /etc/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json"
        "rm -f /etc/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules"
        "rm -f /etc/profile.d/moonshine.sh"
        "rm -f /etc/atomic-update.conf.d/moonshine.conf"
        "systemctl daemon-reload || true"
        "udevadm control --reload || true"
      )
      ;;
  esac
}

# Run the collected root commands in one sudo call, after showing them.
run_cmds() {
  echo ""
  if $DRY_RUN; then
    echo -e "  ${DIM}Dry run. These commands would run with sudo:${RESET}"
  else
    echo -e "  ${DIM}The following commands will be run with sudo:${RESET}"
  fi
  echo ""
  local cmd
  for cmd in "${CMDS[@]}"; do
    echo "  $cmd"
  done
  echo ""
  $DRY_RUN || sudo bash -c "set -e; $(printf '%s\n' "${CMDS[@]}")" </dev/null
}

print_nixos() {
  info "NixOS: use the services.moonshine module"
  echo ""
  echo "  This script does not install on NixOS. Add this to your configuration.nix:"
  echo ""
  echo "    services.moonshine = {"
  echo "      enable = true;"
  echo "      user = \"${USER}\";"
  echo "    };"
  echo ""
  echo -e "  See ${BOLD}https://github.com/${REPO}#nix${RESET} for the application settings that NixOS needs."
  exit 0
}

# --- main ---

main() {
  local METHOD="" UNINSTALL=false ENABLE_ON_BOOT="" LINGER="" INPUT_GROUP="" START_NOW="" HEALTHCHECK="" TARGET_USER=""
  DRY_RUN=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      -h|--help) print_help ;;
      --method) METHOD="${2:-}"; shift 2 ;;
      --dry-run) DRY_RUN=true; shift ;;
      --uninstall) UNINSTALL=true; shift ;;
      --enable) ENABLE_ON_BOOT=true; shift ;;
      --no-enable) ENABLE_ON_BOOT=false; shift ;;
      --linger) LINGER=true; shift ;;
      --no-linger) LINGER=false; shift ;;
      --input-group) INPUT_GROUP=true; shift ;;
      --no-input-group) INPUT_GROUP=false; shift ;;
      --start) START_NOW=true; shift ;;
      --no-start) START_NOW=false; shift ;;
      --healthcheck) HEALTHCHECK=true; shift ;;
      --no-healthcheck) HEALTHCHECK=false; shift ;;
      --user) TARGET_USER="${2:-}"; shift 2 ;;
      *) die "unknown option: $1" ;;
    esac
  done

  case "$METHOD" in
    ""|aur|deb|rpm|ostree|opt) ;;
    *) die "unknown method: ${METHOD} (use aur, deb, rpm, ostree or opt)" ;;
  esac

  # --- guards ---

  [[ $EUID -ne 0 ]] || die "do not run with sudo. Run as your normal user."
  [[ "$(uname -m)" == "x86_64" ]] || die "moonshine only supports x86_64"

  if [[ -z "$METHOD" && "$OS_ID" == "nixos" ]]; then
    print_nixos
  fi

  command -v sudo &>/dev/null || die "sudo not found"
  [[ -d /run/systemd/system ]] || die "moonshine needs systemd, and systemd is not running"

  INSTALLED="$(installed_method)"
  WORKDIR="$(mktemp -d)"
  trap 'rm -rf "$WORKDIR"; echo -ne "${RESET}"' EXIT
  CMDS=()

  # --- uninstall ---

  if $UNINSTALL; then
    local method="${METHOD:-$INSTALLED}"
    [[ -n "$method" ]] || die "moonshine is not installed by this script"
    info "Uninstalling moonshine (${method})"

    local unit
    while read -r unit _; do
      CMDS+=("systemctl disable --now '${unit}' || true")
    done < <(systemctl list-units --all --plain --no-legend 'moonshine@*.service' 2>/dev/null)
    uninstall_cmds "$method"
    run_cmds

    if $DRY_RUN; then
      ok "Dry run finished"
    elif [[ "$method" == "ostree" ]]; then
      ok "moonshine removed from the next deployment. Reboot to finish."
    else
      ok "moonshine uninstalled"
    fi
    exit 0
  fi

  # --- choose the method ---

  local foreign
  foreign="$(foreign_package)"
  if [[ -n "$foreign" && -z "$METHOD" ]]; then
    die "moonshine is installed as the pacman package '${foreign}'. Upgrade it with your package manager, or remove it first."
  fi

  local reason
  if [[ -n "$METHOD" ]]; then
    reason="chosen with --method"
  elif [[ -n "$INSTALLED" ]]; then
    METHOD="$INSTALLED"; reason="the method of the current installation"
  else
    METHOD="$(system_method)"; reason="detected for ${OS_NAME}"
    if [[ "$METHOD" == "opt" && "$OS_ID" != "steamos" ]]; then
      warn "No moonshine package for ${OS_NAME}. Installing the release tarball into ${MOONSHINE_HOME}."
    fi
  fi
  if [[ -n "$INSTALLED" && "$INSTALLED" != "$METHOD" ]]; then
    die "moonshine is already installed with the '${INSTALLED}' method. Run with --uninstall first."
  fi

  # The release binary links against the glibc of its build system. The AUR package is the same binary.
  local glibc
  glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
  glibc="${glibc#glibc }"
  [[ -n "$glibc" ]] || die "moonshine needs glibc, and this system does not use it"
  if [[ "$(printf '%s\n' "$MIN_GLIBC" "$glibc" | sort -V | head -n1)" != "$MIN_GLIBC" ]]; then
    die "moonshine needs glibc ${MIN_GLIBC} or newer, and ${OS_NAME} has ${glibc}. Upgrade the distro, or build from source (see the README)."
  fi

  [[ "$METHOD" == "aur" ]] || resolve_version
  if [[ "$METHOD" == "aur" ]]; then
    info "Installing moonshine from the AUR (${reason})"
  else
    info "Installing moonshine ${VERSION} with the '${METHOD}' method (${reason})"
  fi
  echo ""

  # --- prompts ---

  if [[ -z "$TARGET_USER" ]]; then
    prompt_text "Install for which user?" TARGET_USER "$USER"
  fi
  local user="$TARGET_USER"
  id -u "$user" &>/dev/null || die "user '$user' does not exist"

  local linger_needed=true
  if loginctl show-user "$user" -p Linger 2>/dev/null | grep -q "Linger=yes"; then
    linger_needed=false
    step "Linger already enabled for ${user}"
  fi

  local input_needed=true
  if id -nG "$user" | tr ' ' '\n' | grep -qx input; then
    input_needed=false
    step "${user} is already in the input group"
  fi

  echo ""

  # An rpm-ostree deployment is only active after a reboot: the service cannot run before then.
  local reboot_needed=false
  [[ "$METHOD" == "ostree" ]] && reboot_needed=true

  if [[ -z "$ENABLE_ON_BOOT" ]]; then
    prompt "Enable on boot?" ENABLE_ON_BOOT Y
  fi
  if $linger_needed && [[ -z "$LINGER" ]]; then
    prompt "Run while logged out?" LINGER Y
  fi
  if $input_needed && [[ -z "$INPUT_GROUP" ]]; then
    prompt "Allow virtual gamepads when streaming headless (adds ${user} to the input group)?" INPUT_GROUP Y
  fi
  if $reboot_needed; then
    START_NOW=false; HEALTHCHECK=false
  else
    if [[ -z "$START_NOW" ]]; then
      prompt "Start now?" START_NOW Y
    fi
    if [[ -z "$HEALTHCHECK" ]]; then
      prompt "Run health check?" HEALTHCHECK Y
    fi
  fi
  echo ""

  # --- install ---

  local start_script="/usr/bin/start-moonshine.sh"
  [[ "$METHOD" == "opt" ]] && start_script="${MOONSHINE_HOME}/start-moonshine.sh"

  # Root commands outside the user's temporary directory need read access to it.
  chmod 755 "$WORKDIR"

  # The AUR helper runs at once, before the root commands: the stop and start below then restart the
  # service on the new binary.
  CMDS+=("systemctl stop 'moonshine@${user}' 2>/dev/null || true")
  "install_${METHOD}"

  if $linger_needed && [[ "$LINGER" == "true" ]]; then
    CMDS+=("loginctl enable-linger '${user}' || true")
  fi

  if $input_needed && [[ "$INPUT_GROUP" == "true" ]]; then
    # Fedora Atomic keeps system groups in /usr/lib/group, which usermod cannot change.
    CMDS+=("if ! grep -q '^input:' /etc/group && [[ -f /usr/lib/group ]]; then grep '^input:' /usr/lib/group >> /etc/group; fi")
    CMDS+=("usermod -aG input '${user}'")
  fi

  if [[ "$HEALTHCHECK" == "true" ]]; then
    CMDS+=("echo ''")
    CMDS+=("echo ':: Running health check'")
    CMDS+=("systemd-run --quiet --wait --pipe -p 'User=${user}' -p 'SupplementaryGroups=moonshine' -p 'DeviceAllow=/dev/uinput rw' -p 'DeviceAllow=/dev/uhid rw' -p 'DeviceAllow=char-drm rw' -p 'DeviceAllow=char-nvidia rw' -p 'DeviceAllow=char-nvidia-uvm rw' '${start_script}' healthcheck || true")
  fi

  if ! $reboot_needed; then
    if [[ "$ENABLE_ON_BOOT" == "true" && "$START_NOW" == "true" ]]; then
      CMDS+=("systemctl enable --now 'moonshine@${user}'")
    elif [[ "$ENABLE_ON_BOOT" == "true" ]]; then
      CMDS+=("systemctl enable 'moonshine@${user}'")
    elif [[ "$START_NOW" == "true" ]]; then
      CMDS+=("systemctl start 'moonshine@${user}'")
    fi
  fi

  run_cmds

  if $DRY_RUN; then
    ok "Dry run finished"
    exit 0
  fi

  if [[ "$METHOD" == "aur" ]]; then
    ok "moonshine installed"
  else
    ok "moonshine ${VERSION} installed"
  fi

  echo ""
  if $reboot_needed; then
    step "Reboot to activate the new deployment. Then run:"
    [[ "$ENABLE_ON_BOOT" == "true" ]] \
      && echo -e "    ${BOLD}sudo systemctl enable --now moonshine@${user}${RESET}" \
      || echo -e "    ${BOLD}sudo systemctl start moonshine@${user}${RESET}"
    echo -e "    ${BOLD}moonshine healthcheck${RESET}"
  elif [[ "$START_NOW" != "true" ]]; then
    step "Service not started. Run when ready:"
    echo -e "    ${BOLD}sudo systemctl start moonshine@${user}${RESET}"
  fi
  if $input_needed && [[ "$INPUT_GROUP" == "true" ]]; then
    step "Log out and back in (or reboot) so the input group takes effect"
  fi

  echo ""
  echo -e "  ${DIM}status${RESET}         ${BOLD}systemctl status moonshine@${user}${RESET}"
  echo -e "  ${DIM}control panel${RESET}  ${BOLD}http://localhost:47990${RESET}"
  echo -e "  ${DIM}config${RESET}         ${BOLD}$(getent passwd "$user" | cut -d: -f6)/.config/moonshine/config.toml${RESET}"
  echo ""

  ok "Done"
}

# Everything runs from main, so bash has read the whole script before it starts: this matters when
# the script is piped into bash.
main "$@"
