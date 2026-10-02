# Historical architecture and validation reports

These snapshots preserve reasoning, measurements and unperformed checks from
September 30–October 2, 2026 development. They are not current architecture
contracts or proof that a later checkout passed validation. Test counts, versions
and local workspace paths describe the original runs.

- [Transport remediation (2026-10-02)](TRANSPORT_REMEDIATION_2026-10-02.md):
  packetizer allocation/copy evidence, completion admission, truthful UDP outcomes and hardware acceptance limits.
- [Production-readiness review (2026-10-01)](PRODUCTION_READINESS_REVIEW_2026-10-01.md):
  revision-pinned findings, reproduced defects, implementation batches and release acceptance gates.
- [Pacing cadence correction](PACING_CADENCE.md): composition timing and precise
  packet deadlines restore the measured saturated 4K120 case with one credit.
- [Capture optimization](PIPELINE_OPTIMIZATION.md): admission changes, measured
  tradeoffs, rejected conversion/DWT fusion and deferred synchronization work.
- [Long-session investigation](LONG_SESSION_PERFORMANCE.md): UDP readiness,
  import cleanup, scanout release defects and hardware smoke tests.
- [Vulkan WSI validation](VULKAN_IMAGE_COUNTS.md): dispatch regression and
  non-presenting image-count probes, with outstanding game acceptance.
- [DualSense Edge validation](DUALSENSE_EDGE.md): client/protocol/native builds
  and checks, with physical controller/Steam acceptance still unperformed.

Use the [architecture overview](../ARCHITECTURE.md) and
[documentation index](../README.md) for current design and validation procedures.
When adding a report, record the revision, hardware/driver, workload, measurement
boundaries and checks not performed. Avoid presenting short or loopback tests as
end-to-end validation.
