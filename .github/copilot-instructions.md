# GitHub Copilot Instructions for structured-zstd

## Project Overview

Pure Rust zstd implementation — managed fork of [ruzstd](https://github.com/KillingSpark/zstd-rs). Focus: dictionary compression improvements and performance parity with C zstd for CoordiNode LSM-tree.

## Review Scope Rules

**Review ONLY code within the PR's diff.** For issues found outside the diff, suggest creating a separate issue.

## Rust Code Standards

- **Clippy:** Must pass `cargo clippy --all-features -- -D warnings`
- This is a fork — avoid suggesting architectural changes that diverge too far from upstream
- Performance-critical code: benchmark before/after any changes
