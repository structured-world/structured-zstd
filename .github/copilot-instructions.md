# GitHub Copilot Instructions for structured-zstd

## Project Overview

Pure Rust zstd implementation. Focus: dictionary compression improvements and performance parity with C zstd for CoordiNode LSM-tree.

## Review Scope Rules

**Review ONLY code within the PR's diff.** For issues found outside the diff, suggest creating a separate issue.

## Rust Code Standards

- **Clippy:** Must pass `cargo clippy -p structured-zstd --features hash,std,dict_builder -- -D warnings` (`rustc-dep-of-std` is excluded — it's an internal feature for Rust stdlib builds only; `fuzz_exports` is excluded — fuzzing-specific entry points are validated separately from the regular lint gate)
- Performance-critical code: benchmark before/after any changes
- **Do not flag `use std::vec` (or `use alloc::vec`) as an unused import.** This crate is `no_std` with an `std` feature; the std prelude is not implicitly in scope, and the `vec![..]` macro resolves through the same path as the module. The import is required wherever the macro is used. Removing it fails the build with `cannot find macro 'vec' in this scope`.

## no-std-First Design Rules

These rules govern every Rust library crate in this repository. Apply them in code review even when the crate's CI gate is currently lenient — the migration target is `no_std + alloc` clean.

1. Every Rust library crate MUST in principle support `no_std + alloc` builds, even if not currently enforced in CI.
2. Choose primitives in this order: `core::*` → `alloc::*` → external `no_std + alloc` crate → `std::*` behind `#[cfg(feature = "std")]` → unconditional `std::*` (last resort).
3. `default = ["std"]`, `std = []`, `alloc = []` features MUST exist in the `Cargo.toml` of any crate targeting `no_std`. `src/lib.rs` MUST open with `#![cfg_attr(not(feature = "std"), no_std)]` and `extern crate alloc;`.
4. CI MUST include a `no-std-check` job using a no-std-only target (e.g. `thumbv7em-none-eabihf`) with `--no-default-features --features alloc`. Host targets with available `std` MUST NOT be used for this check (they silently pull `std` in through transitive features).
5. Public API surface (traits, function signatures, returned types) MUST use only `core` / `alloc` types in any crate not tiered std-only. `std::*` in implementation modules is allowed only when the public surface remains alloc-friendly.
6. `std::collections::HashMap` / `HashSet` are FORBIDDEN in alloc-tier or lower. Use `hashbrown::HashMap` / `HashSet`, or `rustc_hash::FxHashMap` for internal-ID keys.
7. `std::sync::Mutex` / `RwLock` in new code requires explicit justification. Prefer `parking_lot::Mutex` / `RwLock` on hot paths. Use `spin::Mutex` only in genuinely no-std contexts and only for very short critical sections.
8. `std::sync::OnceLock` is FORBIDDEN for fallible init. Use `once_cell::sync::OnceCell::get_or_try_init` or `once_cell::race::OnceBox`.
9. `thread_local!` is FORBIDDEN below std-only tier. Replace with caller-managed scratch parameters or atomic-pointer patterns.
10. `std::io::Error` MUST NOT appear in public APIs of crates not tiered std-only. Define a crate-local error enum; `From<std::io::Error>` impls live behind `#[cfg(feature = "std")]`.
11. `std::time::Instant` and `std::time::SystemTime` MUST NOT appear in public APIs of crates not tiered std-only. Use a caller-provided clock trait or a `#[cfg(feature = "std")]`-gated convenience wrapper.
12. `std::thread::*` is FORBIDDEN below std-only tier. Threading must be hoisted to a higher-tier crate.
13. `core::*` MUST be used in source whenever the type is available there. `std::*` re-exports of `core` types (e.g. `std::sync::atomic::AtomicU64`) are forbidden in code that compiles under `no_std` — they break the build for no-std targets even when binary-identical for std targets.
14. The choice between `std::*` and a `no_std`-ready alternative is made per primitive, NOT per crate. A crate tiered std-only MUST still prefer the faster `no_std`-ready primitive when one exists (e.g. `hashbrown`, `parking_lot`, `smallvec`, `rustc_hash`, `bytes`) — these are normally faster than their `std::*` counterparts on hot paths.
15. Adding a new `use std::*` to a non-std-only crate is a one-way ratchet. Reviewers MUST reject such additions unless the PR explicitly re-tiers the crate AND updates the per-crate tier table.
16. Migration progress from `std` to `no_std` MUST be monotonically non-decreasing per PR. CI compile-error count for the `no-std-check` job MAY be used as a tracked metric while a crate is in transition (`continue-on-error: true`), but it MUST decrease, never increase.
17. Test code (`#[cfg(test)]`), benches (`benches/`), and binaries (`src/bin/`) are NOT subject to the `no_std` tier — they MAY use `std::*` freely. Only library code in `src/lib.rs` and its submodules is governed.
18. Tier reclassification (alloc → std-bound, leaf-isolated, etc.) requires a documented justification in the PR description AND an update to the per-crate tier table.
19. Doc comments and rustdoc `# Examples` blocks on `no_std`-capable APIs MUST NOT depend on `std::*` types if the API itself does not. Doctest examples requiring `std` MUST be gated `#[cfg(feature = "std")]`.
20. Adding a transitive dependency that pulls `std` into an otherwise no-std-clean crate counts as a violation of rule 15 and requires the same approval path.
