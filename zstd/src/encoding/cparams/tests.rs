use super::*;

#[test]
fn create_cdict_table_logs_downsizes_for_unknown_source_with_dict() {
    // create_cdict + a dictionary + unknown source size assumes a minimal
    // (513-byte) source, so the prepared CDict tables down-size far below
    // the requested input logs instead of holding max-size tables
    // (`ZSTD_adjustCParams_internal` with `ZSTD_cpm_createCDict`).
    let (hash_log, chain_log) = create_cdict_table_logs(
        27, 27, 28, /* uses_bt */ true, /* dict_size */ 4096,
    );
    assert!(
        hash_log < 27,
        "hash_log {hash_log} must shrink for a tiny CDict source",
    );
    assert!(chain_log <= 28, "chain_log {chain_log} must not grow");

    // A zero-dict CDict does NOT force the minSrcSize assumption (the
    // `dict_size != 0` guard), so the unknown-source path leaves the logs as
    // requested.
    let (h2, c2) =
        create_cdict_table_logs(20, 20, 21, /* uses_bt */ false, /* dict_size */ 0);
    assert_eq!(h2, 20, "no-dict CDict keeps the requested hash_log");
    assert_eq!(c2, 21, "no-dict CDict keeps the requested chain_log");
}

/// The row finder takes its row bits and an 8-bit tag from one 32-bit hash,
/// so a row strategy's hash table is capped at `32 - 8 + rowLog`; a strategy
/// without rows keeps the table it asked for.
#[test]
fn a_row_strategy_hash_table_fits_one_hash() {
    let wide = |strategy| CParams {
        window_log: 30,
        chain_log: 29,
        hash_log: 30,
        search_log: 4,
        min_match: 4,
        target_length: 0,
        strategy,
    };
    for strategy in 3..=5 {
        let cp = adjust_cparams(wide(strategy), CONTENTSIZE_UNKNOWN, 0, false);
        assert_eq!(cp.hash_log, 32 - 8 + 4, "strategy {strategy}");
    }
    let cp = adjust_cparams(wide(6), CONTENTSIZE_UNKNOWN, 0, false);
    assert_eq!(cp.hash_log, 30);
}

/// The CDict table helper sizes tables that have no rows, so the row cap does
/// not narrow them: a dictionary large enough to fill a 2^30 table keeps it.
#[test]
fn cdict_table_logs_are_not_held_to_the_row_cap() {
    let (hash_log, _) = create_cdict_table_logs(
        30,
        30,
        30,
        /* uses_bt */ false,
        /* dict_size */ 1 << 29,
    );
    assert_eq!(hash_log, 30);
}

/// A CDict prepared under explicit parameters takes each knob in place of the
/// level row's and is down-sized for the dictionary again
/// (`ZSTD_getCParamsFromCCtxParams`): a 4 KiB dictionary at level 3 asked for
/// the btopt strategy, search/min-match 6 and a 2^20 hash table runs btopt at
/// 6 / 6 with the hash table the dictionary can fill (2^14). Knobs left unset
/// keep the row's value, and no knob at all is the plain CDict.
#[test]
fn cdict_cparams_take_the_knobs_the_caller_set() {
    let plain = get_cdict_cparams(3, 4096, &Default::default());
    let overrides = crate::encoding::parameters::ParamOverrides {
        strategy: Some(crate::encoding::Strategy::Btopt),
        search_log: Some(6),
        min_match: Some(6),
        hash_log: Some(20),
        ..Default::default()
    };
    let tuned = get_cdict_cparams(3, 4096, &overrides);
    assert_eq!(tuned.strategy, 7);
    assert_eq!((tuned.search_log, tuned.min_match), (6, 6));
    assert_eq!(tuned.hash_log, 14);
    assert_eq!(tuned.target_length, plain.target_length);
    assert_eq!(plain, get_cparams_mode(3, CONTENTSIZE_UNKNOWN, 4096, true));
}
