# Agent guidance

- Enter the dev shell for all Rust and Python commands: `nix develop -c <cmd>`.
  `flake.nix` enables `sccache` so separate worktrees share compiled crates
  without sharing `target/`. Keep a distinct `target/` directory per worktree
  so A/B binaries never overwrite each other.
- Host encode verification: use [verify-moonshine](.agents/skills/verify-moonshine/SKILL.md)
  (`tools/verification/verify_video.py`).
- Physical LG C3 / Moonlight verification: use [verify-moonshine-tv](.agents/skills/verify-moonshine-tv/SKILL.md)
  (`tools/tv/verify_tv.py`). Coordinate TV GUI actions with `tv-lock` and read
  TV state with `tv-snapshot` from `repparw/homelab-devices` (`tv/bin/`).
- Settled Alpha (RX 6700 XT) + LG C3 measurements and ruled-out hypotheses live
  in [docs/alpha-c3-baselines.md](docs/alpha-c3-baselines.md). Check it before
  repeating an experiment.
- Review standards (judgement calls): [CODING_STANDARDS.md](CODING_STANDARDS.md).
- Before pushing: run `nix develop -c cargo fmt --all -- --check`,
  `nix develop -c cargo clippy --workspace --all-features -- -D warnings`,
  `nix develop -c cargo test --workspace --all-features`, and both Python helper
  test suites (`tools/verification` and `tools/tv`).
