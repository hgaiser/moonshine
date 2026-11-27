# LG/webOS Moonlight verification (fork-specific)

**Status:** the guarded launcher has been exercised on the physical LG C3: real streaming sessions were started through `verify_tv.py start` and confirmed with independent host, client and TV evidence (desktop and game workloads, November 2026). The bounded `run` lifecycle and `restore-state` recovery are offline-tested and use the same state guard exercised during those runs. These helpers are specific to our fork and are not intended for the generic upstream Moonshine verification PR.

## Why launch parameters may work

Moonlight's webOS client reads `host_uuid` and `host_app_id` from the JSON payload passed when its native application launches. Its launcher locates that previously paired host and automatically starts the requested app if it appears in the host's app list.

Source:
- [webOS parameter parsing](https://github.com/mariotaku/moonlight-tv/blob/main/src/app/platform/webos/app_webos.c)
- [host selection](https://github.com/mariotaku/moonlight-tv/blob/main/src/app/ui/launcher/launcher.controller.c)
- [app auto-launch](https://github.com/mariotaku/moonlight-tv/blob/main/src/app/ui/launcher/apps.controller.c)
- [LG webOS CLI `ares-launch --params`](https://webostv.developer.lge.com/develop/tools/webos-tv-cli-dev-guide)

This relies on the *installed* Moonlight client supporting those fields. Confirm its revision before depending on this for unattended tests.

## Prerequisites

Configure webOS TV CLI and a device alias following LG's official documentation. Confirm `ares-launch --device <alias> --running` and `ares-install --device <alias> --list` work. Determine Moonlight's actual **installed app ID** from that device, the **paired host UUID** from its configuration, and an **app ID advertised by Moonshine**; these are not IP addresses or automatically inferred.

Commands use `python3 tools/tv/verify_tv.py` from the repository root.

## Safe procedures

Only construct a launch command (does **not** contact the TV):

```sh
python3 tools/tv/verify_tv.py plan --device living-room --app-id YOUR_MOONLIGHT_APP_ID \
  --host-uuid YOUR_PAIRED_HOST_UUID --host-app-id 1
```

Query running applications (read-only):

```sh
python3 tools/tv/verify_tv.py doctor --device living-room --app-id YOUR_MOONLIGHT_APP_ID
```

To request a real launch, only when the TV is available for testing:

```sh
python3 tools/tv/verify_tv.py start --device living-room --app-id YOUR_MOONLIGHT_APP_ID \
  --host-uuid YOUR_PAIRED_HOST_UUID --host-app-id 1 --confirm --out target/tv-verification/run-001
```

The helper refuses to launch when Moonlight is already listed as running, and records the request and launcher response. It does **not** stop the TV app, change persistent client settings, restart Moonshine, change inputs, or assert that streaming succeeded. Use a new empty evidence directory each run.

Collect the current running-app listing without controlling the TV:

```sh
python3 tools/tv/verify_tv.py collect --device living-room --app-id YOUR_MOONLIGHT_APP_ID --out target/tv-verification/run-001
```

## Evidence required for a real TV test

A successful `ares-launch` exit code proves **only that the CLI accepted the request**. It does not prove that the app initiated or decoded a video stream.

To call a TV session verified, capture **independent** evidence: Moonshine host logs with timestamps, Moonlight's on-screen streaming statistics (resolution/codec/FPS/frame drops/host/network/decode metrics), and HDR on/off state where applicable. Preserve the capture and the exact tested Moonshine commit. Where possible, obtain client logs through a supported facility; do not invent remote access paths or expose device secrets.

Start by reproducing the known 4K60 HDR throughput issue under the deployed host, current upstream, and candidate branches on the **same TV settings**. Alternate A/B tests and separate capture/encode, sender queue, network, decoder, and display metrics. A short smoke is not a long-duration stability result.

## Safety and cleanup

This tool deliberately has no `stop` command: it cannot know whether an app started by a previous test has since become a user's active streaming session. End the session using Moonlight's normal interface after verifying that it still belongs to the test. Never kill arbitrary processes by name. No commands should contact the TV before user-authorized local testing.

Run offline command-safety tests with `python3 -m unittest discover -s tools/tv -p 'test_*.py'`.

## Run a bounded observation

`run` is fork-specific and contacts the local host and the TV. Configure the Moonlight settings before you start. The command records power, mute, and volume before automatically muting an idle TV. It verifies mute before launching. It does not wake the TV, send input, change client settings, or stop a session. The minimal `start` action remains a launch request only; use `run` for state-preserving verification.

```sh
python3 tools/tv/verify_tv.py run --device living-room --ssh-alias living-room-ssh \
  --app-id YOUR_MOONLIGHT_APP_ID --host-uuid YOUR_PAIRED_HOST_UUID --host-app-id 1 \
  --host-pid YOUR_MOONSHINE_PID --journal-unit YOUR_SYSTEM_SERVICE \
  --duration 60 --interval 15 --settings-confirmed --confirm --out target/tv-verification/run-002
```

The SSH alias must permit status queries and `com.webos.audio/setMuted`. The local PID must own the listener on `--host-port` (default 47989). The host's `serverinfo` UUID must match the paired UUID and its state must be free. Procfs, server status, TV running-app status, power, foreground app, mute, volume, video status, and the system journal must be accessible; use `--journal-scope user` for a user service. The TV must report `ACTIVE` with substate `active` or `screen saver`, be on its home screen, and have no active video output. An active screensaver is still an on-state, not standby. Sleeping, screen-off, unknown, or active-session state prevents mutation. These checks cannot fence another remote client, so the operator must still reserve the TV. The shared TV GUI lock (`$XDG_STATE_HOME/homelab-tv/gui.lock`, overridable with `TV_GUI_LOCK_PATH`) coordinates cooperative runs across Moonshine verification, `homelab-devices` `tv/bin/tv-lock`, and other TV GUI drivers on this host.

For a system service the default journal query includes all accessible journals, because logs from a system service running as a regular user may be stored in that user's journal.

The output directory must not exist. The command preserves the plan, doctor result, host preflight responses, TV status, launch result, and timestamped samples with journal snapshots. Sampling starts after launch, repeats at the requested interval, and includes an end sample. Command timeouts add to the observation duration. Interrupts and errors leave partial evidence and a report when possible. Exit 0 means collection and audio restoration finished, not that playback worked. A pending restoration returns nonzero. A successful launch request is not a successful stream or input action. Inspect independent client statistics, display and HDR evidence, and host logs to establish playback and quality. Reports include `next_action` stating how to use the evidence or finish cleanup.

No available status proves that a later session is still owned by this run. The command leaves it alone even after an error or interrupt. Inspect the TV and host, then use Moonlight's normal interface to stop the test only if it still belongs to you. If launch timed out or failed, it may still have reached the TV. Check before retrying.

## Preserve audio and recover after interruption

`run` records the original TV state through `tv_state.py`. Before muting, the guard atomically saves the original state in the run's `tv-state.json` and a private pending journal under `$XDG_STATE_HOME/moonshine-tv/pending-state.json`, or `~/.local/state/moonshine-tv/pending-state.json` when that variable is unset. A pending journal blocks new runs so they cannot mistake the test's mute setting for the operator's original setting.

Cleanup restores the original mute setting and verifies that volume remains unchanged. An already-muted TV stays muted. A TV that was on stays on. The guard never changes volume or overwrites a detected external volume change. It refuses to unmute while Moonlight remains running or the TV has another foreground app or active video. The bounded observation command therefore leaves restoration pending until the owned test is stopped normally.

`SIGINT` and `SIGTERM` trigger best-effort cleanup. `SIGKILL`, a terminal abort that kills all processes, or host failure cannot run Python cleanup. The journal survives these cases. After inspecting ownership and stopping only the owned test through its normal UI, run the recovery command printed in `next_action`, for example:

```sh
python3 tools/tv/verify_tv.py restore-state --device living-room --ssh-alias living-room-ssh \
  --app-id YOUR_MOONLIGHT_APP_ID --out /absolute/path/to/original-run --confirm
```

The command is idempotent. It validates the original run and SSH alias before contacting the TV. Do not delete a pending journal to bypass a restoration error. Inspect the error and preserve any operator changes instead.

Automatic wake and power-off are deliberately disabled. An SSH failure does not establish that the TV was off. A future wake implementation must record a known initial power state before waking and restore it through a verified power-control path. Turning the screen off is not the same as returning the TV to standby. For now, the guard refuses that situation rather than guessing.
