---
name: verify-moonshine-tv
description: "Run fork-local, opt-in real Moonlight/webOS TV integration checks for Moonshine, including guarded app launch, host/client evidence collection and cleanup. Use only for TV client and display validation."
---

# Verify Moonshine to a physical LG/webOS TV

Fork-only (`repparw/moonshine`) procedure for authorized physical LG C3 tests. Read [tools/tv/README.md](../../../tools/tv/README.md) for command semantics and [docs/alpha-c3-baselines.md](../../../docs/alpha-c3-baselines.md) for settled TV baselines.

## Steps

1. **Probe read-only state first** using `homelab-devices` `tv/bin/tv-snapshot` (or `verify_tv.py doctor` in `nix develop`):
   ```sh
   /home/repparw/Projects/homelab-devices/tv/bin/tv-snapshot
   nix develop -c python3 tools/tv/verify_tv.py plan --device <alias> --app-id <app-id> --host-uuid <uuid> --host-app-id <id>
   ```
   Confirm the TV is on Home with no active video, Moonlight is idle, and the shared TV GUI lock (`homelab-tv/gui.lock`) is free.

2. **Run bounded, state-preserving verification** (automatically acquires the shared TV GUI lock, records power/mute/volume via `tv_state.py`, mutes an idle TV, and restores original mute state on exit):
   ```sh
   nix develop -c python3 tools/tv/verify_tv.py run --device <alias> --ssh-alias tv \
     --app-id <app-id> --host-uuid <uuid> --host-app-id <id> \
     --host-pid <pid> --journal-unit moonshine.service \
     --duration 60 --interval 15 --settings-confirmed --confirm --out target/tv-verification/<run-id>
   ```
   For game tests, confirm active gameplay past title/main menus and collect matched host logs plus Moonlight client statistics (FPS, drops, decode/network latency, HDR state).

3. **Recover cleanly if interrupted**: if a prior run left a pending journal under `$XDG_STATE_HOME/moonshine-tv/pending-state.json`, inspect ownership, stop only the owned session through Moonlight's UI, and execute the exact `restore-state` command printed in `next_action`.

4. **Report with an explicit operator decision**: state host SHA, client version, workload, matched host/client evidence, and what the operator should do next (`merge`, `recover`, or `none`). Retain evidence under `target/tv-verification/`.

5. **Run offline safety tests** after editing `tools/tv/`:
   ```sh
   nix develop -c python3 -m unittest discover -s tools/tv -p 'test_*.py'
   ```
