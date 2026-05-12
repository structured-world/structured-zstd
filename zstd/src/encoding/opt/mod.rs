//! Optimal-parser DP used by `BtOpt` / `BtUltra` / `BtUltra2`.
//!
//! Will host:
//! - Forward DP loop (`build_optimal_plan_impl`, donor
//!   `ZSTD_compressBlock_opt_generic`)
//! - Reverse traceback + sequence emit
//! - `init_stats_ultra` BtUltra2 first-pass (donor
//!   `ZSTD_initStats_ultra`)
//! - LDM integration (`optLdm_processMatchCandidate` parity hooks);
//!   the actual LDM matcher lands in `super::ldm` during #111 Phase 5
//!   (implements #18).
//!
//! Empty in #111 Phase 1.0 (scaffold).

#![allow(dead_code)]
