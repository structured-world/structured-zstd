# GitHub Copilot Instructions for structured-zstd

## Project Overview

Pure Rust zstd implementation — managed fork of [ruzstd (KillingSpark/zstd-rs)](https://github.com/KillingSpark/zstd-rs). Focus: dictionary compression improvements and performance parity with C zstd for CoordiNode LSM-tree.

## Review Scope Rules

**Review ONLY code within the PR's diff.** For issues found outside the diff, suggest creating a separate issue.

## Rust Code Standards

- **Clippy:** Must pass `cargo clippy -p structured-zstd --features hash,std,dict_builder -- -D warnings` (`rustc-dep-of-std` is excluded — it's an internal feature for Rust stdlib builds only; `fuzz_exports` is excluded — fuzzing-specific entry points are validated separately from the regular lint gate)
- This is a fork — avoid suggesting architectural changes that diverge too far from upstream
- Performance-critical code: benchmark before/after any changes
