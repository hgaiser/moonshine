# Alpha + LG C3 Settled Baselines

Consolidated hardware measurements from Oct–Nov 2026 so new investigations start
from proven limits instead of repeating settled experiments.

## Environment

- **Host (Alpha):** NixOS, AMD Radeon RX 6700 XT (Navi22 / RDNA2), Mesa RADV
  26.2.6, wired `eth0` (1 Gbps `r8169`).
- **Client (LG C3):** LG OLED55C3PSA, webOS TV 5.0 (rooted), MediaTek MT7668
  USB Wi-Fi 5 (`wlan_mt7668_usb`, 5 GHz ch 149 / 80 MHz), TP-Link Archer AX23
  v2.0 AP (Terraform state in `repparw/nix:infra/router/`).
- **Network confounder:** Alpha's `wan-ingress-shaping` (`repparw/nix:modules/aspects/networking.nix`)
  applies a 23 Mbps CAKE ingress limit to all `eth0` ingress, including LAN
  peers (TV → Alpha). Alpha → TV streaming egress is unaffected, but TV → Alpha
  throughput/TCP tests are confounded unless accounted for.

## Proven Host Ceilings (RX 6700 XT VCN)

| Mode (headless `vkcube`, HEVC) | Sustained FPS | Notes |
| --- | --- | --- |
| 1080p120 | 120 FPS | GPU ~2.47 ms |
| 1440p120 | 120 FPS | GPU ~4.17 ms |
| 4K90 | 90 FPS | GPU ~8.85 ms, submit ~79 µs; encoder holds |
| 4K120 (VCN exclusive) | ~112.55 FPS | Hardware VCN ceiling; H.264 reaches ~104 FPS with worse quality at 20 Mbps |
| 4K120 (browser video decode active) | ~99 FPS | Shared VCN decode contention; always verify VCN idle first |

- Kernel GPU scheduling trace proved Moonshine is **not** starving VCN: every
  next frame was queued before the previous job completed (~0.15% inter-job
  gaps).

## Ruled-Out Host Leads (Do Not Re-Test Without a New Mechanism)

- **Encode queue depth > 2:** Adds latency without raising throughput, even with
  preloaded GPU frames.
- **RADV `ULTRA_LOW_LATENCY`, SAO disable, or encoder preset tweaks:** No
  meaningful throughput improvement at 4K120.
- **GPU power profile `high` vs `auto`:** +1.4% isolated throughput for +8 W GPU
  power; VCN clocks unchanged. Keep `auto`.
- **Ferriscope WSI layer / `VK_VALVE_video_encode_rgb_conversion`:** Already
  enabled where applicable; shader RGB conversion is needed for HDR
  tone-mapping; 1440p120 A/B matched at ~4.98 ms.
- **Replacing Moonshine with Sunshine (VA-API):** Offline FFmpeg VA-API 4K120
  (`hevc_vaapi` 72 FPS, `h264_vaapi` 57 FPS) was slower than Moonshine Vulkan
  Video. Keep Moonshine.
- **Client IDR-request rate limiter:** No consistent recovery benefit under
  packet loss; shelved.

## Settled Network & Client Facts

- **LG C3 Wi-Fi throughput:** Asymmetric — Alpha → TV ~262–268 Mbps sustained
  (recommended streaming ceiling 150 Mbps over Wi-Fi; 200 Mbps wired only),
  TV → Alpha ~21 Mbps.
- **Recurring Wi-Fi latency collapses (~700–900 ms RTT):** Cleared immediately
  by toggling TV Wi-Fi (TX PHY jumped e.g. 433 → 585 Mbit/s on the same
  BSSID/channel). Root cause remains open; capture 1 Hz `iw link` + ping before
  and after the next toggle.
- **Upstream & fork PR dispositions:**
  - `hgaiser/moonshine#250` (GSO `WouldBlock` fix): validated; eliminated
    millions of `Udp:SndbufErrors`.
  - `hgaiser/moonshine#256` (RTSP bitrate/fps validation) and `#257`: merged.
  - `hgaiser/moonshine#259` (adaptive congestion/FEC): validated on LG C3
    (Sekiro 1080p60 H.264 20 Mbps; at 5% loss: ON 48–58 FPS vs OFF 32–51 FPS).
  - `moonlight-stream/moonlight-common-c#172` (depacketizer/event race): merged.
  - `repparw/moonshine#15` (capture/release wait debug timing): local diagnostic
    only, not a performance fix.

## Open Leads

1. **Post-encode filler-NALU filter (Moonshine):** Removes CBR padding bytes
   without altering the compressed picture (unlike disabling RADV firmware
   filler). Needs real-game SDR/HDR LG C3 validation before enabling.
2. **LG C3 120 Hz presentation (`repparw/moonlight-tv` / `ss4s` NDL):** Determine
   where 120 FPS decoded/submitted frames drop to 60 Hz presentation on webOS.
3. **LG C3 Wi-Fi collapse attribution:** Capture pre-toggle `iw link` + ping
   time series via `homelab-devices` `tv/bin/tv-snapshot`.
