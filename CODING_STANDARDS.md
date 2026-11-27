# Coding Standards (Review Checklist)

Deterministic checks (formatting, Clippy, Rust tests, Python unit tests, VCN
contention, binary provenance, hardware/TV locks) belong in CI and the
verification scripts. Apply these judgement rules during review:

- **Representative workloads:** Real-game benchmarks must prove active gameplay
  (loaded save or moving scene), not a title or main menu. Require matched host
  and client observation windows and name the measured limiter before
  recommending a performance change.
- **Explicit operator handoff:** Reports and PR summaries must end with the
  exact next action for the operator (`merge`, `recover`, or `none`) and state
  clearly whether live services or NixOS generations were switched or only
  committed.
- **Peer symmetry on refactors:** When deduplicating checks, scripts, or
  assertions across targets or variants (baseline vs candidate, SDR vs HDR,
  desktop vs TV), verify parity across every peer before approving.
- **Clean artifacts:** Reject stray `.bak` files or ad hoc temporary scripts in
  git status, and keep fork-only TV helpers separate from upstream-portable
  changes.
