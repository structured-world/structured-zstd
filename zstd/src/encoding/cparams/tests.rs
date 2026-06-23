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
