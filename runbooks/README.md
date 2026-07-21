# dig-store-cache runbooks

## Deployment (release to crates.io)

`dig-store-cache` is a library crate published to crates.io on a version tag (per-merge-tag model,
CLAUDE.md §3.6-B).

1. A PR bumps `version` in `Cargo.toml` (SemVer; version-increment CI gate enforced) and merges to
   `main` via the full gate set (fmt, clippy, test, coverage ≥80%, build, commitlint).
2. `.github/workflows/release.yml` regenerates `CHANGELOG.md` (git-cliff), commits it, and pushes the
   `vX.Y.Z` tag using `RELEASE_TOKEN`.
3. The tag triggers `.github/workflows/publish.yml`, which `cargo publish --locked`es to crates.io
   using `CARGO_REGISTRY_TOKEN` and cuts a GitHub Release.

Prerequisites (repo secrets):
- `CARGO_REGISTRY_TOKEN` — crates.io publish token (org-level).
- `RELEASE_TOKEN` — a classic PAT allowed past branch protection (pushes the changelog commit + tag).

Release ordering: `dig-store` must be published to crates.io before `dig-store-cache` (already is,
≥0.5). First release only: `release.yml` does not auto-run on the first squash-merge — kick it with an
empty commit (RELEASE_TOKEN) or `workflow_dispatch`.

Verify: `cargo search dig-store-cache` shows the new version; the GitHub Release exists for the tag.

## Local development

Prerequisites: a stable Rust toolchain (`rustup`), `cargo-llvm-cov` + `cargo-nextest` for coverage.

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test                      # or: cargo nextest run --all
cargo llvm-cov nextest --all --fail-under-lines 80   # coverage gate
cargo doc --no-deps
```

There is no service to run — the crate is a library consumed by dig-node. Point a `Cache` at any
writable directory (`Cache::open(root, CacheConfig::default())`).
