use super::*;

#[test]
fn decoder_entry_layout_12_bytes_seqsymbol_shape() {
    // Upstream zstd `ZSTD_seqSymbol` shape: 4-byte header (new_state /
    // symbol / num_bits), 4-byte `base_value`, 1-byte
    // `num_additional_bits` + 3-byte tail padding for natural u32
    // alignment. Total 12 bytes. Grown from the classical 4-byte
    // layout to remove the per-sequence `lookup_ll_code` /
    // `lookup_ml_code` indirection: LL and ML enrich `base_value`
    // / `num_additional_bits` from the packed code-meta tables
    // (`enrich_with_packed_seq_meta`), OF enriches closed-form
    // `base_value = 1 << code`, `num_additional_bits = code`
    // (`enrich_for_offsets`, no meta table). HUF-weight FSE tables
    // never enrich and leave both fields zero.
    assert_eq!(core::mem::size_of::<fse_decoder::Entry>(), 12);
    assert_eq!(core::mem::offset_of!(fse_decoder::Entry, new_state), 0);
    assert_eq!(core::mem::offset_of!(fse_decoder::Entry, symbol), 2);
    assert_eq!(core::mem::offset_of!(fse_decoder::Entry, num_bits), 3);
    assert_eq!(core::mem::offset_of!(fse_decoder::Entry, base_value), 4);
    assert_eq!(
        core::mem::offset_of!(fse_decoder::Entry, num_additional_bits),
        8
    );
}

#[test]
fn build_from_probabilities_rejects_acc_log_over_entry_limit() {
    let mut dec_table = FSETable::new(255);
    let err = dec_table
        .build_from_probabilities(17, &[1, 1, 1, 1])
        .unwrap_err();
    assert!(matches!(
        err,
        crate::decoding::errors::FSETableError::AccLogTooBig { got: 17, max: 16 }
    ));
}

#[test]
fn build_decoder_empty_input_reports_bits_error_with_large_max_log() {
    let mut dec_table = FSETable::new(255);
    let err = dec_table.build_decoder(&[], 17).unwrap_err();
    assert!(matches!(
        err,
        crate::decoding::errors::FSETableError::GetBitsError(_)
    ));
}

#[test]
fn tables_equal() {
    let probs = &[0, 0, -1, 3, 2, 2, (1 << 6) - 8];
    let mut dec_table = FSETable::new(255);
    dec_table.build_from_probabilities(6, probs).unwrap();
    let enc_table = fse_encoder::build_table_from_probabilities(probs, 6);

    check_tables(&dec_table, &enc_table);
}

/// Verify `table_header_bits()` matches the actual byte count written by `write_table()`.
#[test]
#[allow(clippy::borrow_deref_ref)]
fn table_header_bits_exact() {
    use crate::bit_io::BitWriter;
    use fse_encoder::{
        build_table_from_data, build_table_from_probabilities, default_ll_table, default_ml_table,
        default_of_table,
    };

    let check = |table: &fse_encoder::FSETable| {
        let mut buf = alloc::vec::Vec::new();
        let mut writer = BitWriter::from(&mut buf);
        table.write_table(&mut writer);
        writer.flush();
        let written_bits = buf.len() * 8; // flush pads to byte boundary
        let computed_bits = table.table_header_bits();
        assert_eq!(
            computed_bits, written_bits,
            "table_header_bits() mismatch: computed={computed_bits}, written={written_bits}"
        );
    };

    // Predefined tables. Borrow via `&*` so the call compiles on
    // both `FseDefaultTable` shapes — `&'static FSETable` (atomic /
    // `critical-section`) and `Box<FSETable>` (no-atomic-no-CS).
    check(&*default_ll_table());
    check(&*default_ml_table());
    check(&*default_of_table());

    // Tables built from synthetic data
    let data: alloc::vec::Vec<u8> = (0u8..32).cycle().take(1000).collect();
    check(&build_table_from_data(data.iter().copied(), 9, true));

    let data2: alloc::vec::Vec<u8> = alloc::vec![0, 1, 2, 3]
        .into_iter()
        .cycle()
        .take(500)
        .collect();
    check(&build_table_from_data(data2.iter().copied(), 8, true));

    // Uniform distribution: 32 symbols × prob=2 = 64 = 1<<6 (acc_log=6 requires sum=64)
    check(&build_table_from_probabilities(
        &[
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            2, 2, 2,
        ],
        6,
    ));
}

#[test]
fn roundtrip() {
    round_trip(&(0..64).collect::<alloc::vec::Vec<_>>());
    let mut data = alloc::vec![];
    data.extend(0..32);
    data.extend(0..32);
    data.extend(0..32);
    data.extend(0..32);
    data.extend(0..32);
    data.extend(20..32);
    data.extend(20..32);
    data.extend(0..32);
    data.extend(20..32);
    data.extend(100..255);
    data.extend(20..32);
    data.extend(20..32);
    round_trip(&data);

    #[cfg(feature = "std")]
    if std::fs::exists("fuzz/artifacts/fse").unwrap_or(false) {
        for file in std::fs::read_dir("fuzz/artifacts/fse").unwrap() {
            if file.as_ref().unwrap().file_type().unwrap().is_file() {
                let data = std::fs::read(file.unwrap().path()).unwrap();
                round_trip(&data);
            }
        }
    }
}

#[test]
#[should_panic(expected = "FSE table requires at least 2 samples")]
fn fse_header_pricing_rejects_single_sample_histogram() {
    // A histogram with fewer than two samples cannot form a valid FSE
    // table. Header pricing must trip the same `total > 1` guard as the
    // builder it mirrors, rather than fall through into table-log /
    // normalization arithmetic that underflows on `total <= 1`.
    let _ = fse_encoder::fse_header_bits_for_counts(&[1], 9, false);
}

#[test]
fn fse_header_pricing_matches_built_table_header_bits() {
    // The custom-table mode selector trusts that pricing a header from
    // counts returns exactly what the eventually built table's
    // `table_header_bits()` reports. Lock that invariant across a few
    // representative histograms and both `avoid_0_numbit` modes.
    let histograms: [&[usize]; 4] = [
        &[50, 50],
        &[4, 4, 4, 4],
        &[100, 5, 1, 1],
        &[200, 100, 50, 25, 12, 6, 3, 1],
    ];
    for counts in histograms {
        for avoid_0_numbit in [false, true] {
            let priced = fse_encoder::fse_header_bits_for_counts(counts, 9, avoid_0_numbit);
            let built = fse_encoder::build_table_from_symbol_counts(counts, 9, avoid_0_numbit)
                .table_header_bits();
            assert_eq!(
                priced, built,
                "header pricing diverged from built table for counts={counts:?} \
                 avoid_0_numbit={avoid_0_numbit}"
            );
        }
    }
}
