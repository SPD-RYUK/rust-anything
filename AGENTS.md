# DOX for rust-anything

## Purpose

- Maintain the Rust CLI and its language bridges for recording original behavior and comparing it with Rust ports.

## Ownership

- This file owns repository-wide guidance and replay contracts. See `CONTRIBUTING.md` for project verification and `docs/STATUS.md` for runtime coverage.

## Local Contracts

- PHP replay uses the generated wrapper and autoloads referenced project types from non-test source files. Exclude originals listed in the manifest exports or kept entries to avoid redeclaring functions.
- Ruby diff treats missing wrapper targets as provenance mismatches, even when the recorded case expects the same error type.

## Verification

- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- Set `RUST_ANYTHING_REQUIRE` for the runtimes under test so missing runtimes fail the check.

## Child DOX Index

- This repository currently has no child DOX files.
