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

**`structured-zstd` MUST compile and work under `no_std + alloc`.** These rules govern all library code (`src/lib.rs` and its submodules). They are a flat contract — there are no opt-out crate tiers and no per-crate carve-outs; every primitive choice is judged by the rules below.

1. The crate MUST support `no_std + alloc` builds. This is both a CI gate and a design constraint applied during review.
2. Choose primitives in this order: `core::*` → `alloc::*` → external `no_std + alloc` crate → `std::*` behind `#[cfg(feature = "std")]` → unconditional `std::*` (last resort, requires explicit justification in the PR description).
3. `default = ["std"]`, `std = []`, `alloc = []` features MUST exist in `Cargo.toml`. `src/lib.rs` MUST open with `#![cfg_attr(not(feature = "std"), no_std)]` and `extern crate alloc;`.
4. CI MUST include a `no-std-check` job using a no-std-only target (e.g. `thumbv7em-none-eabihf`) with `--no-default-features --features alloc`. Host targets with available `std` MUST NOT be used for this check (they silently pull `std` in through transitive features).
5. Public API surface (traits, function signatures, returned types) MUST use only `core` / `alloc` types. `std::*` in implementation modules is allowed only when gated behind `#[cfg(feature = "std")]` AND only when the public surface remains alloc-friendly.
6. `std::collections::HashMap` / `HashSet` are FORBIDDEN. Use `hashbrown::HashMap` / `HashSet`, or `rustc_hash::FxHashMap` for internal-ID keys.
7. `std::sync::Mutex` / `RwLock` in new code requires explicit justification. Prefer `parking_lot::Mutex` / `RwLock` on hot paths in std-gated code. Use `spin::Mutex` for genuinely no-std code paths and only for very short critical sections.
8. `std::sync::OnceLock` is FORBIDDEN for fallible init. Use `once_cell::sync::OnceCell::get_or_try_init` or `once_cell::race::OnceBox`.
9. `thread_local!` is FORBIDDEN in library code. Replace with caller-managed scratch parameters or atomic-pointer patterns.
10. `std::io::Error` MUST NOT appear in public APIs. Define a crate-local error enum; `From<std::io::Error>` impls live behind `#[cfg(feature = "std")]`.
11. `std::time::Instant` and `std::time::SystemTime` MUST NOT appear in public APIs. Use a caller-provided clock trait or a `#[cfg(feature = "std")]`-gated convenience wrapper.
12. `std::thread::*` is FORBIDDEN in library code. Threading must be the caller's responsibility.
13. `core::*` MUST be used in source whenever the type is available there. `std::*` re-exports of `core` types (e.g. `std::sync::atomic::AtomicU64`) are forbidden — they break the build for no-std targets even when binary-identical for std targets.
14. Performance does NOT excuse picking `std::*` over a faster `no_std`-ready primitive. Community alternatives (`hashbrown`, `parking_lot`, `smallvec`, `rustc_hash`, `bytes`) are normally faster than their `std::*` counterparts on hot paths — pick them on merit, not as a fallback.
15. Adding a new unconditional `use std::*` to library code is a one-way ratchet. Reviewers MUST reject such additions unless the PR explicitly justifies why the std primitive cannot be replaced by a no-std-ready alternative or hidden behind `#[cfg(feature = "std")]`.
16. The `no-std-check` CI job MUST stay green (or, if in transition with `continue-on-error: true`, its compile-error count MUST be monotonically non-decreasing per PR — every PR either keeps the count flat or reduces it; raising it is a regression and blocks merge).
17. Test code (`#[cfg(test)]`), benches (`benches/`), and binaries (`src/bin/`) MAY use `std::*` freely. Only library code in `src/lib.rs` and its submodules is governed by these rules.
18. Doc comments and rustdoc `# Examples` blocks on `no_std`-capable APIs MUST NOT depend on `std::*` types if the API itself does not. Doctest examples requiring `std` MUST be gated `#[cfg(feature = "std")]`.
19. Adding a transitive dependency that pulls `std` into the otherwise no-std-clean build counts as a violation of rule 15 and requires the same approval path.
