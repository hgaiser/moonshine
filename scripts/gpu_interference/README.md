# GPU interference harness

Measures what streaming costs a game, not just Pyroshine's own GPU counters.

* `gpuload` (`build.sh`) is a headless, deterministic GPU-bound "game": a
  fixed-cost full-screen pass (texture bandwidth plus ALU) at 4K with two
  frames in flight. It reports achieved FPS, frame-interval percentiles, 1%
  low FPS and per-frame GPU time. Calibrate `--iters`/`--taps` so it is
  GPU-bound near the stream rate (`1800:96` gives ~141 FPS on an RX 9070 XT).
* `gpusample.py` samples per-process engine busy time from DRM fdinfo
  (deduplicated by `drm-client-id`) and amdgpu `gpu_metrics` (gfx/memory
  activity, socket power, average clocks).
* `runbench.py` runs one case: optional probe, optional `moonshine-bench`
  stream, sampler started once frames flow, and writes one JSON record.
* `matrix.py` runs the codec/resolution/HDR/chroma/capture-path matrix, each
  case once without the probe ("solo": stream cost at natural clocks) and once
  with it ("probe": game interference and stream behavior under contention).

```sh
scripts/gpu_interference/build.sh
python3 scripts/gpu_interference/runbench.py --name probe-alone --out runs --probe 1800:96
python3 scripts/gpu_interference/matrix.py --label after --bench target/release/moonshine-bench \
  --pyrowave-lib /path/to/libpyrowave-shared.so.0 [--content pan4k120.mp4]
```

Interpretation limits: engine percentages are active time at whatever clock
the GPU chose, so solo runs at low load understate work at game clocks. In
probe runs the streamed application (vkcube) is a separate process that the
probe also starves, so stream FPS there is bounded by the content's completed
frame rate, not only by Pyroshine. Compare probe FPS/1% lows against
`probe-alone` runs taken before and after the matrix. Run nothing else on the
GPU, and avoid compiling during measurements.
