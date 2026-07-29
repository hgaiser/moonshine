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
  local yn

  if [[ ! -t 0 ]]; then
    eval "$var=$default"
    return
  fi

  if [[ "$default" == "Y" ]]; then
    read -r -p "  ${question} [Y/n] " yn
    yn="${yn:-Y}"
  else
    read -r -p "  ${question} [y/N] " yn
    yn="${yn:-N}"
  fi

  if [[ "$yn" =~ ^[Yy] ]]; then
    eval "$var=true"
  else
    eval "$var=false"
  fi
}

# --- help ---

print_help() {
  echo "moonshine-install.sh — install moonshine via systemd-sysext"
  echo ""
  echo "  curl -fsSL https://raw.githubusercontent.com/hgaiser/moonshine/main/dist/moonshine-install.sh | bash"
  echo ""
  echo "Options:"
  echo "  --version VER    Install a specific version (e.g. v0.13.0)"
  echo "  --uninstall      Remove moonshine completely"
  echo "  --enable         Enable the service on boot (default: prompt)"
  echo "  --no-enable      Do not enable on boot"
  echo "  --linger         Enable lingering for headless use (default: prompt)"
  echo "  --no-linger      Do not enable lingering"
  echo "  --start          Start the service after install (default: prompt)"
  echo "  --no-start       Do not start after install"
  echo "  --healthcheck    Run a health check after install (default: prompt)"
  echo "  --no-healthcheck Skip the health check"
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

if ! command -v systemd-sysext &>/dev/null; then
  die "systemd-sysext not found (systemd >= 248 required)"
fi

if systemd-detect-virt --container --quiet 2>/dev/null; then
  die "cannot run inside a container (sysext uses overlayfs)"
fi

if ! command -v sudo &>/dev/null; then
  die "sudo not found"
fi

# --- parse args ---

UNINSTALL=false
REQUESTED_VERSION=""
ENABLE_ON_BOOT=""
LINGER=""
START_NOW=""
HEALTHCHECK=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --uninstall) UNINSTALL=true; shift ;;
    --version) REQUESTED_VERSION="$2"; shift 2 ;;
    --enable) ENABLE_ON_BOOT=true; shift ;;
    --no-enable) ENABLE_ON_BOOT=false; shift ;;
    --linger) LINGER=true; shift ;;
    --no-linger) LINGER=false; shift ;;
    --start) START_NOW=true; shift ;;
    --no-start) START_NOW=false; shift ;;
    --healthcheck) HEALTHCHECK=true; shift ;;
    --no-healthcheck) HEALTHCHECK=false; shift ;;    *) die "unknown option: $1" ;;
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

  if systemd-sysext list 2>/dev/null | grep -q moonshine; then
    step "Unmerging sysext..."
    sudo systemd-sysext unmerge moonshine.raw || true
  fi

  if [[ -f /var/lib/extensions/moonshine.raw ]]; then
    step "Removing extension image..."
    sudo rm -f /var/lib/extensions/moonshine.raw
  fi

  sudo udevadm control --reload || true

  ok "moonshine uninstalled"
  exit 0
fi

# --- download ---

if [[ -n "$REQUESTED_VERSION" ]]; then
  VERSION="$REQUESTED_VERSION"
else
  info "Resolving latest release"
  VERSION=$(curl -fsSL "https://api.github.com/repos/hgaiser/moonshine/releases/latest" \
    | grep -oP '"tag_name":\s*"\K[^"]+')
  if [[ -z "$VERSION" ]]; then
    die "could not resolve latest version from GitHub API"
  fi
fi

info "Installing moonshine ${VERSION}"

URL="https://github.com/hgaiser/moonshine/releases/download/${VERSION}/moonshine-${VERSION}-x86_64.raw"
step "Downloading ${DIM}${URL}${RESET}"
TMPFILE="$(mktemp)"
curl -fsSL --retry 3 -o "$TMPFILE" "$URL"

SIZE=$(du -h "$TMPFILE" | cut -f1)
step "Downloaded (${SIZE})"

# --- prompts ---

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

# --- build privileged commands ---

PRIV_CMDS=(
  "mkdir -p /var/lib/extensions"
  "mv '${TMPFILE}' /var/lib/extensions/moonshine.raw"
  "systemd-sysext merge moonshine.raw"
  "systemd-sysusers"
  "udevadm control --reload && udevadm trigger"
  "modprobe uinput && modprobe uhid"
)

if $LINGER_NEEDED && [[ "$LINGER" == "true" ]]; then
  PRIV_CMDS+=("loginctl enable-linger '${USER}'")
fi

if [[ "$ENABLE_ON_BOOT" == "true" ]] || [[ "$START_NOW" == "true" ]]; then
  PRIV_CMDS+=("systemctl daemon-reload")
  if [[ "$ENABLE_ON_BOOT" == "true" ]] && [[ "$START_NOW" == "true" ]]; then
    PRIV_CMDS+=("systemctl enable --now 'moonshine@${USER}'")
  elif [[ "$ENABLE_ON_BOOT" == "true" ]]; then
    PRIV_CMDS+=("systemctl enable 'moonshine@${USER}'")
  else
    PRIV_CMDS+=("systemctl start 'moonshine@${USER}'")
  fi
fi

if [[ "$HEALTHCHECK" == "true" ]]; then
  PRIV_CMDS+=("systemd-run --quiet --wait --pipe -p 'User=${USER}' -p 'SupplementaryGroups=input' -p 'SupplementaryGroups=moonshine' -p 'DeviceAllow=/dev/uinput rw' -p 'DeviceAllow=/dev/uhid rw' -p 'DeviceAllow=char-drm rw' -p 'DeviceAllow=char-nvidia rw' -p 'DeviceAllow=char-nvidia-uvm rw' /usr/bin/moonshine healthcheck")
fi

# --- display and run ---

echo ""
echo -e "  ${DIM}The following commands will be run with sudo:${RESET}"
echo ""
for cmd in "${PRIV_CMDS[@]}"; do
  echo "  $cmd"
done
echo ""

# Rebuild for execution with tolerances (|| true for non-critical steps)
sudo bash -c "
  set -euo pipefail
  mkdir -p /var/lib/extensions
  mv '$TMPFILE' /var/lib/extensions/moonshine.raw
  systemd-sysext merge moonshine.raw 2>/dev/null || true
  systemd-sysusers 2>/dev/null || true
  udevadm control --reload || true
  udevadm trigger || true
  modprobe uinput || true
  modprobe uhid || true
  $($LINGER_NEEDED && [[ \"$LINGER\" == \"true\" ]] && echo "loginctl enable-linger '$USER' || true")
  $([[ \"$ENABLE_ON_BOOT\" == \"true\" ]] || [[ \"$START_NOW\" == \"true\" ]] && echo "systemctl daemon-reload")
  $([[ \"$ENABLE_ON_BOOT\" == \"true\" ]] && [[ \"$START_NOW\" == \"true\" ]] && echo "systemctl enable --now 'moonshine@${USER}'")
  $([[ \"$ENABLE_ON_BOOT\" == \"true\" ]] && [[ \"$START_NOW\" != \"true\" ]] && echo "systemctl enable 'moonshine@${USER}'")
  $([[ \"$ENABLE_ON_BOOT\" != \"true\" ]] && [[ \"$START_NOW\" == \"true\" ]] && echo "systemctl start 'moonshine@${USER}'")
  $([[ \"$HEALTHCHECK\" == \"true\" ]] && echo "systemd-run --quiet --wait --pipe -p 'User=$USER' -p 'SupplementaryGroups=input' -p 'SupplementaryGroups=moonshine' -p 'DeviceAllow=/dev/uinput rw' -p 'DeviceAllow=/dev/uhid rw' -p 'DeviceAllow=char-drm rw' -p 'DeviceAllow=char-nvidia rw' -p 'DeviceAllow=char-nvidia-uvm rw' /usr/bin/moonshine healthcheck || true")
"

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
step "This installer is at /usr/bin/moonshine-install.sh for future upgrades or uninstall"
echo ""

ok "Done"
