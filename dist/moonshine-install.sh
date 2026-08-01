#!/usr/bin/env bash
set -euo pipefail

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

trap 'echo -ne "${RESET}"' EXIT

info()  { echo -e "${BOLD}${CYAN}::${RESET} ${BOLD}$*${RESET}"; }
step()  { echo -e "  ${DIM}->${RESET} $*"; }
ok()    { echo -e " ${BOLD}${GREEN}✓${RESET} $*"; }
warn()  { echo -e " ${BOLD}${YELLOW} !${RESET} $*" >&2; }
die()   { echo -e " ${BOLD}${RED}✗${RESET} $*" >&2; exit 1; }

prompt() {
  local question="$1" var="$2" default="$3"
  local yn prompt_str

  if [[ "$default" == "Y" ]]; then
    prompt_str="  ${question} [Y/n] "
  else
    prompt_str="  ${question} [y/N] "
  fi

  if [[ -t 0 ]]; then
    read -r -p "$prompt_str" yn
    yn="${yn:-$default}"
  elif (</dev/tty true) 2>/dev/null; then
    read -r -p "$prompt_str" yn </dev/tty
    yn="${yn:-$default}"
  else
    echo -e "${prompt_str}${DIM}→ ${default} (non-interactive)${RESET}"
    yn="$default"
  fi

  [[ "$yn" =~ ^[Yy] ]] && eval "$var=true" || eval "$var=false"
}

prompt_text() {
  local question="$1" var="$2" default="$3"
  local val prompt_str="  ${question} [${default}] "

  if [[ -t 0 ]]; then
    read -r -p "$prompt_str" val
    val="${val:-$default}"
  elif (</dev/tty true) 2>/dev/null; then
    read -r -p "$prompt_str" val </dev/tty
    val="${val:-$default}"
  else
    echo -e "${prompt_str}${DIM}→ ${default} (non-interactive)${RESET}"
    val="$default"
  fi

  eval "$var=\$val"
}

# --- help ---

print_help() {
  echo "moonshine-install.sh — install moonshine"
  echo ""
  echo "Options:"
  echo "  --uninstall      Remove moonshine completely"
  echo "  --enable         Enable the service on boot (default: prompt)"
  echo "  --no-enable      Do not enable on boot"
  echo "  --linger         Enable lingering for headless use (default: prompt)"
  echo "  --no-linger      Do not enable lingering"
  echo "  --start          Start the service after install (default: prompt)"
  echo "  --no-start       Do not start after install"
  echo "  --healthcheck    Run a health check after install (default: prompt)"
  echo "  --no-healthcheck Skip the health check"
  echo "  --user USER      Install for this user (default: current user)"
  echo "  --help           Show this message"
  echo ""
  echo "Run as your normal user. The script will ask for sudo when needed."
  exit 0
}

[[ "${1:-}" == "--help" ]] || [[ "${1:-}" == "-h" ]] && print_help

# --- guards ---

if [[ $EUID -eq 0 ]]; then
  die "do not run with sudo. Run as your normal user."
fi

USER="$USER"

if [[ "$(uname -m)" != "x86_64" ]]; then
  die "moonshine only supports x86_64"
fi

if ! command -v sudo &>/dev/null; then
  die "sudo not found"
fi

# --- parse args ---

UNINSTALL=false
ENABLE_ON_BOOT=""
LINGER=""
START_NOW=""
HEALTHCHECK=""
TARGET_USER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --uninstall) UNINSTALL=true; shift ;;
    --enable) ENABLE_ON_BOOT=true; shift ;;
    --no-enable) ENABLE_ON_BOOT=false; shift ;;
    --linger) LINGER=true; shift ;;
    --no-linger) LINGER=false; shift ;;
    --start) START_NOW=true; shift ;;
    --no-start) START_NOW=false; shift ;;
    --healthcheck) HEALTHCHECK=true; shift ;;
    --no-healthcheck) HEALTHCHECK=false; shift ;;
    --user) TARGET_USER="$2"; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done

# --- uninstall ---

if [[ "$UNINSTALL" == "true" ]]; then
  info "Uninstalling moonshine"

  if systemctl is-active --quiet "moonshine@${USER}" 2>/dev/null; then
    step "Stopping moonshine@${USER}.service..."
    sudo systemctl stop "moonshine@${USER}" || true
  fi

  if systemctl is-enabled --quiet "moonshine@${USER}" 2>/dev/null; then
    step "Disabling moonshine@${USER}.service..."
    sudo systemctl disable "moonshine@${USER}" || true
  fi

  step "Removing files..."
  sudo rm -rf /opt/moonshine
  sudo rm -f /etc/systemd/system/moonshine@.service
  sudo rm -f /etc/udev/rules.d/60-moonshine.rules
  sudo rm -f /etc/modules-load.d/moonshine.conf
  sudo rm -f /etc/sysusers.d/moonshine.conf
  sudo rm -f /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json
  sudo rm -f /etc/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules
  sudo rm -f /etc/profile.d/moonshine.sh
  sudo rm -f /etc/atomic-update.conf.d/moonshine.conf
  sudo systemctl daemon-reload || true
  sudo udevadm control --reload || true

  ok "moonshine uninstalled"
  exit 0
fi

# --- download ---

VERSION="__VERSION__"
if [[ "$VERSION" == "__VERSION__" ]]; then
  info "Resolving latest release"
  VERSION=$(curl -fsSL "https://api.github.com/repos/hgaiser/moonshine/releases/latest" \
    | grep -oP '"tag_name":\s*"\K[^"]+')
  if [[ -z "$VERSION" ]]; then
    die "could not resolve latest version from GitHub API"
  fi
fi

info "Installing moonshine ${VERSION}"

URL="https://github.com/hgaiser/moonshine/releases/download/${VERSION}/moonshine-${VERSION}-linux-amd64.tar.zst"
step "Downloading ${DIM}${URL}${RESET}"
TMPFILE="$(mktemp)"
curl -fsSL --retry 3 -o "$TMPFILE" "$URL"

SIZE=$(du -h "$TMPFILE" | cut -f1)
step "Downloaded (${SIZE})"

step "Extracting..."
TMPDIR="$(mktemp -d)"
tar --zstd -xf "$TMPFILE" -C "$TMPDIR"
S="$TMPDIR/moonshine"

echo ""

# --- prompts ---

if [[ -z "$TARGET_USER" ]]; then
  prompt_text "Install for which user?" TARGET_USER "$USER"
fi
USER="$TARGET_USER"

USER_UID=$(id -u "$USER" 2>/dev/null || true)
if [[ -z "${USER_UID:-}" ]]; then
  die "user '$USER' does not exist"
fi

if loginctl show-user "$USER" -p Linger 2>/dev/null | grep -q "Linger=yes"; then
  LINGER_NEEDED=false
  step "Linger already enabled for ${USER}"
else
  LINGER_NEEDED=true
fi

echo ""

if [[ -z "$ENABLE_ON_BOOT" ]]; then
  prompt "Enable on boot?" ENABLE_ON_BOOT Y
fi

if $LINGER_NEEDED && [[ -z "$LINGER" ]]; then
  prompt "Run while logged out?" LINGER Y
elif [[ -z "$LINGER" ]]; then
  LINGER=false
fi

if [[ -z "$START_NOW" ]]; then
  prompt "Start now?" START_NOW Y
fi

if [[ -z "$HEALTHCHECK" ]]; then
  prompt "Run health check?" HEALTHCHECK Y
fi

# --- build and run privileged commands ---

MOONSHINE_HOME="/opt/moonshine"

CMDS=(
  # Stop existing service
  "systemctl stop 'moonshine@${USER}' 2>/dev/null || true"

  # Create directories
  "mkdir -p '${MOONSHINE_HOME}/bin'"
  "mkdir -p /etc/udev/rules.d"
  "mkdir -p /etc/modules-load.d"
  "mkdir -p /etc/sysusers.d"
  "mkdir -p /etc/vulkan/implicit_layer.d"
  "mkdir -p /etc/polkit-1/rules.d"
  # Deploy moonshine binary
  "cp '${S}/bin/moonshine' '${MOONSHINE_HOME}/bin/moonshine'"
  "chmod 755 '${MOONSHINE_HOME}/bin/moonshine'"

  # Deploy Vulkan WSI layer
  "mkdir -p '${MOONSHINE_HOME}/lib'"
  "cp '${S}/lib/moonshine/vulkan-layers/libmoonshine_wsi.so' '${MOONSHINE_HOME}/lib/libmoonshine_wsi.so'"
  "chmod 755 '${MOONSHINE_HOME}/lib/libmoonshine_wsi.so'"

  # Deploy the install script itself for future upgrades/uninstall
  "cp '${S}/bin/moonshine-install.sh' '${MOONSHINE_HOME}/bin/moonshine-install.sh'"
  "chmod 755 '${MOONSHINE_HOME}/bin/moonshine-install.sh'"

  # Deploy start script with path patched
  "sed 's|/usr/bin/moonshine|${MOONSHINE_HOME}/bin/moonshine|g' '${S}/share/moonshine/start-moonshine.sh' > '${MOONSHINE_HOME}/start-moonshine.sh'"
  "chmod 755 '${MOONSHINE_HOME}/start-moonshine.sh'"

  # Deploy service unit with path patched
  "sed 's|/usr/bin/start-moonshine.sh|${MOONSHINE_HOME}/start-moonshine.sh|' '${S}/share/moonshine/moonshine@.service' > /etc/systemd/system/moonshine@.service"

  # Deploy udev rules
  "cp '${S}/share/moonshine/60-moonshine.rules' /etc/udev/rules.d/60-moonshine.rules"

  # Deploy modules-load config
  "cp '${S}/share/moonshine/moonshine-modules.conf' /etc/modules-load.d/moonshine.conf"

  # Deploy sysusers config
  "cp '${S}/share/moonshine/moonshine-sysusers.conf' /etc/sysusers.d/moonshine.conf"

  # Deploy Vulkan layer manifest with library_path patched
  "sed 's|/usr/lib/moonshine/vulkan-layers/libmoonshine_wsi.so|${MOONSHINE_HOME}/lib/libmoonshine_wsi.so|' '${S}/share/moonshine/VkLayer_moonshine_wsi.json' > /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json"

  # Deploy polkit sleep-inhibit rules
  "cp '${S}/share/moonshine/50-moonshine-inhibit-sleep.rules' /etc/polkit-1/rules.d/50-moonshine-inhibit-sleep.rules"

  # Create profile.d for PATH
  "echo 'export PATH=\"\${PATH}:${MOONSHINE_HOME}/bin\"' > /etc/profile.d/moonshine.sh"

  # Preserve /etc integration across SteamOS atomic updates
  "if [[ -d /etc/atomic-update.conf.d ]]; then cp '${S}/share/moonshine/moonshine-atomic-update.conf' /etc/atomic-update.conf.d/moonshine.conf; fi"

  # Reload systemd
  "systemctl daemon-reload"

  # Apply sysusers (creates moonshine group)
  "systemd-sysusers || true"

  # Reload udev rules
  "udevadm control --reload || true"
  "udevadm trigger || true"

  # Load kernel modules
  "modprobe uinput || true"
  "modprobe uhid || true"

  # Reload polkit
  "systemctl reload-or-restart polkit.service || true"
)

if $LINGER_NEEDED && [[ "$LINGER" == "true" ]]; then
  CMDS+=("loginctl enable-linger '${USER}' || true")
fi

if [[ "$HEALTHCHECK" == "true" ]]; then
  CMDS+=("echo ''")
  CMDS+=("echo ':: Running health check'")
  CMDS+=("systemd-run --quiet --wait --pipe -p 'User=${USER}' -p 'SupplementaryGroups=input' -p 'SupplementaryGroups=moonshine' -p 'DeviceAllow=/dev/uinput rw' -p 'DeviceAllow=/dev/uhid rw' -p 'DeviceAllow=char-drm rw' -p 'DeviceAllow=char-nvidia rw' -p 'DeviceAllow=char-nvidia-uvm rw' '${MOONSHINE_HOME}/start-moonshine.sh' healthcheck || true")
fi

if [[ "$ENABLE_ON_BOOT" == "true" ]] || [[ "$START_NOW" == "true" ]]; then
  if [[ "$ENABLE_ON_BOOT" == "true" ]] && [[ "$START_NOW" == "true" ]]; then
    CMDS+=("systemctl enable --now 'moonshine@${USER}'")
  elif [[ "$ENABLE_ON_BOOT" == "true" ]]; then
    CMDS+=("systemctl enable 'moonshine@${USER}'")
  else
    CMDS+=("systemctl start 'moonshine@${USER}'")
  fi
fi

echo ""
echo -e "  ${DIM}The following commands will be run with sudo:${RESET}"
echo ""
for cmd in "${CMDS[@]}"; do
  echo "  $cmd"
done
echo ""

sudo bash -c "$(printf '%s\n' "${CMDS[@]}")"

# Cleanup temp files
rm -rf "$TMPDIR" "$TMPFILE"

ok "moonshine ${VERSION} installed"

if [[ "$ENABLE_ON_BOOT" != "true" ]] && [[ "$START_NOW" != "true" ]]; then
  echo ""
  step "Service not started. Run when ready:"
  echo -e "    ${BOLD}systemctl start moonshine@${USER}${RESET}"
elif [[ "$START_NOW" != "true" ]]; then
  echo ""
  step "Service enabled but not started. Run when ready:"
  echo -e "    ${BOLD}systemctl start moonshine@${USER}${RESET}"
fi

echo ""
echo -e "  ${DIM}status${RESET}  ${BOLD}systemctl status moonshine@${USER}${RESET}"
echo -e "  ${DIM}config${RESET}  ${BOLD}/home/${USER}/.config/moonshine/config.toml${RESET}"
echo ""

ok "Done"
