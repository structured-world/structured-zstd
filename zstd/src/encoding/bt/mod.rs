//! Binary-tree match finder used by `BtOpt` / `BtUltra` / `BtUltra2`.
//!
//! Will host:
//! - `bt_insert_step_no_rebase` and the `bt_update_tree_until` driver
//!   (donor `ZSTD_insertBt1` / `ZSTD_updateTree_internal`)
//! - `bt_insert_and_collect_matches` (donor
//!   `ZSTD_insertBtAndGetAllMatches`)
//! - Repcode probe (donor `ZSTD_insertBtAndGetAllMatches` rep loop)
//! - `matchEndIdx - 8` skip (donor `zstd_opt.c:816`) — pending fix
//!   noted in [[refactor-intrinsics-encoder-plan]].
//!
//! Empty in #111 Phase 1.0 (scaffold).

#![allow(dead_code)]
