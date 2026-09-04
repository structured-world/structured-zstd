window.BENCHMARK_DATA = {
  "lastUpdate": 1788533791725,
  "repoUrl": "https://github.com/structured-world/structured-zstd",
  "entries": {
    "structured-zstd vs C FFI (x86_64-gnu)": [
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "844e95a3f7c8586eea7f39ff231fb928309d46d2",
          "message": "fix(dfast): upstream-identical sequence generation + faster HUF decode (#450)\n\n* test(bench): report per-frame block structure in compare_ffi\n\nAdd a REPORT_BLK diagnostic line alongside the existing REPORT_HDR: walk\neach frame's block list (type / size / last flag) for both the rust and\nffi encoders. Surfaces block-count and per-block-size divergences (e.g. a\nspurious trailing empty block, or a different pre-split boundary) that the\naggregate byte-size REPORT line hides.\n\n* fix(encode): mark last block on exact block-multiple input\n\nWhen the input length was an exact multiple of MAX_BLOCK_SIZE, the owned\nblock loop emitted the final full block with last_block=false and then\nappended a spurious trailing empty Raw block (R0) carrying the last flag\n— 3 wasted bytes per such frame, and a divergence from the C encoder\nwhich marks the last real block last. It hit EVERY multi-block frame whose\ncontent ends exactly on a block boundary (e.g. the 1 MiB = 8x128 KiB bench\nfixtures), independent of compressibility.\n\nRoot cause: both `OwnedBlockSource` feeders reported EOF only when a block\ncould NOT be filled to capacity, so a block that filled exactly never\nsignalled end-of-input until the next zero-byte read.\n\n- Slice feeder: report EOF when the slice is exhausted even if the block\n  filled exactly (the slice knows its own length).\n- Reader feeder: when a block fills exactly to capacity, probe one byte\n  ahead — a 0-byte read marks this block last; a real byte is stashed and\n  prepended to the next block. Mirrors the C encoder on ZSTD_e_end.\n\nRegression test covers both feeders x {incompressible, compressible} x\n{1,2,3} full blocks, asserting no trailing empty block, the last real\nblock carries the flag, and round-trip equality. Verified across strategy\ntiers (fast / dfast / btopt / btultra2) via the block-structure bench\nreport: high-entropy-1m and decodecorpus-1m now emit 8 blocks matching C\n(was 9 with the trailing R0).\n\n* fix(huff0): match C weight-description FSE/direct floor threshold\n\nUpstream HUF_writeCTable_wksp keeps the FSE-compressed weight description\nonly when hSize < maxSymbolValue/2 (FLOOR). Our gate used div_ceil, one byte\ntoo permissive on odd symbol counts, keeping FSE weights in a boundary band\nwhere upstream emits the cheap direct nibbles — a fractionally smaller frame\nthat costs the decoder an FSE weight-table build instead of a nibble unpack.\n\nAlso add a REPORT_HEX frame-head dump to compare_ffi (alongside REPORT_BLK)\nso rust-vs-ffi literal-section / Huffman-tree-description generation can be\ndiffed byte-for-byte.\n\n* fix(dfast): match C double_fast hash-table insertion shape\n\nAlign the dfast hash-table inserts with upstream zstd_double_fast.c so the\nmatch-finder resolves the same candidates and produces C-comparable offset\nstreams (the level_3 ratio gap on repetitive log data traces to divergent\nhash state, not the search itself):\n\n- Immediate-repcode chain inserted curr+2 / ip-2 / ip-1 (the primary-match\n  complementary set). Upstream instead writes BOTH tables at the rep\n  position itself (zstd_double_fast.c:314-315). Insert at the rep position;\n  the previous set left rep-position keys stale.\n- Complementary insertion after a stored match is ASYMMETRIC upstream\n  (:300-304): hashLong at curr+2 and ip-2; hashSmall at curr+2 and ip-1.\n  The previous form wrote curr+2/ip-2/ip-1 into both tables, polluting the\n  long table with ip-1 and the short table with ip-2.\n- Add the `if (step < 4) hashLong[hl1] = ip1` lookahead insert (:287) on the\n  long-at-ip0 and short/_search_next_long commit paths.\n\nAdds `insert_long` / `insert_short` over a const-generic `insert_masked`\ncore, so the symmetric `insert_position` stays byte-identical while the\nasymmetric variants emit a single write each.\n\nbyte-identical round-trip across 796 tests.\n\n* fix(dfast): restore repcode path; drop bogus literals-start gate\n\nThe dfast rep1 peek required the repcode candidate to sit at or after the\ncurrent literal cursor (`cand_pos_r >= literals_start`). After a stored\nmatch `literals_start` sits right at the cursor, so essentially every\nback-reference failed that test: the repcode path almost never fired and\nevery position fell through to the long hash, which minted a creeping /\nfar offset stream instead of a stable repcode. Upstream zstd\n(zstd_double_fast.c:190) gates the rep on nothing but a non-zero offset and\nthe 4-byte equality at `ip+1-offset_1`, so keep only the window-low bound.\n\nEffect on the level_3 dfast ratio (rust vs c_ffi bytes):\n  large-log-stream     11222 -> 8942  (was +15% over C, now beats C 9744)\n  decodecorpus-1m        249 -> 229   (now beats C 239)\n  low-entropy-1m         154 -> 148\n\nAdds a `DFTRACE`-gated per-commit path/offset trace (off by default) used to\npin the divergence.\n\n* fix(dfast): match upstream sequence generation byte-for-byte\n\nTwo divergences made the dfast match-finder emit a different sequence stream\nthan upstream zstd, compounding across a block (worse ratio, and our own\ndecoder is slower on the non-canonical stream):\n\n- Step ramp: the skip step grew one position every \"1 << kSearchStrength\"\n  bytes (upstream kStepIncr; kSearchStrength = 8, so 256). The port used 6\n  (64 bytes) and capped the step at 8. So it accelerated ~4x too soon and\n  stopped growing, skipping source positions upstream still inserts and\n  missing the short matches upstream finds near a block start. Restore 8 and\n  drop the cap.\n- Complementary insertion anchor: upstream inserts curr+2 where curr is the\n  iteration SCAN position (fixed at zstd_double_fast.c:184), not the match\n  start. For a rep1 the match begins at scan+1 and a backward catch-up rewinds\n  the start, so anchoring on the match start shifted every rep1/extended insert\n  by +1, seeding the short hash with wrong positions. Thread the scan position\n  through and anchor curr+2 on it.\n\nLevel_3 sequence comparator is now 100% identical to upstream across all\nfixtures (was 70-90%); small-4k-log-lines compresses to a byte-identical frame.\nbyte-identical round-trip across 796 tests.\n\n* perf(huf): gate the table-log search by size for DoubleFast too\n\nThe optional table-log search trades encode time for a smaller literal\nsection. The Fast strategy already skipped it below one block (128 KiB),\nwhere per-frame HUF setup dominates and the cheap single-build ties upstream\nzstd's ratio. DoubleFast has the same profile (little matching work, so HUF\nsetup is a large fraction on a small frame), so extend the size gate to it.\n\nNow that dfast emits an upstream-identical sequence stream, a small dfast\nframe with the search off compresses byte-for-byte like upstream; large\nframes keep the search and beat upstream on the literal section. Renames\nfast_huf_search_enabled to huf_search_enabled since it no longer covers only\nFast.\n\n* perf(decode): drop per-symbol reload in HUF drain, mirror HUF_decodeStreamX1\n\nThe 4-stream HUF literal drain reloaded the bit reader on every symbol, so each\ntrailing symbol near a stream end paid the cold refill_slow path - the dominant\ndrain cost on a literal-heavy frame. Mirror upstream zstd HUF_decodeStreamX1:\ndecode in groups of four with one reload per group, then the final under-four\nsymbols after a single reload with NO per-symbol reload. Termination is\noutput-based (cursor < segment end), like upstream's p < pEnd, so the loops\nnever read past the last symbol into the zero padding.\n\nThe end-of-stream validation is unchanged and still exact: bits_remaining =\n(64 - bits_consumed) - extra_bits is independent of reload timing (padding\nconsumed at the stream end lands in either extra_bits via refill or\nbits_consumed without it, and the difference is identical), so dropping the\nper-symbol reloads does not shift the -max_bits check. A group reload keeps\nbits_consumed bounded, so the no-reload decodes never overflow the container.\n\nbyte-identical round-trip across 796 tests incl. the 1000-iteration random\nsuite.\n\n* perf(decode): bulk-read the variable frame-header tail\n\nThe frame-header parser issued a separate trait read_exact per field (magic,\ndescriptor, window descriptor, dictionary id, frame content size). On a tiny\nframe those per-field reads - each a generic Read::read_exact loop plus a\nmap_err - dominated decode (the header parse was ~45% of a 1 KiB random-frame\ndecompress). The variable tail sizes are all encoded in the descriptor, so read\nthe whole tail (window descriptor + dict id + frame content size, at most 13\nbytes) in one read_exact and parse it by direct indexing, mirroring upstream\nzstd ZSTD_getFrameHeader slicing the header out of the source in one shot. Five\nreads drop to three. byte-identical across 796 tests.\n\n* perf(io): direct read_exact for slice readers\n\n* perf(decode): inline the per-frame init chain for a flatter decode path\n\n* fix(decode): restore per-field frame-header read errors\n\nThe bulk-read of the variable header tail collapsed the per-field reads into a\nsingle read_exact, leaving WindowDescriptorReadError / DictionaryIdReadError /\nFrameContentSizeReadError unreachable in the public error surface. Read each\nfield with its own error again (the slice read_exact override keeps each a\nsingle bounded copy, so the per-field reads carry no real cost), and address the\nreview round:\n\n- frame_compressor: the huf_optimal_search field docs now mention DoubleFast is\n  size-gated too, matching huf_search_enabled.\n- dfast: drop the stale \"growth interval 64, deviates from upstream\" comment;\n  it is 256 and matches upstream kStepIncr now.\n- tests: the exact-block-multiple regression now also runs a non-default\n  target_block_size, exercising the EOF path for both one-shot and streaming.\n- bench: compare_ffi's block-structure helper reports parse=error on a\n  truncated/invalid frame instead of a misleading n_blocks=0.\n\n* fix(bench): require a terminal last-block in the block-structure report\n\nThe compare_ffi block-structure helper reported a normal REPORT_BLK even when\nthe payload ended without a terminal `last` block (including 1-2 trailing bytes\nthat cannot form a block header), making truncated frames look valid. Track\n`saw_last` and require both a terminal block and exact payload consumption\nbefore printing the block list, else report `parse=error`.\n\nAlso trim the stale sparse-3-target narrative from the dfast immediate-repcode\ninsertion comment: it argued `abs_pos` is not inserted, contradicting the actual\n`insert_position(abs_pos)` (upstream zstd_double_fast.c:314-315). Keep only the\nimmediate-repcode rationale that matches the code.",
          "timestamp": "2026-06-26T15:03:33+03:00",
          "tree_id": "4c43f0440aa2ece3ca5e9e944219d4c47a5c14e3",
          "url": "https://github.com/structured-world/structured-zstd/commit/844e95a3f7c8586eea7f39ff231fb928309d46d2"
        },
        "date": 1782477927839,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 221.08,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 243.784,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.547,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.244,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.855,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.979,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.704,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.894,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.029,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.029,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.685,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.177,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.069,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.223,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.405,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.018,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.391,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "562f3806b91dcd2ccd9253ad5abc365a3ca98a09",
          "message": "chore: release v0.0.45 (#451)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-06-26T18:39:16+03:00",
          "tree_id": "5ba9d0d433922a3a72393589919b0a5070fd6856",
          "url": "https://github.com/structured-world/structured-zstd/commit/562f3806b91dcd2ccd9253ad5abc365a3ca98a09"
        },
        "date": 1782490905198,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.086,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 226.568,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 243.164,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.55,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.356,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.849,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.962,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.707,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.886,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.649,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.659,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.064,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.205,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.388,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.06,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.377,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.155,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "7b527411f28a683076ce122a6aa46ae985bcb088",
          "message": "perf(levels): consolidate cparams + negative-band byte-parity with C (#454)\n\n* perf(levels): consolidate cparams, negative-band byte-parity, decode-perf\n\nResolve every level's compression parameters through the single C-faithful\n`get_cparams` source (port of `ZSTD_getCParams`), dropping the dead hand-tuned\n`LEVEL_TABLE` / BtUltra2 configs and the parallel resolver. Route the\nparam-override source-size re-cap through the same byte-verified `adjust_cparams`\n(`ZSTD_adjustCParams_internal`) so the override and level paths down-size\nidentically, removing a divergent helper and its non-C 16 KiB hinted-window floor.\n\nNegative compression levels now match C byte-for-byte: raw (uncompressed)\nliterals when literal compression is disabled, the fast-matcher repcode policy\n(offBase 1 only), and the buildCTable last-symbol histogram adjustment.\n`set_compression_level` resyncs the literal-disable gate so a reused compressor\nswitched to a negative level emits raw literals (regression-tested).\n\nDecode: port C's two-stage branchless FSE decode-table spread for the\nno-low-prob-count case, and make the inline FSE decode table `MaybeUninit` so a\nfresh `FrameDecoder` skips the ~13 KiB per-table memset that dominated\ntiny-frame decode (z000001 24 B decode 0.13x -> 0.36x of C).\n\nBenches: per-level byte-diff in ratio_parity, round-trip validation across all\ncodecs in the comparison example, negative-band level-list parsing + corpus-path\nfallback in compare_ffi_sequences, and small-input encode/decode harnesses.\n\n797 tests pass; our-vs-C cross-validation and fuzz-interop round-trips green;\nclippy + fmt clean.\n\n* fix(lint): silence too_many_arguments on choose_table_from_counts\n\nThe FSE table selector takes eight cohesive inputs, each carrying its own perf /\ncorrectness rationale inline (the no-copy `&mut` histogram, the caller-tracked\n`max_symbol` / `last_code` that avoid a per-stream rescan). `cargo clippy\n-D warnings` (the CI gate) rejects the 8/7 count; `#[expect]` it with a reason so\nthe documented per-arg rationale stays at the signature instead of being\nrelocated into a struct.\n\n* test(fse): add regression for fast-spread overrun on oversized tables\n\n`build_from_probabilities` accepts acc_log up to ENTRY_MAX_ACCURACY_LOG (16),\nbut the no-low-prob fast-spread path lays symbols into a fixed `512 + 8` scratch\nand writes the decode table into a `[_; CAP]` array (CAP = 512 for sequence\ntables). A valid no-`-1` table with table_size > CAP overruns both. This test\ndrives a 1024-entry sequence table through the public entry and currently panics\n(\"range end index 528 out of range for slice of length 520\"); it passes once the\nbuild rejects table_size > CAP.\n\n* fix(fse): reject FSE tables larger than the decode array capacity\n\n`build_decoding_table_inner` writes the decode table into a `[E; CAP]` array and\nthe no-low-prob fast path lays symbols into a fixed `FSE_FAST_SPREAD_BUF` (512+8)\nscratch. Both assume `table_size <= CAP`, which the wire path guarantees\n(sequence acc_log <= 9, HUF <= 6) but `build_from_probabilities` did not — it\ncaps acc_log at ENTRY_MAX_ACCURACY_LOG (16), so a fuzz / external caller could\ndrive a table_size > CAP and panic-overrun the scratch (\"range end index 528 out\nof range for slice of length 520\") before the table was built.\n\nReject `table_size > CAP` up front as a typed `AccLogTooBig`, so the invariant\nboth the decode array and the fast-spread scratch rely on is enforced once.\nPasses the regression test from the preceding commit; full suite green (798).\n\n* test(bench): fail fast on C-decode failure, mark unsupported zrip levels N/A\n\nratio_parity: panic with context when the C decoder cannot decode `ours`\n(`expect` + `assert_eq`) instead of printing `false` and exiting 0 on the exact\ndrop-in regression the example exists to catch.\n\nzrip_compare: carry the zrip/C ratios as `Option` and print `N/A` for levels\nzrip does not support, instead of `0.00x` that read as real throughput misses in\nboth the per-level table and the worst-first summaries.\n\nAlso clarify the `build_seq_ctable` last-symbol normalization: C gates both\n`count[last]--` and `nbSeq_1--` on `count > 1` (zstd_compress_sequences.c:271-273),\nso a count of exactly 1 normalizes against the full `nbSeq`, not `nbSeq - 1` —\nthe doc and an inline comment now spell this out so it is not misread as a\nparity divergence.\n\n* test(encode): cover literal-disable under a non-fast strategy override\n\n`set_parameters` derives `literal_compression_disabled` from the signed level,\nbut C `ZSTD_literalsCompressionIsDisabled` keys it off the RESOLVED cParams\n(`strategy == fast && targetLength > 0`). A negative level overridden onto a\nnon-fast strategy (e.g. BtUltra2) should keep literal (Huffman) compression\nenabled; the current code wrongly forces raw literals. This test fails until the\nflag is computed from the resolved strategy/target length.\n\n* fix(encode): derive literal-disable from resolved strategy, not signed level\n\n`set_parameters` forced `literal_compression_disabled` from `level < 0`, so a\nnegative level overridden onto a non-fast strategy (e.g. BtUltra2) kept raw\nliterals even though the resolved strategy is no longer fast. Compute it like C\n`ZSTD_literalsCompressionIsDisabled` (ps_auto) instead: `strategy_tag == Fast &&\ntargetLength > 0`, using the already-resolved (overridden) strategy tag and the\ntarget-length override (falling back to `level < 0`, which for the fast strategy\nis exactly `targetLength > 0`). The constructors and `set_compression_level`\ntake no strategy override, so their native-strategy `level < 0` stays correct.\n\nPasses the regression test from the preceding commit; full suite green (799).\n\n* fix(levels): include negatives in BENCH_LEVEL=all, correct BT search_mls doc\n\ncompare_ffi_sequences: `STRUCTURED_ZSTD_BENCH_LEVEL=all` now sweeps the negative\nband too (`-7..=MAX`, dropping 0), so the natural \"run everything\" setting does\nnot silently skip the negative-level coverage the per-level parser already\naccepts.\n\nconfig.rs: the BT-finder `search_mls` doc claimed `BOUNDED(4, minMatch, 6)`, but\nupstream `ZSTD_selectBtGetAllMatches` (zstd_opt.c:896) uses\n`BOUNDED(3, minMatch, 6)`. Every BT level's minMatch is already in `[3, 6]`\n(btultra/btultra2 L18-22 = 3), so the existing `search_mls = cp.min_match` is\ncorrect; clamping up to 4 would build a 4-byte BT hash on levels 18+ and diverge\nfrom C's 3-byte one. Doc corrected to prevent that mis-\"fix\".\n\n* build(deps): bump zrip to 0.7 (bench-only comparison codec)\n\n* fix(levels): clamp BT search_mls to >=4 to hold level-22 sequence parity\n\nThe cparams consolidation set `search_mls = cp.min_match`, which is 3 on the\nbtultra/btultra2 rows (L18-22). C's BT finder does use mls=3 there\n(`BOUNDED(3, minMatch, 6)`, zstd_opt.c:896) and surfaces 3-byte matches via a\nfallback-only HC3 finder, but our optimal parser does not yet replicate that\n3-byte handling — it emits short matches C prices out, which broke\n`level22_sequences_match_reference` (our sequences diverged from C's:\n1217 vs 1160 sequences, first split at idx 44).\n\nRestore the long-standing workaround: clamp the BT hash width up to 4 so the\nfinder stops surfacing the 3-byte matches our parser mis-prices, matching C's\noutput again. This is deliberately NOT C's finder width; the proper fix is to\nmake the optimal parser C-faithful at minMatch 3 (tracked in #337), after which\nthe clamp can be dropped. Corrects the earlier doc that wrongly claimed the\nclamp would diverge from C.\n\nVerified: full lib suite (799) and the bench_internals ffi suite (58, incl. the\nlevel-22 sequence-parity test) green.\n\n* test(bench): make the zrip_compare speed-miss threshold explicit\n\nThe speed-misses summary filters at ours/C < 0.95, not < 1.0, so sub-5%\nslower-than-C decodes are intentionally excluded. Rename the header to state the\n>5% threshold so the report does not read as \"every case below 1.0\".\n\n* test(encode): make the dict-improves-compression payload dict-covered\n\nThe dict test compressed a payload whose lines only shared a 24-byte prefix with\nthe trained dictionary while otherwise self-repeating, so the dict's marginal\ngain was below the dict-id frame overhead — the assertion passed by a single\nbyte. The cparams consolidation legitimately improved the no-dict baseline\n(level-1 Fastest 234 -> 221 bytes, now at/under C), which erased that 1-byte\nmargin and flipped the test.\n\nUse a payload the dictionary actually covers (the training line shape with unseen\n`idx` values) so the dict's benefit is the first occurrence's full-line literals\n— substantial and unambiguous — instead of a fragile sub-overhead margin. The\ndeeper partial-match dict ratio gap is a separate concern, not gated by this\nround-trip + basic-benefit test. The failure message now reports both sizes.\n\nVerified with the CI feature set (-F hash,std,dict_builder): full lib suite and\nthe bench_internals ffi suite green.\n\n* fix(fse): wrap the fuzz_exports read_entry assume_init in unsafe",
          "timestamp": "2026-06-27T22:32:57+03:00",
          "tree_id": "8f6cbfc1eb21e34f61e0dab8bf841c7701957bc3",
          "url": "https://github.com/structured-world/structured-zstd/commit/7b527411f28a683076ce122a6aa46ae985bcb088"
        },
        "date": 1782591339705,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.083,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 211.206,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 227.178,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.533,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.302,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.836,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.963,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.692,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.887,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.114,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.706,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.224,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.376,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.018,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.37,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.168,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e73d9d9093f7813b97b7ee759b683393138396b1",
          "message": "perf(huf+lazy): gate HUF table-log on the size-adaptive strategy, unify the lazy parser (#455)\n\n* perf(huf): gate optimal-depth tableLog search to btultra+ (match C)\n\n* perf(huf): match C FSE_optimalTableLog for the cheap-path literal tableLog\n\n* fix(lazy): match C depth-2 lookahead bias (+3 increment, not +4)\n\n* refactor(lazy): extract shared C-faithful lazy commit/defer decision\n\nHoist the lazy lookahead's commit-vs-defer decision (the gain comparison,\ndepth-1/depth-2 bias, bounds) into one shared helper, mirroring upstream's\nsingle ZSTD_compressBlock_lazy_generic instead of a per-strategy copy. Wire the\nHC matcher to it (byte-identical) and route HcMatcher::match_gain through the\nshared gain fn. Row and dfast follow.\n\n* refactor(lazy): wire Row lookahead to shared C-faithful driver\n\nRow's lazy lookahead used a raw match-length comparison; route it through\nlazy_parse::lazy_should_commit so it weighs candidates by upstream's gain\n(match_len*4 - offset_bits) like HC/C, keeping Row's carry-forward (the FnMut\nclosure captures the ip+1 result) and its no-sufficient-length policy\n(target_len = MAX). Behavior-changing on the Row band (the lazy outsiders);\nto be ratio+speed verified on the bench host before merge.\n\n* Revert \"refactor(lazy): wire Row lookahead to shared C-faithful driver\"\n\nThis reverts commit dc493a53270692e1fae1545ea88a92f49217c914.\n\n* Reapply \"refactor(lazy): wire Row lookahead to shared C-faithful driver\"\n\nThis reverts commit d6eb1f3da4e0c8fc43ba7f8eff6b442fd20b3a1c.\n\n* refactor(lazy): convert shared lazy decision to an inline macro\n\nThe fn+closure driver de-inlined Row's #[target_feature] finder across the call\nboundary (+~6% on the lazy band, measured at dc493a53). Replace it with a\nmacro_rules! that splices the finder inline at the call site — upstream's lazy\nparse is one FORCE_INLINE_TEMPLATE with the search method as a template param,\nso the finder is never behind a call boundary. HC keeps its out-of-line finder\n(it ignores the carry and re-picks), Row inlines its finder and uses the carry.\nOne source for the C-faithful gain decision, no per-strategy copies.\n\n* refactor(matcher): route HC back-extension through extend_backwards_shared\n\nHC carried its own copy of the catch-up loop; Row already delegates to\nmatch_table::helpers::extend_backwards_shared. Make HC a thin wrapper too so the\nback-extension (upstream zstd_lazy.c:1707-1718 parity) has one implementation.\nByte-identical.\n\n* refactor(lazy): reconcile the shared lazy decision to the length heuristic\n\nThe gain weighting (committed earlier) cost ~6% on the lazy band with no ratio\nwin — the length heuristic that Row already used is faster and still beats C\n(drop-in, not binary parity). Switch lazy_decide! to length: Row's monolith\nreturns to its prior (faster) behaviour, HC moves from gain to length (one\nsource). best_off is bound once; with target_len=usize::MAX (Row) the\nsufficient-length early-out folds away, so the hot loop gains no branch.\n\n* refactor(lazy): route the test-only Row lazy probe through lazy_decide!\n\npick_lazy_match_rl (a dead-in-production, test-only path) carried its own copy\nof the lazy decision via pick_lazy_match_shared + LazyMatchConfig. Route it\nthrough the same lazy_decide! macro the production monolith uses and delete the\nnow-unused fn + config struct. The tests now exercise the exact decision that\nships. Byte-identical.\n\n* fix(huf): gate literal HUF search on the size-adaptive strategy\n\nThe literal-compression / HUF table-log-search gate read strategy_tag from the\nbare compression level, but the matcher resolves strategy size-adaptively\n(upstream ZSTD_getCParams: a <=16 KiB frame promotes levels 13-17 to\nbtultra/btultra2). On small frames the gate therefore thought btlazy2/btopt and\nskipped the HUF table-log search the matcher's btultra frame should run,\novershooting C by 4 bytes on the small-4k-log-lines literal section (L13-17).\n\nResolve strategy_tag through the same resolve_level_params -> get_cparams path\nthe matcher uses, so the gate and the parse agree. small-4k-log-lines L13-17 go\n150 -> 146, matching C; full local ratio matrix shows no new rust>ffi cases.\n\n* refactor(hc): select repcode-vs-chain by length, matching C and the chain walk\n\nHcMatcher::better_candidate ranked the repcode probe against the hash-chain\nmatch by gain (ml*4 - offset_bits), while the chain walk itself, lazy_decide!,\nand the shared Row/Dfast repcode probe all rank by length. Upstream\nZSTD_compressBlock_lazy_generic compares the repcode against the searched match\nby length at depth 0 (ml2 > matchLength, the repcode keeps ties). Rank by length\n(ties to the smaller offset) so HC's selection is internally consistent and\nC-faithful. Drops the now-unused match_gain / lazy_match_gain.\n\n* refactor(hc): share the lazy back-extension via extend_backwards_shared\n\nThe HC lazy loop hand-inlined the catch-up back-extension with slice indexing,\na fourth copy of the same logic the Row / Dfast probes reach through\nmatch_table::helpers::extend_backwards_shared. Call the shared helper (it is\n#[inline] and raw-pointer, so the hot loop also sheds the per-step slice bounds\nchecks). One back-extension source; byte-identical output.\n\n* perf(hc): carry the lazy lookahead instead of re-searching the deferred position\n\nThe HC lazy loop searched the lookahead position inside pick_lazy_match, threw\nit away, then re-searched it next iteration (double work on every defer). Carry\nthe lookahead match forward like the Row parser and upstream's lazy depth loop\n(each position searched once). Insert abs_pos before the lookahead so the\nabs_pos+1 probe sees it at offset 1 — matching upstream, where the searched\nposition is inserted during its own search before the depth loop probes the next.\nBehavior-changing (the defer decision now sees the offset-1 candidate); i9 ratio\n+ speed verification follows.\n\n* perf(hc): return the lazy carry decision in registers (16-byte HcMatch)\n\nlazy_decide_carry returned Option<Option<HcMatch>> (>16 bytes, stack-returned\nvia sret), marshalled across the hot per-position boundary on every match — it\nspilled the register-tight lazy2 (depth-2) monomorph and cost ~2.3% on the\nsmall-10k-random L10/L12 band. Flatten the return to a bare 16-byte HcMatch\n(System V rax:rdx): the NONE sentinel means commit, a real match means\ndefer-carry. Byte-identical output.\n\n* fix(no-std): keep lazy_parse ungated, gate ldm on the hash feature\n\nDeclaring pub(crate) mod lazy_parse wedged it between the #[cfg(feature = hash)]\nand the mod ldm that attribute guards, so lazy_parse became hash-gated (absent in\nno_std core compression) and ldm lost its gate (unresolved twox_hash without the\nhash feature). lazy_parse is core; ldm is the hash-gated module.\n\n* fix(hc): defer on the depth-2 no-carry lazy lookahead instead of committing\n\nlazy_decide_carry collapsed the macro's Some(None) into the NONE sentinel, which\nthe generator read as COMMIT. Some(None) is reachable at lazy_depth>=2 when the\nabs_pos+2 probe wins but abs_pos+1 has no match (cold bucket); it must DEFER one\nbyte (searching the next position fresh), or lazy2 misses the two-ahead match.\nReturn Option<HcMatch>: None=commit, Some(real)=defer-with-carry,\nSome(NONE)=defer-without-carry.\n\nResolve the eager set_parameters strategy_tag sync size-adaptively through the\nsame resolve_level_params path prepare_frame uses, not the bare level mapping.\nReword the row lazy_parse_body inline note: the decision weighs by length (ties\nto the smaller offset), matching the macro after the gain helper removal.\n\nAdd lazy_parse macro-contract tests: the Some(None) defer, the carry defer, the\nplain commit, and the target_len early-out.\n\n* fix(lazy): pin a one-byte advance after a depth-2 defer-without-carry\n\nWhen the lazy lookahead defers because abs_pos+2 won but abs_pos+1 was cold\n(the Some(None) / defer-without-carry case), the deferred position carries no\nmatch. If abs_pos+1 then misses, the no-match skip heuristic (step grows with\nthe literal-run length) could hop past the already-proven abs_pos+2 winner.\n\nTrack that state in both lazy parsers and pin the next advance to one byte on\nthe following miss so abs_pos+2 stays reachable, matching upstream's lazy depth\nloop (it steps one position at a time, never skipping a proven match):\n- HcMatcher generator: deferred_without_carry -> forced_single_step.\n- Row lazy_parse_body: deferred_from_prev keeps the one-byte advance.\n\nAlso align the test-only Row pick_lazy_match_rl with the production row body:\ntarget_len = usize::MAX (lazy has no sufficient-length early-out).",
          "timestamp": "2026-06-29T16:24:31+03:00",
          "tree_id": "25eda9a826138b7b124e5901f7f43106654d572a",
          "url": "https://github.com/structured-world/structured-zstd/commit/e73d9d9093f7813b97b7ee759b683393138396b1"
        },
        "date": 1782742026445,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.113,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 217.556,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 225.209,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.54,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.226,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.85,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.962,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.702,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.887,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.57,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.567,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.216,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.408,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.06,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.373,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.155,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "01f5407207da752ff68276c47cec0d74810faf11",
          "message": "chore: release v0.0.46 (#453)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-06-29T17:15:13+03:00",
          "tree_id": "bfaae93e60938d521b59a8c13952859884c2d387",
          "url": "https://github.com/structured-world/structured-zstd/commit/01f5407207da752ff68276c47cec0d74810faf11"
        },
        "date": 1782745165584,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.082,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.113,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 211.135,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 225.146,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.614,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.309,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.844,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.969,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.7,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.892,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.289,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.618,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.069,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.215,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.383,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.063,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.37,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.05,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.156,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "7b005e46fa88a72c4197ccdaad767f3ac6d15ab2",
          "message": "perf(decode): cut per-frame fixed overhead on the in-memory path (#456)\n\n* chore(bench): drop the redundant no-dict arm from the compress-dict matrix\n\nc_ffi_without_dict re-measured C compressing the same level/scenario WITHOUT a\ndictionary — already covered by the plain compress/{level}/{scenario}/matrix/c_ffi\ngroup. It also built the CCtx per-iter while the two dictionary arms are\nsteady-state, so it was not even a clean baseline, and the REPORT ratio reads\nthe with-dict sizes, not this arm. Keep only c_ffi_with_dict and\npure_rust_with_dict.\n\n* perf(decode): skip the per-frame entropy-table reset when unbuilt\n\nThe per-frame scratch reset unconditionally cleared all three sequence FSE\ntables and the Huffman table (6 buffers + a [0; 13] rank-count memset) even on\nRAW / RLE / raw-literal frames that never build any entropy table. Upstream zstd\nZSTD_decompressBegin never clears entropy per frame — it marks tables invalid by\nflag and rebuilds lazily. Gate each table reset on whether it holds a built\ntable (FSE accuracy_log != 0, Huffman max_num_bits != 0): resetting an\nalready-empty table is a no-op on observable state, so this is byte-identical\nand just drops the wasted clears on entropy-less frames.\n\n* perf(decode): direct copy for a single-RAW-block frame\n\nA frame that is one RAW block spanning the whole declared content (the shape\nincompressible / already-compressed payloads always take) went through the full\ndirect-decode machinery: DirectScratch / DecodeBuffer / UserSliceBackend wrapper\nconstruction plus the general per-block loop, for what is ultimately a single\nmemcpy. Mirror upstream zstd's ZSTD_copyRawBlock: detect the single last RAW\nblock whose size equals the frame content size and copy it straight into the\ncaller slice, with the same checksum / counter bookkeeping the general path\ndoes. Byte-identical; falls through to the general path for every other shape.\n\n* perf(decode): parse the frame header straight from the input slice\n\nThe in-memory decode path holds the input as a &[u8], but the header parse went\nthrough read_frame_header_with_format's generic Read interface: 3-5 per-field\nread_exact calls dispatched through the io::impls Read-for-&[u8] shim plus\nper-byte little-endian assembly loops. Add read_frame_header_from_slice — direct\nbyte indexing + from_le_bytes, advancing the slice exactly as the Read version\ndoes (identical skippable-frame / truncation contracts) — and route decode_all /\nthe skippable-visitor path through a reset_from_slice that uses it, sharing the\nparsed-header apply with the Read path. Mirrors upstream zstd parsing the header\nfrom a raw pointer. Byte-identical; the streaming Read path is unchanged.\n\n* test(fse): add regression for is_populated missing RLE tables\n\nbuild_rle sets decode_len=1 but leaves accuracy_log=0, so the accuracy_log-based\nis_populated misreports an RLE sequence table as unpopulated. The per-frame\nscratch reset gates on is_populated, so a used RLE table is not cleared and a\nlater Repeat-mode frame reads stale state. Failing test pins decode_len as the\nbuilt signal.\n\n* fix(decode): detect RLE tables in is_populated; consume checksum without hash\n\nis_populated now tests decode_len != 0, not accuracy_log != 0: a valid RLE\nsequence DTable has decode_len = 1 but accuracy_log = 0, so the accuracy_log\ncheck left a used RLE table uncleared by the per-frame scratch reset, letting a\nlater Repeat-mode frame read stale RLE state. decode_len is the same built/\nuninitialized signal init_state uses.\n\nIn the single-RAW-block fast path, the trailing content-checksum read was wholly\nbehind #[cfg(feature = hash)]. Without hash a checksummed frame left the 4 bytes\nin the input (misparsed as the next frame) and never set check_sum (is_finished\nstayed false). Move the byte consumption + counter + check_sum out of the cfg —\nmatching the general direct loop and decode_blocks, which gate only the hashing.\nVerified no-std/no-hash builds clean; the bug is build-config-specific so it is\nnot exercisable in the hash-enabled test suite.",
          "timestamp": "2026-06-29T23:26:43+03:00",
          "tree_id": "ba06bd2d05f5e36969665af9e5d6d38f74ab4dbc",
          "url": "https://github.com/structured-world/structured-zstd/commit/7b005e46fa88a72c4197ccdaad767f3ac6d15ab2"
        },
        "date": 1782767392613,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.085,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.112,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 219.975,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 250.661,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.568,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.319,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.845,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.96,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.705,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.881,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.35,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.197,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.068,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.223,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.374,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.017,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.355,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e29b9e7b312ffd0929456d29b42595767bcacc76",
          "message": "perf(codec): dict-band hot-path — slice header parse, dms-walk floor, SIMD prefix leading word (#458)\n\n* perf(decode): slice-direct header parse on the dict-handle path\n\ndecode_all_with_dict_handle still routed its per-frame header read through\nthe Read-trait reset_with_dict_handle (per-field read_exact via the io::impls\nshim) while the non-dict decode_all already parsed the header straight from\nthe input slice. Add reset_from_slice_with_dict_handle, mirroring the\nRead-based dict reset but reading the header via read_frame_header_from_slice\nand applying it through the shared parsed-header path, then attaching the\ndictionary. Wire decode_all_with_dict_handle through it.\n\nByte-identical (dict cross-validation + fuzz_interop round-trips pass).\n\n* perf(compress): hoist the dict-walk reachability floor out of the loop\n\nThe lazy hash-chain dict-match walk recomputed the candidate offset\n(current_idx - dict_idx) every iteration solely to gate on decoder\nreachability (offset <= max_window_size). The offset is only consumed when a\ncandidate WINS, which is rare on the walk. Hoist the equivalent floor\n(current_idx.saturating_sub(max_window_size)) once before the loop and gate on\ndict_idx >= floor; the per-iteration subtract moves to the (rare) win branch and\nthe max_window_size register is freed inside the hot loop. Mirrors C's\nZSTD_HcFindBestMatch dms loop, which bounds reachability by chain construction\nrather than re-checking each candidate.\n\nByte-identical: dict_idx >= current_idx - max_window_size is algebraically the\nsame gate as current_idx - dict_idx <= max_window_size given dict_idx <\ncurrent_idx (dict positions precede the cursor). Dict cross-validation +\nfuzz_interop round-trips pass.\n\n* perf(compress): leading scalar word probe in the SIMD common-prefix kernels\n\nThe SIMD common_prefix_len_ptr (avx2 / sse42 / neon / simd128) ran the wide\nvector loop even when the prefix diverges within the first 8 bytes. On BT-tree\nnode compares — the optimal parser calls the seeded prefix counter per visited\nnode, and each node extends the running seed by only a short run before the\nsmaller/larger split — that divergence is almost always inside the first word,\nso the vector load + compare is pure overhead. Upstream C ZSTD_count leads with\none MEM_readST (8-byte) check and returns on a mismatch there; mirror it with a\nleading scalar word probe, falling through to the vector loop only when the\nfirst word matches (long matches).\n\ni686 (ours-vs-c_ffi, flat control): compress-dict L13 small-10k-random\n782->725us (-7.2%); compress L19 btultra2 large-log-stream 64.5->59.5ms\n(-7.7%) — the long-match path benefits too, since per-node tree extensions are\nshort regardless of total match length. Byte-identical (838 lib + 59 ffi incl\ncross-validation; scalar already word-at-a-time). The scalar fallback kernel is\nunchanged.",
          "timestamp": "2026-06-30T11:13:09+03:00",
          "tree_id": "067a219049ecd06d55e2256641436a0825b4cc86",
          "url": "https://github.com/structured-world/structured-zstd/commit/e29b9e7b312ffd0929456d29b42595767bcacc76"
        },
        "date": 1782809725791,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.082,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.085,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 239.314,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 247.774,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 1.018,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.816,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.778,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.948,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.653,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.872,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.154,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.773,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.072,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.218,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.39,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.059,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.372,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.155,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e20b0b3cb83e9e1ec8d7903e6e2718efd9a0c491",
          "message": "chore: release v0.0.47 (#457)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-06-30T12:00:17+03:00",
          "tree_id": "c34058c4420a8fb455846c4e327e53ddb63ad399",
          "url": "https://github.com/structured-world/structured-zstd/commit/e20b0b3cb83e9e1ec8d7903e6e2718efd9a0c491"
        },
        "date": 1782812781333,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.083,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.113,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 193.645,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 226.672,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.532,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.185,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.837,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.958,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.695,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.877,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.285,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.295,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.071,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.214,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.389,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.063,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.376,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.049,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.156,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.035,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e9a85a7eca6d35eb00b6821dcacd200fde7c2737",
          "message": "fix(encoding): bound the dms-table hash-log floor by the matcher hash_log (#459)\n\n* test(encoding): regression for prime_dms_bt dms-table sizing panic\n\nExtract the dms-table hash-log sizing into `storage::dms_hash_log` and cover it:\na tiny source adjusts a BT matcher's `hash_log` below 10, and the\n`ceil_log2(region).clamp(10, hash_log)` sizing then panics 'min > max'\n(min = 10, max = 7) when priming the dictionary match binary-tree (level >= 13\nwith a dictionary). The unit test calls `dms_hash_log` with hash_log < 10 and\nFAILS on this commit; a BtUltra2 dict round-trip exercises the path end to end.\n\nFix follows separately.\n\n* fix(encoding): bound the dms-table hash-log floor by the matcher hash_log\n\nprime_dms_bt sized the dictionary match binary-tree with\nceil_log2(region).clamp(10, hash_log). When a tiny source adjusts the BT\nmatcher's hash_log below 10, the clamp floor (10) exceeds the ceiling\n(hash_log), so std clamp panics 'min > max'. Lower the floor to\nmin(10, hash_log): at hash_log >= 10 it is the unchanged 10-bit minimum, and\nbelow 10 it collapses to hash_log so the dms table just matches the (small)\nlive-table width. Byte-identical for every hash_log >= 10 configuration (all\nexisting fixtures); only the previously-panicking small-BT-dict path changes.\n\nCloses the level >= 13 + dictionary regression from 0.0.46.",
          "timestamp": "2026-06-30T13:25:34+03:00",
          "tree_id": "d606bf0ada8a7ab90d5c2583e2e04de83dbdac5f",
          "url": "https://github.com/structured-world/structured-zstd/commit/e9a85a7eca6d35eb00b6821dcacd200fde7c2737"
        },
        "date": 1782817709329,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.095,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.087,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 211.046,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 201.803,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.609,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.415,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.932,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 2,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.775,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.923,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.343,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.679,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.07,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.215,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.003,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.389,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.058,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.375,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.156,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "14e31ff4d499e8b7a2c866da6740e73d34513f99",
          "message": "chore: release v0.0.48 (#460)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-06-30T13:29:07+03:00",
          "tree_id": "2b362d6de631770b71e37e0d47d475e4668ecca5",
          "url": "https://github.com/structured-world/structured-zstd/commit/14e31ff4d499e8b7a2c866da6740e73d34513f99"
        },
        "date": 1782820507561,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.085,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 203.591,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 257.906,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.559,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.313,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.83,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.961,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.69,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.883,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.004,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 5.759,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 2.695,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.034,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.097,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 0.793,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.612,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 0.78,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.601,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.075,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.01,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.074,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "d7074b253962f814fa77ddeb756c38d605c9322f",
          "message": "perf(opt): fold BT-walk coordinate decode; drop dead hash-chain optimal path (#462)\n\n* refactor(opt): drop the non-upstream hash-chain optimal fallback\n\nThe optimal parser's per-position match-finder always used the binary-tree\nbranch (every production call site selects it): upstream zstd's optimal\nparser (ZSTD_insertBtAndGetAllMatches) is binary-tree only, with no\nhash-chain variant — the chain is upstream's lazy parser\n(ZSTD_HcFindBestMatch), a different path. The else-branch hash-chain\noptimal walk and its chain_candidates helper had no upstream counterpart and\nwere dead in every shipped binary (const-folded out, absent from objdump).\n\nRemove the dead else-branch, the chain_candidates helper, and the two tests\nthat exercised the chain optimal path through the test-only dispatcher; the\nBT path's own skip-window / rebase coverage remains. Byte-identical.\n\n* refactor(opt): drop the now-vestigial USE_BT_MATCHFINDER const generic\n\nWith the hash-chain optimal fallback gone, the per-position match-finder is\nunconditionally the binary-tree walk, so the USE_BT_MATCHFINDER const generic\n(and the macro metavars that only fed the removed chain branch:\nbt_insert_and_collect method handle, for_each_repcode, hash3) carried no\ninformation. Remove the const generic from the collect entry points and the\nfour kernel wrappers, and drop the dead macro parameters. Byte-identical.\n\n* perf(opt): fold per-node coordinate decode in the BT walk\n\nThe BT match-finder's hot per-node loop decoded the chain entry into three\ncoordinates each iteration: candidate_abs (position_base + stored - 1 -\nindex_shift), the BT pair slot (via bt_pair_index_for_abs, which re-read\nindex_shift and bt_mask and round-tripped index_shift back in), and\ncandidate_idx (candidate_abs - history_abs_start). That reloaded four struct\nfields and ran ~6 arithmetic ops per node.\n\nPrecompute the three loop-invariant biases once before the walk so each\ncoordinate is a single wrapping_add from the stored entry — the\nsingle-coordinate form of upstream zstd's window-relative matchIndex\n(match = base + matchIndex, slot = 2*(matchIndex & btMask)). Byte-identical:\nthe folded values equal the originals for every gate-validated entry.\n\n* refactor(opt): enforce binary-tree-only contract on the optimal collect\n\nThe optimal candidate collector is binary-tree only; the Fast / Dfast /\nGreedy / Lazy strategies never run the optimal parser (they drive their own\nmatch finders) and keep chain_table as an HC chain, not BT pair slots. The\npublic dispatcher's non-BT arm previously routed those tags into the BT\ncollect, which would walk the HC chain as a binary tree. Make that arm\nunreachable so misuse fails loudly, and tag the test-only shim callers as a\nBT strategy (BtOpt shares Lazy's OPT_LEVEL=0 / USE_HASH3=false consts, so the\ncollect behavior is unchanged). No production caller hit the non-BT arm (the\non-encode path goes through build_optimal_plan_impl directly).\n\n* test(opt): cover the optimal-collect dispatcher arms + BT-only contract\n\nAdds a should_panic test proving a non-BT strategy tag reaching\ncollect_optimal_candidates panics (the binary-tree-only contract), covering\nthe unreachable arm, and a dispatch test exercising every BT tag (BtOpt /\nBtUltra / BtUltra2 / Btlazy2) under the scalar kernel so the dispatcher's\nper-tag and scalar arms are covered.\n\n* test(opt): assert per-tag dispatch via the hash3 observable\n\nThe dispatch test previously only proved each BT tag ran without panicking,\nso a cross-group mis-mapping (e.g. BtUltra routed to the BtOpt\nspecialization) would slip through. Add a USE_HASH3 observable: a fixture\nwhere `abc` repeats with a differing 4th byte, so the hash3 specializations\n(BtUltra / BtUltra2) surface a length-3 match at offset 12 that the 4-byte BT\nhash misses, while the non-hash3 ones (BtOpt / Btlazy2) do not. Assert the\nmatch's presence equals the tag's USE_HASH3. (BtOpt vs Btlazy2 share identical\ncollect consts, so they are runtime-indistinguishable; that mapping is\ncompiler-enforced.)",
          "timestamp": "2026-07-01T03:08:53+03:00",
          "tree_id": "f142691bfe1a7faf911493cfec212c977b33fe3b",
          "url": "https://github.com/structured-world/structured-zstd/commit/d7074b253962f814fa77ddeb756c38d605c9322f"
        },
        "date": 1782866989717,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 190.324,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 222.74,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.611,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.264,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.839,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 2.202,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.707,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 2.088,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.031,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.55,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.071,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.24,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.384,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.057,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.37,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.047,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.155,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.188,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e7e8adff5667956696657b54d39c748c7cfa9681",
          "message": "perf(compress): inline the row search into the lazy parse monolith (#461)\n\nThe lazy row parse called an out-of-line per-tier #[target_feature] search\nmethod (`find_best_<tier>`) at both probe sites (current position + the\nlazy_decide lookahead). A #[target_feature] fn cannot inline across the call\nboundary, so every position paid call + argument-marshalling overhead — a\nlarge share of the ~2.24x instruction-count gap vs C on the lazy band, whose\nZSTD_searchMax is FORCE_INLINE_TEMPLATE into ZSTD_compressBlock_lazy_generic.\n\nSplice the rep + row-probe body (row_best_match!) inline at both sites instead,\nexactly as the greedy monolith already does, so each lazy tier kernel is one\ntarget_feature function with no per-position search call. Removed the now-unused\ngen_row_find_monolith standalone-method generator. Byte-identical (841 lib + 59\nffi incl cross-validation). Measuring decodecorpus instruction count + speed.",
          "timestamp": "2026-07-01T04:04:48+03:00",
          "tree_id": "c2ab4d52e6cc7279b0d8e1f820169369ed304561",
          "url": "https://github.com/structured-world/structured-zstd/commit/e7e8adff5667956696657b54d39c748c7cfa9681"
        },
        "date": 1782870342188,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.083,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 194.069,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 224.707,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.544,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.229,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.837,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.959,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.689,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.876,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.009,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.131,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.212,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.068,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.223,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.374,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.358,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.008,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "255865126+sw-release-bot[bot]@users.noreply.github.com",
            "name": "sw-release-bot[bot]",
            "username": "sw-release-bot[bot]"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "66342a4b1953c7b6c7e8f111a928915cad426677",
          "message": "chore: release v0.0.49 (#463)\n\nCo-authored-by: sw-release-bot[bot] <255865126+sw-release-bot[bot]@users.noreply.github.com>",
          "timestamp": "2026-07-13T12:36:44+03:00",
          "tree_id": "8e41e577d756696796422eff1e7d85dae29ac7ac",
          "url": "https://github.com/structured-world/structured-zstd/commit/66342a4b1953c7b6c7e8f111a928915cad426677"
        },
        "date": 1783938016963,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.083,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 206.426,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 271.091,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.635,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.55,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.779,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.964,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.634,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.881,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 7.988,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.977,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.053,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.171,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.046,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.79,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.046,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.784,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.129,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ac08d193734ba76ae2b1c20e53efa0f0846d43fe",
          "message": "perf(row): port the lazy parse, row/chain search and dictionary plan from upstream; match upstream CDict fill for the fast dict scan (#467)\n\n* perf(compress): inline the row search into the lazy parse monolith\n\nThe lazy row parse called an out-of-line per-tier #[target_feature] search\nmethod (`find_best_<tier>`) at both probe sites (current position + the\nlazy_decide lookahead). A #[target_feature] fn cannot inline across the call\nboundary, so every position paid call + argument-marshalling overhead — a\nlarge share of the ~2.24x instruction-count gap vs C on the lazy band, whose\nZSTD_searchMax is FORCE_INLINE_TEMPLATE into ZSTD_compressBlock_lazy_generic.\n\nSplice the rep + row-probe body (row_best_match!) inline at both sites instead,\nexactly as the greedy monolith already does, so each lazy tier kernel is one\ntarget_feature function with no per-position search call. Removed the now-unused\ngen_row_find_monolith standalone-method generator. Byte-identical (841 lib + 59\nffi incl cross-validation). Measuring decodecorpus instruction count + speed.\n\n* perf(encode): match upstream CDict fill for the fast dict table\n\nThe fast dictMatchState hash table was filled every-position last-wins\n(nearest occurrence per bucket). Upstream `ZSTD_fillHashTableForCDict`\nuses a step-3 policy: the step position overwrites its slot, the two\nintermediate positions fill an empty slot only, so a bucket resolves to a\ndifferent occurrence. Our fill diverged from that set, so the fast parse\npicked different (often nearer, shorter) dict matches than the reference,\nfragmenting long dictionary matches into several short ones and inflating\nthe offset stream.\n\nReplicate the upstream fill exactly and size the dict table to the CDict\ncParams hash-log (dict-and-window log off a minSrcSize source) rather than\nthe source-window-sized main table. The two hash-logs are now independent,\nso the kernel's main==dict hash-log equality assert is dropped (the kernel\nalready hashes dict lookups with the dict table's own hash-log).\n\nsmall-4k-log-lines compress-dict fast band: rust 56-58 B -> 44-51 B every\nlevel (level_1 now matches the reference at 44 B; the level_-2 outsider\n58 -> 50); several fast levels now match or beat it. Zero regressions on\nthe dict fixture set; round-trip and the full lib+ffi suite stay green.\n\n* perf(encode): step the fast dict scan by upstream stepSize\n\nThe fast dict-attach kernels are 2-cursor ports of upstream\n`ZSTD_compressBlock_fast_dictMatchState_generic`, but they received the\nshared level step_size, which carries a `+1` only the 4-cursor no-dict\npipeline needs (its `step_size >= 2` invariant). So the dict scan advanced\none position MORE than upstream (`stepSize = targetLength + !targetLength`),\nskipping candidate positions and coarsening the parse; the extra skip\ninflated the dict frame on the negative-level band.\n\nUndo the `+1` inside both dict kernels (flat + borrowed) so the scan steps\nexactly like the reference. small-4k-log-lines compress-dict is now\nbyte-exact with the reference across the ENTIRE fast band (level_-7..2):\nthe level_-2 outsider 50 B -> 44 B, and every fast level matches. No\nregression elsewhere (small-10k-random keeps its pre-existing 10254 B; that\nis a separate match-finding gap, not stepping). The borrowed kernel's\nmain==dict hash-log equality assert is also dropped: it already hashes main\nand dict lookups with their own independent hash-logs, so the CDict-geometry\ndict table (narrower than the source-window main table) is handled correctly.\n\nFull lib + ffi suite green.\n\n* perf(encode): add the no-match step accelerator to the fast dict scan\n\nThe fast dict-attach kernels stepped by a constant `step_size`, so an\nincompressible stretch probed every position at that fixed cadence. Upstream\n`ZSTD_compressBlock_fast_dictMatchState_generic` ramps the step: `step`/\n`nextStep` reset once per found match, and the inner search loop bumps `step`\nby 1 every `kStepIncr` bytes of fruitless scanning, so no-match regions skip\nahead instead of probing linearly.\n\nPort it to both dict kernels (flat + borrowed). The dict path's `kStepIncr`\nis `1 << kSearchStrength` (256) — DOUBLE the no-dict / extDict value\n(`1 << (kSearchStrength - 1)` = 128), so the dict scan ramps half as often;\nadd a dedicated `K_STEP_INCR_DICT` for it.\n\nByte-neutral on the match-dense fixtures (small-4k-log-lines stays byte-exact\nwith the reference; small-10k-random unchanged): the ramp only fires on long\nno-match runs, where it saves work. Full lib + ffi suite green.\n\n* perf(row): port the lazy parse and row search from upstream lazy_generic\n\nThe Row lazy body is now ZSTD_compressBlock_lazy_generic (row-hash search)\nand ZSTD_RowFindBestMatch: forward-only match lengths gated by the 4-byte\ncompare at the current best length, newest-first row walk stopping at the\nwindow floor, repcode 1 probed one byte ahead, gain-weighted depth-1/depth-2\nlookahead with every position searched once, catch-up on the chosen match\nonly, immediate-repcode loop after each stored sequence, kSearchStrength 8\nmiss acceleration with lazy skipping past step 8, and the 16-byte ilimit\ntail. Replaces the length-heuristic parse that back-extended every\ncandidate, probed three reps per position and re-searched the depth-2\nposition: on z000033 L11 it executed 2.1x upstream's instructions per\ncompress (perf stat, i9) for a 5% smaller output.\n\n* perf(row): add the upstream row hash cache to the lazy parse\n\nMirrors ZSTD_row_fillHashCache / ZSTD_row_nextCachedHash: the parse hashes\nROW_HASH_CACHE_SIZE (8) positions ahead and prefetches each row's heads,\ntags and positions lines while consuming cached hashes for the gap indexing\nand the search, refilling at block start, at the 384-gap skip and when a\nmatch ends lazy skipping. The gap indexing moves into the parse monolith.\nRow prefetch also covers the heads byte and the second positions line\n(upstream ZSTD_row_prefetch).\n\n* perf(row): upstream-shape row hash and hoisted scan context in the lazy parse\n\nRow hash is now ZSTD_hashPtrSalted's shape (key shifted to the top of the\n64-bit read, one prime multiply, precomputed shift per mls) instead of the\nkernel-dispatched crc32 mix, applied identically in the probe, the hash\ncache and the dictionary fill. The lazy parse hoists the live history to a\nraw base + length once per block (RowScan) so hashing, rep probes and the\ncandidate walk read no Option branch or Vec header per access, and the row\ntable reads in the search use the ensure_tables bound instead of per-access\nchecks.\n\n* bench(compare_ffi): compress the FFI arm one-shot with ZSTD_compress2\n\nThe FFI arm used ZSTD_compressStream2, which buffers blockSizeMax (128 KB)\nand runs ZSTD_compress_frameChunk per 128 KB even when handed the whole\ninput; ZSTD_optimalBlockSize never splits a remainder below 128 KB, so the\nreference emitted at most two blocks per 128 KB (14 on the 1 MB corpus)\nwhile the one-shot ZSTD_compress2 path that compress_independent_frame\nmirrors keeps pre-splitting (~90 blocks, ~5% smaller output, 7x the\nentropy work). Pairing our one-shot encoder with streaming C misstated\nboth the time and the size comparison.\n\n* fix(encode): pre-split at upstream's default splitLevels tier per strategy\n\nZSTD_optimalBlockSize subtracts 2 only from an EXPLICIT blockSplitterLevel;\nthe default path passes splitLevels[strategy] = {0,0,1,2,2,3,3,4,4,4} to\nZSTD_splitBlock as is. The table here had the -2 applied to the default,\nso dfast / greedy / lazy ran the from-borders heuristic (14 blocks on the\n1 MB corpus vs upstream's 94) and lazy2 / btlazy2 / btopt sampled two\ntiers coarser: z000033 L3 523341 vs 498911 and L6 508691 vs 484957 bytes\nagainst ZSTD_compress2. Tiers now match upstream: on the periodic 512 KB\nlog stream every level is byte-identical to the reference (L7 189 B,\nL8 / L15 528 B), asserted by the new ffi-bench parity test; the old lib\ntest that pinned the lazy2 tier below 2x the lazy frame encoded a\nguarantee upstream does not give and is reduced to the round-trip.\n\n* fix(row): upstream-exact row hash, salt, slot-0 rule and cross-block nextToUpdate\n\nRow assignment now matches a fresh upstream context bit for bit: the\nZSTD_hash4/5/6 multipliers per key width, the fresh-CCtx hash salt\n(ZSTD_advanceHashSalt over zero entropy) XORed before the reduction, slot\n0 of a row never written and never walked (ZSTD_row_nextIndex / the\nmatchPos == 0 skip, so a row holds row_entries - 1 candidates and evicts\nin upstream's order), and the lazy parse carries nextToUpdate across\nblocks so a block's unindexed tail is hashed by the next block with the\nfull 8-byte key instead of a per-block 3-byte backfill and a 4-byte-key\ntail fill. With the pre-split tiers already aligned these were the\nremaining sources of the per-block byte deltas against ZSTD_compress2.\n\n* fix(row): carry the lazy parse repcodes across blocks as upstream does\n\nZSTD_compressBlock_lazy_generic enters a block with rep[0..2] exactly as\nthe previous block's offset_1 / offset_2 left them (a window-disabled rep\nit never used is handed back). The parse loaded them from the block\nencoder's offset_hist instead, which diverges from upstream's parse-side\nreps whenever a searched offset equal to a rep is encoded as a repcode\n(no rotation) where upstream stores it raw and rotates; the first search\nof the next block then probed a different rep. Sequence streams on the\n1 MB corpus now agree with the reference through the whole first block;\nthis removes the block-entry divergence.\n\n* bench(sequences): capture our sequences with the pre-splitter off\n\nZSTD_generateSequences runs ZSTD_compress2 in sequence-collecting mode,\nwhere every block is written raw (cSize = blockSize + 3), so the\nsavings >= 3 gate of ZSTD_optimalBlockSize never opens and upstream's\nstream is cut into full 128 KB blocks only. The capture pre-split as in\nproduction, so on inputs over 128 KB every block boundary after the first\nproduced RUST_ONLY / FFI_ONLY rows and a different block-entry parse state\non our side. A bench_internals diagnostic switch on FrameCompressor cuts\nfull blocks only for the capture; production block sizing is unchanged.\n\n* bench(sequences): print the absolute input position of every diverging row\n\nRows were only numbered by sequence index; the byte offset a divergence\nsits at is what a prefix-based reproduction needs.\n\n* fix(row): resume the lazy gap fill from the carried nextToUpdate\n\nThe carried cursor was clamped to the 3 bytes before the block start, so\nthe previous block's last 16 bytes (never searched, hence never indexed\nby that block) were skipped instead of indexed by the next block's first\nsearch; every cross-block match into that tail was lost (first\ndivergence on the corpus: pos 131405, offset 347 into the block-0 tail).\nThe cursor is now bounded to the live history only; the greedy and\nskip parses hand the lazy parse their own end cursor so nothing is\nindexed twice.\n\n* fix(row): size the row table by the level's hashLog as upstream does\n\nset_hash_bits capped the row table at 20 bits, one below upstream's\nsource-size-adjusted hashLog (21) on the 1 MB corpus. A narrower table\nfolds twice as many keys per row, so rows evict older candidates sooner\nand every search past the first block sees a different candidate set\n(rows near capacity lost the match upstream still holds). With the table\nat upstream's width the lazy sequence stream on the corpus prefix is\nidentical to ZSTD_generateSequences at L11 (27360 / 27360).\n\n* feat(row): hash-chain finder, dictionary plan and greedy on the shared lazy parse\n\nThe greedy / lazy band now runs ONE parse (the upstream lazy_generic port)\nover two match-finders: rows above a 2^14 window, the ZSTD_HcFindBestMatch\nhash chain at or below it (ZSTD_resolveRowMatchFinderMode), chosen inside\nthe Row backend instead of rerouting small windows to the old HashChain\nlazy parser. Greedy (L5) is the same body at depth 0; the separate greedy\nkernels are gone.\n\nDictionary frames follow ZSTD_resetCCtx_usingCDict: the CDict's cParams\n(get_cdict_cparams: tier row by dictSize + 498, createCDict adjust) give\nthe frame its strategy, widths, search depth and finder; sources up to the\nstrategy cutoff attach (separate dictionary tables with the CDict geometry,\nunsalted, probed as dictMatchState by both finders with the shared\nattempt budget), larger ones copy (dictionary indexed into the live tables,\nsalt 0, nextToUpdate = dictSize - 8, upstream's extDict rules: first prefix\nbyte skipped, per-probe rep window + index overlap check, catch-up bounded\nby the segment start, head-gated dictionary candidates, lazy skipping one\nstretch later). The hash-chain ilimit is iend - 8.\n\nSequence streams now match ZSTD_generateSequences: z000033 L5/6/8/11/12\nwithout dictionary, L5/6/8 with a raw dictionary (copy mode), and the\n16 KB / 4 KB / 1 KB fixtures with and without dictionary (attach mode).\nKnown gaps: a dictionary whose cParams leave the greedy/lazy band (L11 with\na 4 KB dictionary resolves to btlazy2) keeps the plain level params; one\nL11 divergence remains on the 16 KB fixture with a dictionary.\n\n* perf(row): hoist the frame's search bounds into the scan view\n\nThe dictionary-plan checks (prefix start, distance bound, attach flag,\nsalt) were read back through the matcher on every search and rep probe,\nwhere the optimizer cannot hoist them across the table writes; they are\nframe constants, so RowScan carries them and the hot loops read locals.\nByte-identical (sequence parity re-checked with and without dictionary).\n\n* perf(row): fold the plain-frame rep check into the block clamp\n\nWithout a dictionary the rep probe only needs the non-zero check: the\nblock-start clamp keeps carried reps in-window and searched offsets are\nin-window by construction (upstream noDict shape). The chain-vs-row\nchoice moves into the scan view with the other frame constants.\nByte-identical (sequence parity re-checked with and without dictionary).\n\n* chore: satisfy clippy 1.98 lints\n\nneedless_late_init, chunks_exact_to_as_chunks, manual_clear and\nbyte_char_slices, all raised by the latest stable toolchain the CI\nfollows.\n\n* chore: satisfy clippy 1.98 lints\n\nneedless_late_init, chunks_exact_to_as_chunks, manual_clear and\nbyte_char_slices, all raised by the latest stable toolchain the CI\nfollows.\n\n* test(encode): pin the fast dict fill to one stride pass over the whole dictionary\n\nA dictionary primed in slices must build the same table as a single\npass (the stride-3 phase carries across the slice seam) and take the\nCDict geometry of the whole dictionary, not of the first slice. Both\ntests fail before the fix.\n\n* fix(encode): keep the fast dict fill phase and geometry across dictionary slices\n\nThe fill resumes at the first unprocessed stride position instead of one\npast the slice's hashable end, so a group cut by the slice seam is\ncompleted by the next slice as upstream's single pass would; the dict\ntable is sized from the whole dictionary's length (the CDict geometry)\nrather than the slice that allocates it, and a retained table of another\nwidth is rebuilt.\n\n* feat(row): btlazy2 on the lazy backend with the upstream binary tree and dictionary window rules\n\nbtlazy2 (strategy 6, levels 13-15 at tier 0, 9-10 at the 16 KiB tier)\nran the optimal parser's BT collector under an ad-hoc greedy loop, which\nsurfaced 3-byte matches and diverged from upstream. It now runs the same\nlazy2 parse as the other lazy levels over a third finder, the port of\nZSTD_BtFindBestMatch: ZSTD_updateDUBT links the gap unsorted,\nZSTD_DUBT_findBestMatch sorts a bucket's nodes in one batch and walks the\ntree with upstream's offset-cost margin, nextToUpdate jumps past the\nlongest match seen, and ZSTD_DUBT_findBetterDictMatch walks an attached\ndictionary's sorted tree (ZSTD_updateTree / ZSTD_insertBt1 over the whole\ndictionary, in one pass as upstream). A copied dictionary's tree is built\ninto the live tables. The old HashChain-backend btlazy2 body is deleted.\n\nThe window follows upstream per block: loadedDictEnd / lowLimit /\ndictLimit with ZSTD_checkDictValidity and ZSTD_window_enforceMaxDist, so a\ndictionary is matched without distance bound only while it is valid, an\nattached one stops being probed and a copied one falls out of reach as\nthe window slides past it, and the search floors of all three finders\n(ZSTD_getLowestMatchIndex, the distance-only sort floor of\nZSTD_insertDUBT1) read the same state. A copied dictionary also leaves\nnextToUpdate at the dictionary end (ZSTD_loadDictionaryContent), so its\nlast 8 positions are not indexed.\n\nThe dictionary plan now covers CDict strategy 6. Sequence parity with\nZSTD_generateSequences: z000033 L13-15 100% (was divergent), L5-12 and\nthe dictionary cases unchanged at 100%, btlazy2 with a dictionary in both\nattach and copy mode 100%.\n\n* test(row): pin absolute dictionary chain indices, borrowed-frame floors and CDict geometry under overrides\n\nThree review findings, each failing before its fix: a copied hash-chain\ndictionary re-indexed on a reused compressor (snapshot miss) must match\na cold compressor's frame; the same input compressed twice on one\ncompressor through the borrowed one-shot path must give identical\ndecodable frames; a dictionary frame under search_log / min_match\noverrides must still find its dictionary matches.\n\n* fix(row): absolute dictionary chain indices, borrowed-frame floor, CDict geometry under overrides, uninitialised FFI bench output\n\n- a copied hash-chain dictionary is indexed at absolute positions (the\n  slot and the stored index were relative, so a reused matcher whose\n  floor sat past its previous frames never matched the dictionary again)\n- the borrowed one-shot path keeps the coordinate floor instead of\n  zeroing it; reset advances the floor past the borrowed extent, so a\n  previous frame's chain / tree entries can neither resurface as\n  offset-0 self-matches nor point past the searched position\n- a dictionary frame keeps the CDict's finder geometry under public\n  parameter overrides (upstream \"cdict overrides\"); only windowLog is\n  the caller's\n- the FFI bench arm reserves compressBound uninitialised instead of\n  zero-filling it per timed iteration\n\n* chore: gate the level-form block-size probe to tests and bench_internals\n\nThe frame loop resolves the pre-split tier itself; CI's default-feature\nclippy flagged the level-form helper as unused.\n\n* perf(row): one lazy monolith per finder; hoist the FFI bench output buffer\n\nThe finder (rows / chain / tree) becomes a const parameter of the lazy\nmonolith: each tier now carries one parse per finder (rows per row_log,\nthe chain and the tree once) instead of all three finders inlined into\nevery row_log parse, and the per-search finder branch folds away. The\nsimd128 wasm payload drops 1,078,189 -> 1,018,647 bytes, back under the\n1 MiB CI budget.\n\nThe FFI bench arm compresses into one buffer reserved once per sample\n(the C dst contract): no per-iteration alloc / memset, and no heap churn\nthat perturbed the Rust arm's small-frame samples.\n\n* fix(encode): CDict strategy for optimal dictionaries, effective pre-split tier, Fast CDict hashLog, lazy-backend table sizing\n\n- a dictionary whose CDict cParams resolve to an optimal strategy makes\n  the frame run it (upstream ZSTD_resetCCtx_usingCDict), so a small\n  dictionary on a btlazy2 level is indexed by the finder that searches\n  it instead of rows the tree never reads\n- the pre-split tier (upstream splitLevels) follows the effective\n  strategy: a strategy override, a source-size promotion or the CDict's\n  strategy, recorded in CompressState next to the strategy tag\n- the Fast dictionary table takes the CDict's hashLog, not the\n  source-capped main width\n- CompressionLevel::Best is level 13 everywhere (bridge, from_level,\n  docs); the lazy backend allocates only the active finder's tables and\n  the workspace estimate counts the chain / tree hash table; the wasm\n  size budget is 1.25 MiB\n\n* fix(encode): take the CDict strategy whatever backend the plain level resolved to\n\nA dictionary frame runs the CDict's cParams unconditionally upstream\n(ZSTD_resetCCtx_usingCDict); resolving the plan only when the plain\nlevel's source-size tier already sat on the lazy backend left a frame on\nthe wrong search algorithm whenever the two tiers disagreed: L4 on a\n1 MiB source (dfast) with a 4 KiB dictionary (greedy CDict) ran dfast,\nL13 on a 4 KiB source (btopt) with a 300 KiB dictionary (btlazy2) ran\nbtopt. z000033 + 4 KiB raw dictionary at L4: 15 % -> 100 % sequence\nparity; L1 / L2 11 % / 8 % -> 57 % / 55 % (the rest is the fast\ndictionary paths).\n\nCarries the regression test (both tier mismatches) and reshapes the\ndict_builder round-trip test onto a payload the dictionary genuinely\ncovers: on its previous payload upstream too compresses smaller without\nthe dictionary.\n\n* perf(dfast): tag the attached dictionary slots as upstream's short cache\n\nThe dfast dictMatchState tables now pack the hash tag next to the index\n(ZSTD_SHORT_CACHE_TAG_BITS, ZSTD_writeTaggedIndex / comparePackedTags),\nso a colliding slot is rejected on the tag without loading the\ndictionary bytes. With the CDict-sized tables a frame now runs (12 / 11\nbits for a 1.3 KiB dictionary) most slots are occupied and every\nuntagged collision on incompressible input was a cache miss into the\ndictionary: compress-dict/level_3_dfast/small-10k-random had gone 73.6\n-> 117 us once the frame took the CDict geometry. Dictionaries past the\n24-bit index field take the copy path. Sequence streams unchanged.\n\n* fix(encode): CDict geometry for attached dfast tables, effective strategy on planned dictionary frames, btlazy2 out of the optimal estimate\n\n- the dfast dictionary tables take the CDict's hashLog / chainLog and\n  the probes hash at those widths (upstream dictCParams), instead of the\n  source-capped live widths that indexed a large dictionary into a tiny\n  table on a small source\n- the frame state (literal gates, pre-split tier) records the CDict's\n  strategy when a lazy-band dictionary plan makes the matcher ignore a\n  public strategy override\n- estimated_bt_strategy_extra_bytes charges the optimal-parser scratch\n  to strategies 7..=9 only: btlazy2's tree lives in the lazy backend\n\nCarries the regression tests for the first two.\n\n* fix(encode): dictionary frames follow the CDict on every backend\n\n- a public strategy / finder override is ignored on any dictionary frame\n  (upstream \"cdict overrides\": only windowLog is the caller's), not only\n  on the lazy band; the frame state records the CDict strategy for every\n  backend family\n- the CDict cParams tier is keyed by the serialized dictionary size\n  (ZSTD_createCDict dictSize), retained as Dictionary::serialized_len;\n  the matcher hint carries both sizes (DictionarySizes) so the content\n  length still drives the dictionary tables and attach cutoffs\n- the hash-chain / binary-tree widths of a dictionary frame are the\n  CDict's as resolved (verbatim in copy mode, source-adjusted in attach\n  mode, where the dms owns the dict-sized tables); the dict-tier resizing\n  block that overwrote them with 8 MiB tables on a 4 KiB attached source\n  is removed together with cdict_table_logs\n- the Fast / Dfast dictionary-table geometry setters drop a resident\n  table of another geometry so a level change with unchanged live widths\n  re-primes instead of probing the previous level's table\n\nRegression tests: dictionary_frame_keeps_a_fast_cdict_strategy_under_a_\nstrategy_override, driver_dictionary_frame_ignores_a_strategy_override_on_\na_fast_cdict, dictionary_cdict_tier_follows_the_serialized_dictionary_size,\ndriver_dfast_dictionary_tables_follow_the_cdict_geometry_across_levels\n(each failed before its fix); driver_fast_dictionary_table_follows_the_\ncdict_geometry_across_levels pins the Fast contract.\n\nPart of #323\nPart of #178\n\n* fix(encode): pre-split full blocks on the streaming encoder\n\nThe streaming encoder emitted every full 128 KiB buffer as one block; the\nframe compressor's reader path (and upstream ZSTD_compress_frameChunk,\nstreaming included) cuts a full block at the pre-splitter's boundary once\nthe frame has saved enough, carrying the suffix into the next block. The\nstreamed frame, which is what the CLI writes, was 5.8 % larger than the\none-shot frame on decodecorpus z000033 at level 6 (512,625 vs 484,728\nbytes; the libzstd CLI writes 491,854).\n\n- cut a full pending buffer with optimal_block_size_with at the frame's\n  effective pre-split tier, tracking upstream's `savings` (consumed minus\n  produced, block headers included); a full final buffer is cut the same\n  way and its suffix becomes the last block\n- a drain failure while emitting a cut prefix restores the whole pending\n  buffer (prefix + suffix), so no input is lost\n- resolve the streaming frame's effective strategy / pre-split tier through\n  the same size- and dictionary-adaptive path as the frame compressor\n  (shared frame_compressor::{sync_effective_strategy, resolve_frame_params});\n  the strategy override keeps its lazy depth for the tier\n\nRegression test streaming_encoder_pre_splits_full_blocks_like_the_frame_\ncompressor (block stream identical to the reader path at L6 / L16) failed\nbefore the fix.\n\nPart of #178\n\n* fix(encode): keep dictionary-frame gates on the CDict parameters\n\n- set_parameters resolves the dictionary's CDict tier only when the frame\n  will prime it (not for Uncompressed, whose level has no numeric value:\n  it panicked with a dictionary attached)\n- the raw-literals gate (ZSTD_literalsCompressionIsDisabled) drops a\n  target_length override on a dictionary frame, where the matcher runs the\n  CDict's targetLength; the streaming encoder now resolves the same gate\n  at frame start from its own target_length override\n- README / BENCHMARKS: the Best preset is level 13, as numeric_level maps\n  it (the docs said 11)\n\nRegression tests: set_parameters_uncompressed_with_a_dictionary_attached_\ndoes_not_resolve_a_cdict, dictionary_frame_literal_gate_ignores_a_target_\nlength_override (both failed before the fix),\nstreaming_encoder_literal_gate_follows_the_effective_target_length.\n\nPart of #178\n\n* perf(encode): keep the coarse block pre-split tiers\n\nThe effective-strategy pre-split tier introduced with the lazy parse used\nupstream's default `splitLevels` table as is (greedy/lazy rate 11,\nlazy2/btlazy2 rate 5, optimal band rate 1). On periodic input that\nover-splits every block, exactly as upstream does: the 100 MiB repeated\nlog stream went from 8,944 to 140,625 bytes at L8-L10 (upstream's size)\nand 3.6x slower, low-entropy 1 MiB from 148 to 155 bytes; on real data\nthe finer tiers bought 170 bytes of 483 KiB at L8-L12 while costing time\n(btopt rate-1 scan: L17 low-entropy 1.75x). The drop-in contract is ratio\n<= upstream, not upstream's block boundaries, so the tiers two steps\ncoarser (from-borders up to lazy, rate 43 for lazy2/btlazy2, rate 11 for\nthe optimal band) stay; `pre_split_for` documents the deviation.\n\nPart of #178\n\n* test(levels): pin resolved level params to ZSTD_getCParams over a size grid\n\nEvery (level -7..22, source size 1 KiB..100 MiB) cell: strategy / backend,\nwindow, hash and chain logs, search depth, targetLength and minMatch of the\nparams the encoder runs equal upstream's cParams for that size tier.\n\n* perf(encode): upstream pre-split tiers up to lazy, coarse above\n\nMeasured per strategy on the i9 (decodecorpus vs the periodic 100 MiB\nstream): the borders tier on dfast/greedy/lazy compresses mixed data\n4.6-4.9 % WORSE than upstream (this was the long-standing greedy L5 ratio\noffender), so those follow upstream's `splitLevels` (dfast rate 43,\ngreedy/lazy rate 11); the lazy2/btlazy2 rate-5 and optimal rate-1 tiers\nstay two steps coarser (rate 43 / 11): on the periodic stream they match\nupstream's over-split (140,625 vs 9,742 bytes, 3.6x the time) and on\nmixed data buy under 0.1 %.\n\nPart of #178\n\n* perf(encode): keep the block sizer out of the frame loop\n\noptimal_block_size_with inlined into the per-block frame loop shifted the\ncode layout around run_fast_kernel_block and cost the Fast levels 17-24 %\non 1 MiB+ inputs on x86 (i9: the kernel's instruction stream is\nbyte-identical to the previous shape, the caller grew). One call per block\nis noise; mark it inline(never) so the loop stays compact.\n\nPart of #178\n\n* fix(encode): dictionary attach caches and geometry hold across resets\n\n- the serialized dictionary size moves off the public Dictionary struct\n  (its all-pub fields are externally constructible; a new field broke that)\n  into EncoderDictionary: exact on the from_bytes / set_dictionary_from_bytes\n  / C ABI paths, content-length fallback on from_dictionary (documented)\n- the Fast / Dfast dictionary-geometry setters also drop a resident table\n  when the next frame carries no dictionary, so a reused matcher cannot\n  re-borrow dict tables under a frame whose header declares none\n- a dictionary whose content exceeds the tagged attach tables' 2^24\n  position range resolves the COPY geometry (CDict verbatim table logs)\n  instead of source-capped attach widths it would then be copied into\n\nRegression tests: driver_no_dictionary_reset_drops_the_attached_tables and\noversized_attach_dictionary_resolves_the_copy_geometry (both failed before\ntheir fixes).\n\nPart of #178\n\n* fix(encode): raise the valid-data floor when the lazy window evicts\n\nThe Row backend's eviction (add_data / trim_to_window) advanced\nhistory_abs_start without raising low_limit, so the distance-only window\nfloor (pos - search_window) could trail the eviction by up to a block and\nthe DUBT walks dereferenced evicted positions that were still inside the\nadvertised window: an out-of-bounds read, crashing (index underflow with\ndebug assertions, SIGSEGV without) on any input a block past the window at\nthe btlazy2 levels — the streaming CLI at L13-L15 on a repetitive stream\nlarger than the window, and the one-shot reader path equally. Raise\nlow_limit (and prefix_low) past everything eviction drops, exactly\nupstream's window.lowLimit semantics; the DUBT sort keeps a debug\nassertion that a sorted node's bytes are resident.\n\nRegression test streaming_periodic_btlazy2_roundtrips (6 MiB periodic\nstream, past the L15 window) failed before the fix with the underflow.\n\nPart of #178\n\n* fix(encode): one size for the streaming reset, no attach reborrow in copy mode\n\n- the streaming encoder re-forwards the authoritative size\n  (pledge.or(advisory hint)) to the matcher right before the reset, so a\n  pledge followed by a different advisory hint (or vice versa) cannot leave\n  the matcher and the frame gates on different size tiers\n- the driver drops a resident Dfast attach table when the next dictionary\n  frame is copy-mode: the width-change invalidation did not cover an\n  unhinted attach frame followed by a large hinted one whose live widths\n  coincide at the level's full widths\n- README backend map updated to the Row-backend lazy band (greedy..btlazy2\n  on the shared lazy_generic parse; rows / chain / lazily-sorted tree)\n\nRegression tests: streaming_encoder_matcher_and_gates_resolve_from_one_size\nand driver_dfast_attach_table_is_dropped_when_the_next_frame_copies (both\nfailed before their fixes).\n\nPart of #178\n\n* fix(encode): release inactive lazy tables, rebase the borrowed cursor\n\n- a rows-level reset on a reused compressor coming back from a btlazy2\n  level (same Row storage, no backend swap) now releases the chain / tree\n  tables the rows finder never reads (tens of MiB retained otherwise)\n- registering a borrowed window applies the same u32 headroom guard as\n  add_data: the borrowed reuse path advances the coordinate floor per frame\n  without committing, so after ~4 GiB of cumulative reused frames every\n  stored position wrapped u32 and matching silently degraded; rebase the\n  origin before the frame's positions are stored\n- a synthesized btlazy2 override already hashes at minMatch 5 (ROW_L5\n  carries ROW_MIN_MATCH_LEN); documented at the synthesis site\n\nRegression tests: driver_rows_reset_releases_the_tree_tables and\nborrowed_window_rebases_before_the_u32_cursor_wraps (both failed before\ntheir fixes).\n\nPart of #178\n\n* fix(encode): 32-bit-safe u32 headroom check on the borrowed window\n\nThe borrowed-window rebase guard summed history_abs_start + buffer.len()\nin usize, which itself overflows on a 32-bit target exactly where the\nguard must fire (caught by the i686 CI run of\nborrowed_window_rebases_before_the_u32_cursor_wraps). Do the comparison\nin u64.\n\n* fix(encode): recompute the raw-literals gate per frame\n\nset_parameters computed literal_compression_disabled once, so attaching\nor clearing a dictionary afterwards left it stale: a dictionary frame\nemitted raw literals for a target_length override the matcher had dropped\n(the CDict's targetLength applies there), and the inverse ordering lost\nthe override on the plain frame. Persist the target_length override and\nrecompute the gate in prepare_frame next to the strategy sync.\n\nRegression test literal_gate_follows_dictionary_attach_and_clear (both\norderings) failed before the fix.\n\n* fix(levels): keep the full search-log budget for the chain and tree\n\nAn explicit search_log override stored 1 << min(searchLog, rowLog) as the\ncompare budget, silently capping e.g. search_log(7) on btlazy2 to 64\ncompares. Upstream's nbAttempts is the full 1 << searchLog for the chain\nand tree walks; only the row probe bounds its budget by the row size, and\nit does so at the search site. Store the full depth.\n\nRegression test search_log_override_keeps_the_full_depth_for_chain_and_tree\nfailed before the fix.",
          "timestamp": "2026-09-02T01:04:10+03:00",
          "tree_id": "f25764e682a8ba403452ebc5a8eb0fe59b7f2a4e",
          "url": "https://github.com/structured-world/structured-zstd/commit/ac08d193734ba76ae2b1c20e53efa0f0846d43fe"
        },
        "date": 1788303239086,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.086,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.085,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 210.638,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 204.197,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.622,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.373,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.909,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.994,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.946,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 2.043,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.028,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.173,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 11.313,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.724,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.137,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.189,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.553,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.153,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.725,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.251,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "3b37ed6f16210408eb12dd4158234f652bd9e159",
          "message": "feat(encoder)!: gate encoder SIMD on the kernel features, drop the dead hash mix (#473)\n\n* docs(readme): restructure for scannability, refresh discovery metadata\n\n- README: lead with a Highlights list; move the SIMD kernel-feature prose\n  out of Quick start into a Feature flags table (AVX-512 rationale folded\n  into a details block); break the storage-extensions and performance\n  paragraphs into bullets; drop the finished encoder-rewrite notice;\n  embedded doctest code blocks unchanged\n- Cargo.toml: description now covers the full encoder, streaming, no_std\n  and WebAssembly; keywords swap pure-rust for no-std; categories add\n  no-std, wasm and encoding; homepage points at the public benchmark\n  dashboard\n\nCloses #472\n\n* docs(readme): correct level range, npm dependency claim, resume window contract\n\n- state the encoder level range as -131072..=22 (the full negative fast\n  range is supported, matching the C API minimum)\n- the npm package carries a runtime dependency (wasm-feature-detect); the\n  accurate claim is no native addons / no postinstall scripts\n- resumable-decoding bullet now notes that ResumeState does not carry the\n  match window and the resuming call supplies it via ResumeInput::window_prime\n\n* docs(readme): scope SIMD flags to the decoder, note LDM needs hash\n\n- The `hash` feature also gates long-distance matching (the LDM match\n  finder hashes each window with XXH64), not only content checksums.\n  Without it the builder still accepts `enable_long_distance_matching`\n  and the frame stays valid, but no long-distance matches are produced;\n  say so in the feature table and beside the LDM example.\n- The `kernel_*` flags gate the decoder kernels only. The encoder\n  fastpath picks its tier independently: at runtime under `std`, from\n  the target's `target_feature` set under `no_std`. So dropping the\n  flags while keeping `std`, or building `no_std` for a target tuned\n  with `+avx2,+bmi2,+sse4.2`, still runs explicit encoder SIMD.\n  `kernel_simd128` is the one flag that also gates an encoder path\n  (wasm), which the table now says.\n\nPart of #472\n\n* refactor(encoder): drop the dead per-tier hash mix\n\n`hash_mix_u64` had five per-tier implementations and no production caller:\nthe only references in the workspace were two unit tests. Match hashing\nruns through `MatchTable::hash_value_with_mls` / `hash_value8_with_mls`,\nwhich mirror upstream `ZSTD_hash4/5/6/8`. The module-level\n`allow(dead_code)` (added as scaffolding, with a note to drop it once\nconsumers came online) is why this never surfaced.\n\nThree comments described the dead code inaccurately:\n\n- sse42 called it a mirror of an upstream \"CRC-folded hash mix\". Upstream\n  has no such thing: `crc32` does not appear anywhere in `lib/compress/`\n  on 1.5.7 or on dev, and `ZSTD_hash4/5/6/8` are plain multiplications.\n- neon claimed its mix matched the x86 kernels. It cannot: `__crc32d` is\n  CRC-32 (poly 0x04C11DB7) while `_mm_crc32_u64` is CRC-32C (Castagnoli).\n- simd128 claimed bit-identity with every other tier, while the scalar\n  mix is a bare multiply.\n\nRemoving it drops the CRC requirement that only that function had, so each\ntier now declares what it actually executes:\n\n- sse42 -> SSE2 (its three live intrinsics are all SSE2)\n- avx2_bmi2 -> avx2,bmi2 (was avx2,bmi2,sse4.2)\n- neon -> the aarch64 NEON baseline, no optional `crc` extension\n\nThe runtime probes shrink accordingly, and the NEON tier stops falling\nback to scalar on aarch64 parts without `crc`.\n\nAlso removed alongside: `dispatch_count_match_from_indices` and the\nper-tier `KERNEL_TAG` constants (both unreferenced workspace-wide), and\nthe module-level `allow(dead_code)` from the fastpath, scalar and simd128\nmodules so this cannot accumulate silently again. Two items that are\ngenuinely unreachable per target keep a scoped `allow` with the reason:\nthe scalar BT probe on aarch64 (its walker is compiled out there) and the\nsimd128 BT probe (the BT walker has no wasm wrapper yet).\n\nRenamed `Row::hash_kernel` to `cpl_kernel`: it feeds\n`common_prefix_len_with_kernel`, not the mix its name and doc claimed.\n\nCompressed output is unchanged; dead code cannot affect it. Verified on\naarch64, x86_64, thumbv7em-none-eabihf and wasm32 (+simd128).\n\nPart of #474\n\n* perf(encoder): wire the wasm simd128 tier into the BT walker\n\nThe simd128 tier had a `count_match_from_indices` that nothing called: the\nBT walker only had neon / sse2 / avx2 / scalar wrappers, so a wasm build\nran the BT walk through the scalar probe. Add the missing simd128 arm to\nall three dispatchers (`bt_insert_step_no_rebase`,\n`bt_insert_and_collect_matches`, `collect_optimal_candidates_initialized`)\nso the wasm tier reaches its own vector probe.\n\nMeasured, interleaved A/B on the bench host (one process loading both\npayloads, alternating samples, 25 per arm, load < 0.3), compress at the BT\nlevels:\n\n  decodecorpus-z000033  L19 +0.22%   L22 +0.06%\n  small-4k-log-lines    L19 -0.23%   L22 -0.03%\n  low-entropy-1m        L19 +0.01%   L22 +0.15%\n  large-log-stream-4m   L19 -0.14%   L22 -0.26%\n\nEncoded bytes identical on every fixture and level, so the wiring is\ncorrect; the throughput effect is nil, including on the long-match\nfixtures where a vector prefix compare was expected to pay. The payload\ngrows 1574 bytes (1023318 -> 1024892, budget 1.25 MiB).\n\nPart of #474\n\n* docs(encoder): record why the BT vector probe barely fires\n\nInstrumented the BT-path prefix compare over decodecorpus-z000033 to\nexplain the flat A/B on the wasm tier wiring. At both L19 and L22 the\nleading 8-byte word probe resolves 87.45% of calls; only 12.55% reach the\nvector loop, and those average 1.6 (L19) to 4.6 (L22) 16-byte iterations.\nA short buffer (`max < 16`) accounts for 0.05%, so the early exit is\ndriven by the data, not by the buffer bound.\n\nThat is the whole explanation for the flat measurement: the vector cannot\nmatter much when 87% of compares end before it starts. Noted at the probe\nso the next person does not re-run the same experiment, along with the\nimplication that the lever on this path is cutting the number of candidate\ncompares, not widening the survivors.\n\nAlso drops two stale references to the removed hash mix from the module\nheader and the wasm dispatch comment.\n\nPart of #474\n\n* feat(encoder): gate encoder SIMD on the kernel features, add an SSE2 tier\n\nThe `kernel_*` features gated only the decoder: `fastpath::select_kernel`\npicked SSE4.2 / AVX2+BMI2 / NEON on its own, so `--no-default-features`\nstill executed encoder SIMD. Every encoder tier (module, enum variant,\ndispatch arm, tag-mask macro, per-tier wrapper) now sits behind the same\nfeature as its decoder counterpart, and a build with no kernel features\nemits no explicit encoder SIMD: `nm` counts 99 avx2/sse42 symbols with the\ndefault features and 0 without.\n\nThe x86 tier turned out not to be a single tier. Its prefix-compare kernel\nis plain SSE2, but the optimal parser's price set calls\n`priceset_range_nonabort_sse41`, whose `_mm_min_epu32` is SSE4.1. Rather\nthan keep probing the whole tier on SSE4.2, split it:\n\n- `Sse42`: unchanged, SSE4.1 price set.\n- `Sse2`: new. Same 128-bit prefix-compare kernel plus an SSE2 price set,\n  for x86 CPUs without SSE4.2. These previously fell all the way back to\n  the scalar kernel despite having usable SSE2.\n\nSSE2 has no unsigned 32-bit compare, so the new improve-mask biases both\noperands by 0x8000_0000 and uses the signed compare, which yields the same\nmask in one compare instead of min/eq/andnot. The cached-price loader was\nalready SSE2-only, so both tiers share it.\n\nThe NEON tier no longer requires the optional `crc` extension (that was\nthe removed hash mix); AArch64 parts without `crc` now get NEON instead of\nscalar. AArch64 with the tier compiled out newly resolves to the scalar\nkernel, a configuration that previously could not build at all.\n\nTests: the price-set tier test now covers the SSE2 helpers, plus a new\ncase pinning the compare as unsigned above `i32::MAX` across every tier,\nwhich is exactly where a signed compare would invert the result and where\nruntime dispatch on a modern host would never reach the SSE2 path.\n\nBREAKING CHANGE: the `kernel_sse2` feature is renamed to `kernel_sse`, and\nit now gates encoder SIMD as well as decoder SIMD. A build that disables\nthe kernel features loses encoder SIMD it previously kept.\n\nPart of #474\n\n* test(encoder): gate the per-tier test helpers on their kernel features\n\nThe tier-comparison and tag-mask tests referenced SIMD helpers\nunconditionally, so a build with those kernels compiled out failed to\nbuild its test targets even though the library itself was fine. Gate each\nhelper and its call site on the same feature as the tier it exercises, and\nadd the new SSE2 tier to the list the match-generator test cross-checks\nagainst scalar.\n\nFound by running the feature matrix on x86; an aarch64 host does not\ncompile these paths, so the breakage was invisible locally.\n\nPart of #474\n\n* test(encoder): silence the scalar-only tier-list lint\n\nWith every SIMD kernel compiled out the tier list is a single push, which\nclippy flags as vec-init-then-push. The incremental shape is what the\nfeature gates need, so allow the lint at that binding with the reason.\n\nPart of #474\n\n* test(encoder): seed the tier list instead of pushing into an empty vec\n\nAllowing vec-init-then-push on the binding did not take, since the lint\nfires on the statement pair. Seed the vec with the always-present scalar\ntier instead, and allow unused_mut for the build where every SIMD entry\nbelow is compiled out.\n\nPart of #474\n\n* docs: correct the SIMD selection story for wasm and the encoder\n\nTwo things were wrong in the same paragraph. The claim that `std` means\nruntime tier selection does not hold on wasm32: there is no runtime feature\ndetection there, and both the decoder kernels and the encoder fastpath also\nrequire `target_feature = \"simd128\"`, so a default-feature wasm build with\nno extra flags silently stays scalar. The npm package only appears to pick\nat runtime because it ships separately compiled scalar and `+simd128`\npayloads. Document the `-C target-feature=+simd128` requirement.\n\nThe rest of the paragraph described the pre-gating world: the `kernel_*`\nflags now cover the encoder too, x86 has two 128-bit tiers under\n`kernel_sse`, and the NEON tier no longer wants the `crc` extension. Update\nthe table and the crate-level docs to match, including the rename.\n\nPart of #474\n\n* feat(encoder): give long-distance matching its own feature\n\nLDM was gated on `hash`, because its match finder hashes each window with\nXXH64. That made the flag mean two unrelated things: a `--no-default-features`\nbuild dropping `hash` for the checksum also silently lost long-distance\nmatching, while `enable_long_distance_matching(true)` kept being accepted and\nthe frame kept being valid, just without any long-distance matches.\n\nSplit it out as `ldm = [\"hash\"]`, default-on, and move the LDM-specific gates\n(the module, the producer slot and its plumbing, the dict-snapshot handoff,\nthe strategy-ordinal helper) onto it. `hash` now means the checksum only, and\n`hash` without `ldm` is a build that could not be expressed before.\n\nThe name sits one letter away from the unrelated `lsm` feature, so the\nmanifest says so at both entries.\n\nPart of #474\n\n* chore(deps): drop the empty dhat-heap feature, mark the internal ones\n\n`structured-zstd/dhat-heap` gated no code at all — the dhat allocator swap\nlives entirely in the `ffi-bench` example — so the feature and the forward\nfrom `ffi-bench` are both removed; the example keeps its own flag.\n\nThe remaining bench / fuzz / diagnostic features are real but not public\nAPI. Group them under a header saying so, since they show up in the\ncrates.io feature list where a consumer could reasonably mistake them for\nsupported knobs.\n\nStale `kernel_sse2` spellings left by the rename are updated to\n`kernel_sse`.\n\nPart of #474\n\n* fix(encoder): gate the wasm optimal-parser SIMD on kernel_simd128\n\nThe optimal parser's wasm path checked only `target_feature = \"simd128\"`,\nnever the cargo feature, in eleven `cfg`s across `hc/optimal.rs` and\n`hc/priceset.rs`. So a `wasm32` build with `-C target-feature=+simd128` but\n`--no-default-features` still compiled and selected\n`build_optimal_plan_impl_simd128` and its `v128` price-set helpers at levels\n16-22, which is exactly the guarantee the previous commit strengthened.\n\nMeasured on a `+simd128 --no-default-features --features std,hash` build:\n`nm` counted 18 simd128 symbols before, 0 after; adding `kernel_simd128`\nback brings the 18 return.\n\nAlso corrects two feature-doc claims: `kernel_vbmi2` and `kernel_sve` are\ndecoder-only (the encoder has no AVX-512 or SVE tier), and `kernel_vbmi2`\nis the one kernel feature that is off by default.\n\nPart of #474\n\n* refactor(deps): rename the snake_case features to kebab-case\n\nCargo's own convention is kebab-case, and the manifest was mixed:\n`critical-section` and `rustc-dep-of-std` already used it while the kernel,\ndict and bench features did not. Rename the remaining twelve across the\nworkspace — the library, the four dependent crates, the fuzz manifest, CI,\nand the `--features` lines in example doc comments.\n\n`zdict_builder` (a feature of the `zstd` crate) and the\n`dict_builder_fastcover` bench target keep their names; so do Rust\nidentifiers such as `kernel_trace_enabled` and the\n`select_x86_kernel_*` test names.\n\nBREAKING CHANGE: every snake_case feature is renamed to its kebab-case\nspelling: `dict_builder` to `dict-builder`, `kernel_sse` to `kernel-sse`,\nand likewise for `kernel_scalar`, `kernel_bmi2`, `kernel_avx2`,\n`kernel_vbmi2`, `kernel_neon`, `kernel_sve`, `kernel_simd128`,\n`bench_internals`, `fuzz_exports`, `copy_shape_stats` and `kernel_trace`.\n\nPart of #474\n\n* test(bench): find the decode corpus from the ffi-bench manifest dir\n\nThe bench targets belong to `ffi-bench` (they link the C bindings) while\ntheir sources and `decodecorpus_files/z000033` live under `zstd/`. The\nlookup only tried `CARGO_MANIFEST_DIR/decodecorpus_files/z000033`, which\nfor a local `cargo bench -p ffi-bench` resolves under `ffi-bench/` where\nthe fixture does not exist — so the run silently substituted the synthetic\n1 MiB corpus and reported it as `decodecorpus-synthetic-1m`.\n\nCI is unaffected: it passes `STRUCTURED_ZSTD_BENCH_CORPUS_PATH` explicitly.\nThis only fixes the local path, where the substitution is easy to miss and\nmeans benching different data than intended.\n\nAdds the `../zstd/decodecorpus_files/z000033` sibling candidate.\n\nPart of #474\n\n* fix(encoder): use the simd128 candidate collector in the wasm DP wrapper\n\n`build_optimal_plan_impl_simd128` passed\n`collect_optimal_candidates_initialized_scalar` to the body macro, so a\nwasm build at levels 16-22 ran the simd128 price set over scalar BT\ncandidate collection. The simd128 collector added earlier in this branch\nwas reachable only through the `#[cfg(test)]` shim, which is why nothing\nfailed.\n\nPass the simd128 collector, matching what the native tiers do. Confirmed\non a `+simd128 --features kernel-simd128` build: `nm` now finds the\ncollector's monomorphisations in the archive.\n\nPart of #474\n\n* refactor(encoder): rename the sse42 fastpath module to sse2\n\nThe module holds only SSE2 intrinsics — `_mm_cmpeq_epi8`, `_mm_loadu_si128`,\n`_mm_movemask_epi8` — and its functions were lowered to\n`target_feature(enable = \"sse2\")` when the dead CRC hash mix went away. The\n`sse42` name dated from that mix and had been describing the wrong ISA ever\nsince, which is exactly the kind of name this branch has been removing\nelsewhere.\n\nRenames the file, the module path, `Sse42Tags`, and the nine per-tier\nwrappers that compile under the SSE2 umbrella\n(`bt_insert_step_no_rebase`, `bt_insert_and_collect_matches`,\n`bt_update_tree_until`, `hash3_candidate`, `row_probe`, `lazy`,\n`for_each_repcode_candidate_with_reps`, `start_matching_fast_loop`,\n`cbfd_borrowed`).\n\n`build_optimal_plan_impl_sse42` and\n`collect_optimal_candidates_initialized_sse42` keep their names: those do\nneed SSE4.2, since the price set calls `priceset_range_nonabort_sse41`.\n`FastpathKernel::Sse42` likewise still names the real SSE4.2 tier.\n\nPart of #474\n\n* refactor(encoder): scope the scalar BT-probe dead-code allow to the NEON tier\n\nThe allow claimed the probe is unused on every little-endian aarch64\nbuild. That stopped being true once the tiers were gated: with\n`kernel-neon` off, the scalar walker is compiled back in and this probe is\nthe live path. Narrow the attribute and its documentation to the case that\nactually holds.\n\nPart of #474\n\n* ci(wasm): build the scalar check with kernel-scalar\n\nThe step named \"wasm32 scalar\" passed `--features kernel-simd128`. It did\nbuild scalar code, since the wasm kernels also need\n`target_feature = \"simd128\"` and that step sets no rustflags, but it never\nexercised a `kernel-scalar` build, so a scalar-only compile or dispatch\nregression could pass CI unseen.\n\nPoint it at `kernel-scalar` and keep the old invocation as its own step:\n\"feature on, target feature off\" is exactly the combination where a\nmissing cargo-feature gate hides, which this branch already had to fix\nonce in the optimal parser.\n\nAlso updates the LDM comments that still described `hash` gating after the\ncfgs moved to `ldm`, and corrects the `start_matching_optimal` reference to\n`hc/optimal.rs` (verified with ast-grep: the definition is at\n`hc/optimal.rs:1001` and the `prepare_ldm_candidates` call at `:1044`).\n\nPart of #474",
          "timestamp": "2026-09-02T17:48:10+03:00",
          "tree_id": "6e04e5f33db0c11643730f5da441248d514f5029",
          "url": "https://github.com/structured-world/structured-zstd/commit/3b37ed6f16210408eb12dd4158234f652bd9e159"
        },
        "date": 1788363076877,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.069,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.066,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 190.641,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 186.036,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.871,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.351,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.365,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.653,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.383,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.681,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 7.386,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 3.523,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.099,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.1,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.088,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.895,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.206,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.965,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.107,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.106,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fe45d4455045bb953764f54697176c128ff3ad7c",
          "message": "perf(encoder): read owned blocks straight into the matcher history (#477)\n\n* docs(readme): restructure for scannability, refresh discovery metadata\n\n- README: lead with a Highlights list; move the SIMD kernel-feature prose\n  out of Quick start into a Feature flags table (AVX-512 rationale folded\n  into a details block); break the storage-extensions and performance\n  paragraphs into bullets; drop the finished encoder-rewrite notice;\n  embedded doctest code blocks unchanged\n- Cargo.toml: description now covers the full encoder, streaming, no_std\n  and WebAssembly; keywords swap pure-rust for no-std; categories add\n  no-std, wasm and encoding; homepage points at the public benchmark\n  dashboard\n\nCloses #472\n\n* docs(readme): correct level range, npm dependency claim, resume window contract\n\n- state the encoder level range as -131072..=22 (the full negative fast\n  range is supported, matching the C API minimum)\n- the npm package carries a runtime dependency (wasm-feature-detect); the\n  accurate claim is no native addons / no postinstall scripts\n- resumable-decoding bullet now notes that ResumeState does not carry the\n  match window and the resuming call supplies it via ResumeInput::window_prime\n\n* docs(readme): scope SIMD flags to the decoder, note LDM needs hash\n\n- The `hash` feature also gates long-distance matching (the LDM match\n  finder hashes each window with XXH64), not only content checksums.\n  Without it the builder still accepts `enable_long_distance_matching`\n  and the frame stays valid, but no long-distance matches are produced;\n  say so in the feature table and beside the LDM example.\n- The `kernel_*` flags gate the decoder kernels only. The encoder\n  fastpath picks its tier independently: at runtime under `std`, from\n  the target's `target_feature` set under `no_std`. So dropping the\n  flags while keeping `std`, or building `no_std` for a target tuned\n  with `+avx2,+bmi2,+sse4.2`, still runs explicit encoder SIMD.\n  `kernel_simd128` is the one flag that also gates an encoder path\n  (wasm), which the table now says.\n\nPart of #472\n\n* refactor(encoder): drop the dead per-tier hash mix\n\n`hash_mix_u64` had five per-tier implementations and no production caller:\nthe only references in the workspace were two unit tests. Match hashing\nruns through `MatchTable::hash_value_with_mls` / `hash_value8_with_mls`,\nwhich mirror upstream `ZSTD_hash4/5/6/8`. The module-level\n`allow(dead_code)` (added as scaffolding, with a note to drop it once\nconsumers came online) is why this never surfaced.\n\nThree comments described the dead code inaccurately:\n\n- sse42 called it a mirror of an upstream \"CRC-folded hash mix\". Upstream\n  has no such thing: `crc32` does not appear anywhere in `lib/compress/`\n  on 1.5.7 or on dev, and `ZSTD_hash4/5/6/8` are plain multiplications.\n- neon claimed its mix matched the x86 kernels. It cannot: `__crc32d` is\n  CRC-32 (poly 0x04C11DB7) while `_mm_crc32_u64` is CRC-32C (Castagnoli).\n- simd128 claimed bit-identity with every other tier, while the scalar\n  mix is a bare multiply.\n\nRemoving it drops the CRC requirement that only that function had, so each\ntier now declares what it actually executes:\n\n- sse42 -> SSE2 (its three live intrinsics are all SSE2)\n- avx2_bmi2 -> avx2,bmi2 (was avx2,bmi2,sse4.2)\n- neon -> the aarch64 NEON baseline, no optional `crc` extension\n\nThe runtime probes shrink accordingly, and the NEON tier stops falling\nback to scalar on aarch64 parts without `crc`.\n\nAlso removed alongside: `dispatch_count_match_from_indices` and the\nper-tier `KERNEL_TAG` constants (both unreferenced workspace-wide), and\nthe module-level `allow(dead_code)` from the fastpath, scalar and simd128\nmodules so this cannot accumulate silently again. Two items that are\ngenuinely unreachable per target keep a scoped `allow` with the reason:\nthe scalar BT probe on aarch64 (its walker is compiled out there) and the\nsimd128 BT probe (the BT walker has no wasm wrapper yet).\n\nRenamed `Row::hash_kernel` to `cpl_kernel`: it feeds\n`common_prefix_len_with_kernel`, not the mix its name and doc claimed.\n\nCompressed output is unchanged; dead code cannot affect it. Verified on\naarch64, x86_64, thumbv7em-none-eabihf and wasm32 (+simd128).\n\nPart of #474\n\n* perf(encoder): wire the wasm simd128 tier into the BT walker\n\nThe simd128 tier had a `count_match_from_indices` that nothing called: the\nBT walker only had neon / sse2 / avx2 / scalar wrappers, so a wasm build\nran the BT walk through the scalar probe. Add the missing simd128 arm to\nall three dispatchers (`bt_insert_step_no_rebase`,\n`bt_insert_and_collect_matches`, `collect_optimal_candidates_initialized`)\nso the wasm tier reaches its own vector probe.\n\nMeasured, interleaved A/B on the bench host (one process loading both\npayloads, alternating samples, 25 per arm, load < 0.3), compress at the BT\nlevels:\n\n  decodecorpus-z000033  L19 +0.22%   L22 +0.06%\n  small-4k-log-lines    L19 -0.23%   L22 -0.03%\n  low-entropy-1m        L19 +0.01%   L22 +0.15%\n  large-log-stream-4m   L19 -0.14%   L22 -0.26%\n\nEncoded bytes identical on every fixture and level, so the wiring is\ncorrect; the throughput effect is nil, including on the long-match\nfixtures where a vector prefix compare was expected to pay. The payload\ngrows 1574 bytes (1023318 -> 1024892, budget 1.25 MiB).\n\nPart of #474\n\n* docs(encoder): record why the BT vector probe barely fires\n\nInstrumented the BT-path prefix compare over decodecorpus-z000033 to\nexplain the flat A/B on the wasm tier wiring. At both L19 and L22 the\nleading 8-byte word probe resolves 87.45% of calls; only 12.55% reach the\nvector loop, and those average 1.6 (L19) to 4.6 (L22) 16-byte iterations.\nA short buffer (`max < 16`) accounts for 0.05%, so the early exit is\ndriven by the data, not by the buffer bound.\n\nThat is the whole explanation for the flat measurement: the vector cannot\nmatter much when 87% of compares end before it starts. Noted at the probe\nso the next person does not re-run the same experiment, along with the\nimplication that the lever on this path is cutting the number of candidate\ncompares, not widening the survivors.\n\nAlso drops two stale references to the removed hash mix from the module\nheader and the wasm dispatch comment.\n\nPart of #474\n\n* feat(encoder): gate encoder SIMD on the kernel features, add an SSE2 tier\n\nThe `kernel_*` features gated only the decoder: `fastpath::select_kernel`\npicked SSE4.2 / AVX2+BMI2 / NEON on its own, so `--no-default-features`\nstill executed encoder SIMD. Every encoder tier (module, enum variant,\ndispatch arm, tag-mask macro, per-tier wrapper) now sits behind the same\nfeature as its decoder counterpart, and a build with no kernel features\nemits no explicit encoder SIMD: `nm` counts 99 avx2/sse42 symbols with the\ndefault features and 0 without.\n\nThe x86 tier turned out not to be a single tier. Its prefix-compare kernel\nis plain SSE2, but the optimal parser's price set calls\n`priceset_range_nonabort_sse41`, whose `_mm_min_epu32` is SSE4.1. Rather\nthan keep probing the whole tier on SSE4.2, split it:\n\n- `Sse42`: unchanged, SSE4.1 price set.\n- `Sse2`: new. Same 128-bit prefix-compare kernel plus an SSE2 price set,\n  for x86 CPUs without SSE4.2. These previously fell all the way back to\n  the scalar kernel despite having usable SSE2.\n\nSSE2 has no unsigned 32-bit compare, so the new improve-mask biases both\noperands by 0x8000_0000 and uses the signed compare, which yields the same\nmask in one compare instead of min/eq/andnot. The cached-price loader was\nalready SSE2-only, so both tiers share it.\n\nThe NEON tier no longer requires the optional `crc` extension (that was\nthe removed hash mix); AArch64 parts without `crc` now get NEON instead of\nscalar. AArch64 with the tier compiled out newly resolves to the scalar\nkernel, a configuration that previously could not build at all.\n\nTests: the price-set tier test now covers the SSE2 helpers, plus a new\ncase pinning the compare as unsigned above `i32::MAX` across every tier,\nwhich is exactly where a signed compare would invert the result and where\nruntime dispatch on a modern host would never reach the SSE2 path.\n\nBREAKING CHANGE: the `kernel_sse2` feature is renamed to `kernel_sse`, and\nit now gates encoder SIMD as well as decoder SIMD. A build that disables\nthe kernel features loses encoder SIMD it previously kept.\n\nPart of #474\n\n* test(encoder): gate the per-tier test helpers on their kernel features\n\nThe tier-comparison and tag-mask tests referenced SIMD helpers\nunconditionally, so a build with those kernels compiled out failed to\nbuild its test targets even though the library itself was fine. Gate each\nhelper and its call site on the same feature as the tier it exercises, and\nadd the new SSE2 tier to the list the match-generator test cross-checks\nagainst scalar.\n\nFound by running the feature matrix on x86; an aarch64 host does not\ncompile these paths, so the breakage was invisible locally.\n\nPart of #474\n\n* test(encoder): silence the scalar-only tier-list lint\n\nWith every SIMD kernel compiled out the tier list is a single push, which\nclippy flags as vec-init-then-push. The incremental shape is what the\nfeature gates need, so allow the lint at that binding with the reason.\n\nPart of #474\n\n* test(encoder): seed the tier list instead of pushing into an empty vec\n\nAllowing vec-init-then-push on the binding did not take, since the lint\nfires on the statement pair. Seed the vec with the always-present scalar\ntier instead, and allow unused_mut for the build where every SIMD entry\nbelow is compiled out.\n\nPart of #474\n\n* docs: correct the SIMD selection story for wasm and the encoder\n\nTwo things were wrong in the same paragraph. The claim that `std` means\nruntime tier selection does not hold on wasm32: there is no runtime feature\ndetection there, and both the decoder kernels and the encoder fastpath also\nrequire `target_feature = \"simd128\"`, so a default-feature wasm build with\nno extra flags silently stays scalar. The npm package only appears to pick\nat runtime because it ships separately compiled scalar and `+simd128`\npayloads. Document the `-C target-feature=+simd128` requirement.\n\nThe rest of the paragraph described the pre-gating world: the `kernel_*`\nflags now cover the encoder too, x86 has two 128-bit tiers under\n`kernel_sse`, and the NEON tier no longer wants the `crc` extension. Update\nthe table and the crate-level docs to match, including the rename.\n\nPart of #474\n\n* feat(encoder): give long-distance matching its own feature\n\nLDM was gated on `hash`, because its match finder hashes each window with\nXXH64. That made the flag mean two unrelated things: a `--no-default-features`\nbuild dropping `hash` for the checksum also silently lost long-distance\nmatching, while `enable_long_distance_matching(true)` kept being accepted and\nthe frame kept being valid, just without any long-distance matches.\n\nSplit it out as `ldm = [\"hash\"]`, default-on, and move the LDM-specific gates\n(the module, the producer slot and its plumbing, the dict-snapshot handoff,\nthe strategy-ordinal helper) onto it. `hash` now means the checksum only, and\n`hash` without `ldm` is a build that could not be expressed before.\n\nThe name sits one letter away from the unrelated `lsm` feature, so the\nmanifest says so at both entries.\n\nPart of #474\n\n* chore(deps): drop the empty dhat-heap feature, mark the internal ones\n\n`structured-zstd/dhat-heap` gated no code at all — the dhat allocator swap\nlives entirely in the `ffi-bench` example — so the feature and the forward\nfrom `ffi-bench` are both removed; the example keeps its own flag.\n\nThe remaining bench / fuzz / diagnostic features are real but not public\nAPI. Group them under a header saying so, since they show up in the\ncrates.io feature list where a consumer could reasonably mistake them for\nsupported knobs.\n\nStale `kernel_sse2` spellings left by the rename are updated to\n`kernel_sse`.\n\nPart of #474\n\n* fix(encoder): gate the wasm optimal-parser SIMD on kernel_simd128\n\nThe optimal parser's wasm path checked only `target_feature = \"simd128\"`,\nnever the cargo feature, in eleven `cfg`s across `hc/optimal.rs` and\n`hc/priceset.rs`. So a `wasm32` build with `-C target-feature=+simd128` but\n`--no-default-features` still compiled and selected\n`build_optimal_plan_impl_simd128` and its `v128` price-set helpers at levels\n16-22, which is exactly the guarantee the previous commit strengthened.\n\nMeasured on a `+simd128 --no-default-features --features std,hash` build:\n`nm` counted 18 simd128 symbols before, 0 after; adding `kernel_simd128`\nback brings the 18 return.\n\nAlso corrects two feature-doc claims: `kernel_vbmi2` and `kernel_sve` are\ndecoder-only (the encoder has no AVX-512 or SVE tier), and `kernel_vbmi2`\nis the one kernel feature that is off by default.\n\nPart of #474\n\n* refactor(deps): rename the snake_case features to kebab-case\n\nCargo's own convention is kebab-case, and the manifest was mixed:\n`critical-section` and `rustc-dep-of-std` already used it while the kernel,\ndict and bench features did not. Rename the remaining twelve across the\nworkspace — the library, the four dependent crates, the fuzz manifest, CI,\nand the `--features` lines in example doc comments.\n\n`zdict_builder` (a feature of the `zstd` crate) and the\n`dict_builder_fastcover` bench target keep their names; so do Rust\nidentifiers such as `kernel_trace_enabled` and the\n`select_x86_kernel_*` test names.\n\nBREAKING CHANGE: every snake_case feature is renamed to its kebab-case\nspelling: `dict_builder` to `dict-builder`, `kernel_sse` to `kernel-sse`,\nand likewise for `kernel_scalar`, `kernel_bmi2`, `kernel_avx2`,\n`kernel_vbmi2`, `kernel_neon`, `kernel_sve`, `kernel_simd128`,\n`bench_internals`, `fuzz_exports`, `copy_shape_stats` and `kernel_trace`.\n\nPart of #474\n\n* test(bench): find the decode corpus from the ffi-bench manifest dir\n\nThe bench targets belong to `ffi-bench` (they link the C bindings) while\ntheir sources and `decodecorpus_files/z000033` live under `zstd/`. The\nlookup only tried `CARGO_MANIFEST_DIR/decodecorpus_files/z000033`, which\nfor a local `cargo bench -p ffi-bench` resolves under `ffi-bench/` where\nthe fixture does not exist — so the run silently substituted the synthetic\n1 MiB corpus and reported it as `decodecorpus-synthetic-1m`.\n\nCI is unaffected: it passes `STRUCTURED_ZSTD_BENCH_CORPUS_PATH` explicitly.\nThis only fixes the local path, where the substitution is easy to miss and\nmeans benching different data than intended.\n\nAdds the `../zstd/decodecorpus_files/z000033` sibling candidate.\n\nPart of #474\n\n* fix(encoder): use the simd128 candidate collector in the wasm DP wrapper\n\n`build_optimal_plan_impl_simd128` passed\n`collect_optimal_candidates_initialized_scalar` to the body macro, so a\nwasm build at levels 16-22 ran the simd128 price set over scalar BT\ncandidate collection. The simd128 collector added earlier in this branch\nwas reachable only through the `#[cfg(test)]` shim, which is why nothing\nfailed.\n\nPass the simd128 collector, matching what the native tiers do. Confirmed\non a `+simd128 --features kernel-simd128` build: `nm` now finds the\ncollector's monomorphisations in the archive.\n\nPart of #474\n\n* refactor(encoder): rename the sse42 fastpath module to sse2\n\nThe module holds only SSE2 intrinsics — `_mm_cmpeq_epi8`, `_mm_loadu_si128`,\n`_mm_movemask_epi8` — and its functions were lowered to\n`target_feature(enable = \"sse2\")` when the dead CRC hash mix went away. The\n`sse42` name dated from that mix and had been describing the wrong ISA ever\nsince, which is exactly the kind of name this branch has been removing\nelsewhere.\n\nRenames the file, the module path, `Sse42Tags`, and the nine per-tier\nwrappers that compile under the SSE2 umbrella\n(`bt_insert_step_no_rebase`, `bt_insert_and_collect_matches`,\n`bt_update_tree_until`, `hash3_candidate`, `row_probe`, `lazy`,\n`for_each_repcode_candidate_with_reps`, `start_matching_fast_loop`,\n`cbfd_borrowed`).\n\n`build_optimal_plan_impl_sse42` and\n`collect_optimal_candidates_initialized_sse42` keep their names: those do\nneed SSE4.2, since the price set calls `priceset_range_nonabort_sse41`.\n`FastpathKernel::Sse42` likewise still names the real SSE4.2 tier.\n\nPart of #474\n\n* refactor(encoder): scope the scalar BT-probe dead-code allow to the NEON tier\n\nThe allow claimed the probe is unused on every little-endian aarch64\nbuild. That stopped being true once the tiers were gated: with\n`kernel-neon` off, the scalar walker is compiled back in and this probe is\nthe live path. Narrow the attribute and its documentation to the case that\nactually holds.\n\nPart of #474\n\n* ci(wasm): build the scalar check with kernel-scalar\n\nThe step named \"wasm32 scalar\" passed `--features kernel-simd128`. It did\nbuild scalar code, since the wasm kernels also need\n`target_feature = \"simd128\"` and that step sets no rustflags, but it never\nexercised a `kernel-scalar` build, so a scalar-only compile or dispatch\nregression could pass CI unseen.\n\nPoint it at `kernel-scalar` and keep the old invocation as its own step:\n\"feature on, target feature off\" is exactly the combination where a\nmissing cargo-feature gate hides, which this branch already had to fix\nonce in the optimal parser.\n\nAlso updates the LDM comments that still described `hash` gating after the\ncfgs moved to `ldm`, and corrects the `start_matching_optimal` reference to\n`hc/optimal.rs` (verified with ast-grep: the definition is at\n`hc/optimal.rs:1001` and the `prepare_ldm_candidates` call at `:1044`).\n\nPart of #474\n\n* test(bench): add a C-side encode loop for instruction-count comparison\n\n`encode_loop_z000033` profiles our encoder in isolation, but there was no\nstructurally identical C counterpart, so a `perf stat` comparison mixed in\nharness differences. This mirrors it: same read-once, allocate-once,\ncompress-N-times shape through `ZSTD_compress`.\n\nPart of #474\n\n* test(bench): add a one-shot slice encode loop for profiling\n\n`encode_loop_z000033` drives `FrameCompressor::compress` from a `Read`\nsource, which takes the owned block loop and copies the input into the\nmatcher history. `compare_ffi` instead times\n`compress_independent_frame_into`, which is borrowed-eligible for the\nDfast/Row/Fast backends and scans the caller's slice in place.\n\nProfiling the former to explain the latter attributes a per-frame copy the\nmeasured path never performs, so keep a binary for each shape.\n\nPart of #474\n\n* perf(encoder): read owned blocks straight into the matcher history\n\nThe owned block loop staged every block in a scratch `Vec`, which the\nmatcher then copied into its `history`, and copied any pre-split remainder\nback out into a carry buffer. On a streaming `compress()` that copy was\n6.35% of runtime (150M of 2362M cycles on decodecorpus-z000033 L3), against\n0.68% for the equivalent C run.\n\nRead into the history buffer instead. The block length is only known after\nthe bytes are read (the pre-split pass needs to see them), so ingest is now\ntwo-phase: `fill_in_place` appends into the history tail without touching\nthe window, the splitter picks a boundary inside `uncommitted_input`, and\n`commit_filled` claims that prefix. Whatever the splitter leaves over stays\nwhere it is and heads the next block, so the carry costs nothing either —\n`pending_input` is unused on this path.\n\n`Matcher` gains the three hooks with defaults that keep the staged path, so\nexternal implementations are unaffected and the other three backends stay\non the old route until they get the same treatment. `compress_block_encoded`\ntakes a `BlockInput` describing where the bytes are; classification (RLE\ndetect, raw fast path, per-block checksum) happens before the commit, where\na shared borrow of the matcher is still available.\n\nOutput is byte-identical: all 168 scenario/level pairs in `compare_ffi`\nreport the same `rust_bytes` as before.\n\nPart of #474\n\n* test(bench): add an owned-path digest example\n\nPrints length plus an FNV digest of the reader-path frame per level, so a\nchange to the owned block loop can be shown byte-identical without a\ncriterion run. The compare_ffi REPORT lines only cover the one-shot\nborrowed path.\n\nPart of #474\n\n* perf(encoder): extend in-place block ingest to Row and HashChain\n\nAdds the two-phase ingest to `RowMatchGenerator` and `MatchTable` (which\nHashChain owns), so three of the four backends now read the block straight\ninto their history instead of staging a copy. Simple keeps the staged path:\nit parks the block in a `pending` slot the kernel consumes later, so its\nbytes do not reach `history` at commit time and the shape does not apply\nas written.\n\nUncommitted bytes are tracked in an explicit `uncommitted_len` rather than\nderived from `window_size`: a primed dictionary also lives in `history`, so\n`history_start + window_size` is not the committed end on a dict frame,\nand deriving it there retired dictionaries mid-frame.\n\nEvery reader of the committed region now stops at that boundary —\n`live_history`, `get_last_space`, the Dfast owned scan descriptor and the\nlast-committed-block pointer. Without it a scan could forward-count into\nthe next block's bytes, which the staged path never had in the buffer.\n\nVerified byte-identical on the one-shot path (all 168 compare_ffi\nscenario/level pairs) and on the reader path for levels 1, 2, 5, 6, 9, 12,\n13, 16, 19 and 22. Levels 3 and 4 (Dfast) still differ from the staged\noutput in 5 bytes of 293823 at equal frame length and equal block\nboundaries — the sequences inside two blocks differ. Not yet root-caused;\ntracked as follow-up before this lands.\n\nPart of #474\n\n* fix(dfast): bound hash inserts and the drain trigger to committed bytes\n\nWith in-place ingest the history buffer also holds bytes no block has\nclaimed yet. Two readers still measured against the raw buffer length:\n\n- `scan_source` reported those bytes as readable, so the insert guard it\n  feeds admitted hash positions inside the next block's data, seeding the\n  tables with candidates the staged path never had.\n- `compact_history`'s drain trigger compared `history_start` against the\n  full buffer length, firing later than on the staged path.\n\nBoth now measure the committed length, matching `owned_scan_descriptor`.\nLevels 3 and 4 were the only ones affected; encoder output across levels\n1-22 is now byte-identical to the staged path on the corpus fixture and\non a 600 KB prefix (previously 5 bytes of 293823 differed, at equal frame\nlength and equal block boundaries).\n\n* perf(encoder): apply the eviction ceiling on commit, not on fill\n\n`fill_uncommitted` sized the one-time eviction ceiling off the read\nbuffer's capacity rather than a block length, so `window_size + capacity\n> max_window_size` held on the very first fill and reserved the full\nwindow + window/4 mirror for frames that never evict at all. Every fresh\nframe then faulted that mirror in.\n\nMeasured on the i9 at level 13 (1 MB corpus, 10 iters): page-faults\n14,561 -> 47,367 and cycles 1.523G -> 1.703G against the staged path.\nThe ceiling now runs in `commit_block`, at the same point in the\nsequence as `add_data` and keyed on the length a block actually claims.\n\nThe drain trigger in Row and the match-table storage gets the same\ncommitted-length bound the dfast one already has.\n\nOutput stays byte-identical across levels 1-22 on the corpus fixture.\n\n* perf(encoder): size the ingest buffer once per frame\n\nThe per-block `reserve` grew the in-place ingest buffer along a doubling\nchain, so a 1 MB frame walked four allocations. With a fresh compressor\nper frame that churn fragments the allocator arena: peak RSS was\nunchanged (23 MB) yet minor faults tripled (14.5K -> 47.4K) and landed in\nthe table allocation rather than the buffer itself.\n\nA pledged content size now sizes the buffer up front, clamped to the\neviction ceiling so an over-long or absent hint cannot over-reserve.\n\n* perf(encoder): reserve a block of slack with the frame\n\nThe final top-up asks for a whole block's room even when only a tail of\nthe frame remains, so reserving exactly the pledged size still forced one\nreallocation (and one copy of the frame) per frame. Reserve the pledged\nsize plus a block, still clamped to the eviction ceiling.\n\n* fix(encoder): keep Uncompressed frames off the in-place ingest path\n\n`fill_in_place` dispatches on the matcher, not on the level, so the\nstaged-path assumption for `Uncompressed` held only because the built-in\ndriver resolves that level to a backend without in-place ingest. An\nexternal `M: Matcher` that implements the hook broke it: debug builds hit\nthe assert, release builds emitted an empty Raw block while the payload\nstayed uncommitted in the matcher, losing the frame's data.\n\nThe gate is now on the level, at the ingest site. Carries a regression\ntest with a matcher that does implement in-place ingest.\n\n* fix(encoder): carry the uncommitted count through reset and clone_from\n\nTwo paths left the in-place ingest count describing a buffer that no\nlonger existed. Every bound is `history.len() - uncommitted_len`, so a\nstale count silently truncates the live window or underflows.\n\n- `reset` cleared history but kept the count. Bytes an abandoned frame\n  ingested without claiming are now dropped for Dfast, Row and the match\n  table, before the floor advance that would otherwise count them.\n- `clone_from` (the primed-dictionary restore) overwrote history from the\n  snapshot without adopting the source's count, unlike `clone`.\n\nCarries regression tests for both, which panicked on subtract-with-overflow\nbefore the fix.\n\n* fix(encoder): drop stale uncommitted bytes and harden the block claim\n\nCompanion to the tests in the previous commit. Two paths left the\nin-place ingest count describing a buffer that no longer existed, and\nevery bound is `history.len() - uncommitted_len`, so a stale count\nsilently truncates the live window or underflows.\n\n- `reset` cleared history but kept the count. Bytes an abandoned frame\n  ingested without claiming are now dropped for Dfast, Row and the match\n  table, before the floor advance that would otherwise count them.\n- `clone_from` (the primed-dictionary restore) overwrote history from the\n  snapshot without adopting the source's count, unlike `clone`.\n\nAlso here: the per-block claim check becomes a hard assert (it runs once\nper block, and a release-mode wrap surfaces as a panic far from the\ncause), the wasm `simd128` tier gains its tree-update and HC3 probe\nvariants so a simd128 build no longer drives a SIMD insert step from a\nscalar walk, and the row SSE probe's SAFETY note now names SSE2, which is\nwhat it actually requires.\n\n* perf(encoder): hash each block once, and only when a sink collects it\n\nThe compressed branch discarded the checksum taken from the pre-commit\nview and hashed the block a second time from the committed copy, though\nboth cover the same bytes. It now reuses the single pass, and that pass\nis skipped entirely when no sink collects checksums, which is the common\ncase for a whole-block hash.\n\nAlso: the owned-path digest example walks every level 1-22 rather than a\nsample of twelve, and the benchmark quick-start selects the package that\nowns `compare_ffi`, so the documented command actually runs.\n\n* perf(dfast): derive literal lengths at the emit sites\n\n* perf(dfast): scan the block through one cursor base like upstream\n\n* perf(dfast): gate the rep probe in block coordinates\n\nContinues folding the fast loop onto one coordinate system. The rep probe\nheld two absolute-position checks (`rep1 <= abs_ip1` and the candidate\nagainst the window floor) that together say one thing about the index:\nthe back-reference lands at or after the start of live history. In block\ncoordinates that is a single `rep1 <= idx1`, and the borrowed floor\nbecomes `rep1 <= advertised_window`; both const branches fold.\n\nSlot packing loses its `position_base` term the same way: it is a\nper-block constant, so the per-position work is one add.\n\n* fix(encoder): retire the dictionary budget on in-place commits\n\nThree defects from review, with the regression tests that proved them.\n\n`commit_filled` skipped the eviction accounting `commit_space` runs, so a\ndictionary-backed frame fed from a reader kept its inflated\n`max_window_size` after the dictionary bytes were evicted. The backend\nthen admitted matches older than the window the frame header reports:\nthe new test compresses periodic data through a 1 KiB window and the\ndecoder rejected the result with `OffsetTooBig { offset: 4096, buf_len:\n2048 }`.\n\nReaching that test first needed a block-sizing fix: upstream sizes a\nblock as `MIN(maxBlockSize, windowSize)`, we always asked for the full\n128 KiB, and the matcher asserts on a block wider than its window. This\nchanges block boundaries for frames whose window is smaller than the\nblock size, which previously could not run at all.\n\nThe stream-headroom guard counted only the top-up, not the bytes the\ningest buffer already carried (and the EOF re-inspection passes a\ncapacity of zero), so a commit could walk past the slack the unchecked\nabsolute-position lookahead relies on. All three backends now count both.\n\nAlso from review: the whole-block checksum is skipped for the post-split\npath, whose helper records a checksum per emitted partition, and the\nclone_from test now stages bytes in the source instead of asserting\nagainst an empty one.\n\nInterop coverage grew to match: the C codec roundtrip now runs the whole\nlevel ladder in both directions, plus a level x LDM x dictionary matrix\nthat asserts LDM actually changes the output.\n\n* perf(dfast): unpack slots straight into concat indices\n\n* perf(dfast): drop the checked_sub form from the candidate gates\n\n* perf(dfast): RESULT unpacking slots into indices loses, keep the position gate\n\nMeasured on the i9, `encode_loop_z000033 3 40` on the corpus, interleaved\nwith a flat c_ffi control arm (899-920M cycles throughout):\n\n  position gate (kept)   1.963G / 1.966G cycles, 4.217G instructions\n  index gate, checked    2.036G / 2.040G cycles, 4.296G instructions  +3.7%\n  index gate, compares   1.977G / 1.975G cycles, 4.239G instructions  +0.6%\n\nThe idea was that unpacking a slot straight into a concat index would take\nthe absolute position off the hot path. It does not pay: the emit paths\nstill need the absolute position, so nothing is freed, and the rebase\nconstant is added work on every candidate.\n\nBetween the two index forms, `checked_sub` cost the most — not the\ninstruction, which is a subtract plus a flag test, but the `if let\nSome(..)` around it, which reorders the branch and the register\nallocation. Two bare comparisons recover most of that.\n\nKept from this line of work (all measured wins, all byte-identical): the\nblock-relative cursor, the per-block packing constant, and the rep probe\nin block coordinates. Level 3 stands at 2.15x the C reference, down from\n2.44x.\n\n* fix(encoder): reserve the ingest buffer only for a pledged size\n\nTwo review findings on the frame-sized reservation.\n\nAn advisory `set_source_size_hint` is an estimate, not a promise, so a\ncompressor with a large window and a tiny reader could be asked to\nallocate hundreds of MiB before reading a byte. The reservation is now\ngated on the hint being exact; an inexact one keeps the doubling growth,\nwhich is bounded by what actually arrives.\n\nThe block-sized slack was the format maximum rather than the block the\nframe will really cut, so a 1 KiB-window frame reserved ~129 KiB for a\n1 KiB payload. The caller derives the slack from the active block\ncapacity and the backends take the request as given, with a test that\npins the reservation to the request.",
          "timestamp": "2026-09-02T23:35:17+03:00",
          "tree_id": "75cd2310a7d00a129214966b21347e9108508905",
          "url": "https://github.com/structured-world/structured-zstd/commit/fe45d4455045bb953764f54697176c128ff3ad7c"
        },
        "date": 1788383842326,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.067,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.064,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 172.689,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 166.521,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.874,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.348,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.485,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.692,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.455,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.732,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.129,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.13,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 11.559,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 5.958,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.135,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.228,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.547,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.148,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.73,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.253,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.172,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.167,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "23f70e2e8ca299bbcd023219b0faf64321e313f1",
          "message": "perf(encoder): hold each backend's index tables in one buffer (#479)\n\n* perf(row): hold the row and chain tables in one buffer\n\n* perf(row): reach the row table through its accessor in the x86 prefetch\n\n* perf(hc): hold the hash, chain and hash3 tables in one buffer\n\n* fix(encoder): hand back the table buffer when the layout shrinks\n\nConsolidating the tables into one buffer swapped `vec![..]` for `clear` +\n`resize`, which reduces the length and keeps the capacity. A reused\ncompressor coming down from a wide level therefore pinned the largest\nallocation it had ever made: at the btlazy2 levels the Row backend kept\n16 MB resident through every later row frame, and the HashChain backend\nkept its widest hash and chain for the rest of its life. The separate\nvectors released that; the shared buffer has to as well.\n\nBoth now hand the allocation back once it exceeds twice the layout being\nbuilt. The threshold is hysteresis, not a tight fit: re-using the buffer\nis the point of consolidating it, so levels within a factor of two trade\nthe same allocation instead of churning it, which is the same shape\nupstream gives its workspace.\n\nCarries a regression test per backend, each of which kept the full\noversized capacity before the fix.\n\n* refactor(encoder): express the oversize threshold as a halving\n\nThe threshold was `wanted * 2`, which needs a clamp to stay inside the\ntype, and the clamp fails open: `usize::MAX` compares above every\ncapacity, so an overflow would silently disable the release the previous\ncommit added rather than surface anything.\n\nPhrasing it as `capacity / 2 > wanted` removes the arithmetic that could\nleave the type at all, and reads as what it is — a predicate, not a size.",
          "timestamp": "2026-09-03T01:38:44+03:00",
          "tree_id": "ffe62ff8c06f9713f886e2db558a94bdeb3e80f7",
          "url": "https://github.com/structured-world/structured-zstd/commit/23f70e2e8ca299bbcd023219b0faf64321e313f1"
        },
        "date": 1788391387637,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.111,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 208.642,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 272.459,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.709,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.371,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.766,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.971,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.796,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 2.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.008,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 9.674,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.71,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.138,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.165,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.542,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.266,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.709,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.358,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.029,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.148,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.029,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.144,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "2a8676d95c8dcbd88d55a7cad6d97e90e93f7cfc",
          "message": "build(cli): ship the tool from the library crate, with no dependencies (#480)\n\n* build(cli): publish the CLI and accept the upstream flag surface\n\nThe CLI was built but unreachable: `publish = false` kept it off crates.io\nand no release job shipped it, so it existed only for people who cloned\nthe repo. It now publishes alongside the library, from the same release\njob so the library is in the index first.\n\nThe binary is named `structured-zstd`, not `zstd`: `cargo install` writes\ninto a directory that usually precedes `/usr/bin`, so taking that name\nwould shadow the system tool for everyone who installs this. It still\ndispatches on `argv[0]`, so an alternatives entry or a plain `unzstd` /\n`zstdcat` symlink selects the matching default mode. README documents\nboth, plus the install itself, which it did not mention at all.\n\nFlag surface, which mattered more than the packaging: any unrecognised\nflag used to abort, so `zstd -T4 file` — valid upstream — failed here.\nFlags that only steer HOW the work happens are now accepted, since the\nresult is the same valid stream either way: `-T`/`--single-thread`/\n`--auto-threads`, `-M`/`--memory`/`--memlimit`, `--adapt`,\n`--[no-]progress`, `--[no-]check`, `--[no-]sparse`, `--[no-]asyncio`,\n`--[no-]mmap-dict`, `--[no-]pass-through`, `--[no-]compress-literals`,\n`--[no-]row-match-finder`, `--exclude-compressed`.\n\n`--stream-size` / `--size-hint` are threaded through instead of ignored:\nstdin has no length to stat, which is the case upstream offers them for,\nand the encoder sizes its window from the pledge. Verified against the\nupstream `zstd -l`, which reads the declared size back out of the frame.\n\nFlags that would change the OUTPUT still fail loudly — `--format=` for\nanything but zstd, `--patch-from`, `--rsyncable` — because answering a\n`.gz` request with a zstd frame is worse than refusing it.\n\n* build(cli): ship the tool from the library crate, with no dependencies\n\nFollows the xmlsec1 shape: one crate carries both, so `cargo add\nstructured-zstd` and `cargo install structured-zstd` name the same thing,\nand the install needs no feature flags.\n\nThat only works because the tool no longer depends on anything. `clap`\nand `console` turned out to be dead — the argument parser has been\nhand-written all along, since clap's derive cannot model a bare `-19`.\n`indicatif`, `tracing`, `tracing-subscriber` and `tracing-indicatif` are\nreplaced by a ~60-line progress bar and an `info!` that writes to stderr;\n`color-eyre` by a boxed error with `bail!` / `eyre!` / `wrap_err` kept\nunder the same names, so the call sites read unchanged.\n\nThe binary's `required-features` therefore name only library\ncapabilities, both in `default` — `dict-builder` joins it for `--train`,\nwhich adds `fastrand` and nothing transitive. A `no_std` build turns\ndefault features off and simply does not select the binary target, which\nis what the previous optional-dependency gate was working around.\n\nBinary name stays `structured-zstd`, not `zstd`: `cargo install` writes\ninto a directory that usually precedes `/usr/bin`, so that name would\nshadow the system tool. `argv[0]` dispatch is unaffected — verified an\n`unzstd` symlink to the installed binary still defaults to decompress.\n\nVerified: `cargo install --path zstd` with no flags produces the binary;\n`cargo tree` shows the library's dependencies unchanged at `fastrand` and\n`twox-hash`; the `thumbv7em-none-eabihf` no-std check still passes.\n\n* fix(cli): keep the promises the accepted flags make\n\nReview found the previous round too generous: several flags were taken as\nno-ops although they change what happens, which is worse than refusing\nthem — the caller gets a wrong answer instead of an error.\n\n`--stream-size` and `--size-hint` were one field feeding\n`set_pledged_content_size`, so an estimate that missed by a byte turned a\nworking compression into a failure. They are now separate: the pledge\nreaches the frame header, the hint only sizes the encoder through\n`set_source_size_hint`.\n\n`--pass-through` and `--exclude-compressed` decide which files are\nprocessed and what happens to input that is not compressed. Unimplemented,\nthey now fail rather than silently compressing a file the caller asked to\nskip.\n\n`-M` / `--memory` is a bound on what untrusted input may demand, not a\nhint. This build enforces a fixed 128 MiB decompression window, so a\nrequest at or above it is already satisfied; a tighter one is refused\nwith the ceiling named, instead of leaving the caller believing a bound\nthat is not there. The constant is now public — it is the same question\nany embedder has.\n\n`-f` was documented as the flag that permits output to a terminal, and\nthe help said so, but nothing checked: `-c` into an interactive terminal\nsprayed a compressed frame at the session. There is a guard now, split\ninto a pure function so the decision is testable without a tty.\n\nAlso: attached short-option values (`-T`, `-B`, `-M`) are parsed rather\nthan swallowed, so `-Tinvalid` fails; `--adapt=min=1,max=9` is accepted\nalongside the bare form; and the help lists what is ignored separately\nfrom what is refused.\n\nSix regression tests, each seen failing first.\n\n* fix(cli): verify content checksums, and require the features that back them\n\nReview asked for `ldm` in the binary's required features, since a custom\n`--no-default-features --features std,dict-builder` install builds a tool\nwhose `-t` cannot check anything. Testing that claim turned up something\nworse: the DEFAULT build did not check either.\n\nThe decoder computes the digest but defaults to `EmitOnly`, comparing\nnothing and leaving the decision to the caller. For a library that is the\nright default; for `-t`, whose entire job is to answer \"is this frame\nintact\", it meant answering `OK` for a frame upstream rejects with\n\"Restored data doesn't match checksum\". A tool that reports success\nwithout doing the check is worse than one that refuses to build, so\ndecompression now runs in `Verify` and the required-features list names\nwhat the parser advertises rather than what happens to link.\n\nAlso from this round: `--adapt=` parameters are parsed (`min=`/`max=`\nnumbers), so `--adapt=nim=1` and `--adapt=garbage` fail instead of being\nswallowed; and `-M` is checked against the window PLUS the decoder's\nliteral, block, sequence and table buffers, since the promise is about\ntotal memory — a limit of exactly 128 MiB was one we would have broken.\n\nThree regression tests, each seen failing first; the checksum one via a\nsurgical revert to `EmitOnly`.\n\n* fix(cli): stop losing data, dictionaries and checksums\n\nFive defects found in review, each with a regression test that fails\nwithout its fix:\n\n- `--rm` deleted the source even when the output went to stdout, so\n  `zstd -c --rm f > out` destroyed `f`. Upstream keeps it; we now do too.\n- `-c` and `-o` did not override each other, so the pair took the wrong\n  branch. Each now clears the other, matching last-option-wins upstream.\n- Verified decoding replaced the dictionary constructors with a handle\n  add, which drops a forced dictionary for a frame carrying no dictID.\n  The constructors are back, and the checksum mode is set through a new\n  `StreamingDecoder::decoder_mut`.\n- The default build never compared checksums (`EmitOnly`), so `-t`\n  answered OK on a frame upstream rejects. Decoding now uses `Verify`.\n- `--memory` / `-M` was accepted in `--train` mode, where it bounds\n  nothing; training loads samples rather than decompressing. It is\n  rejected instead of implying a limit it does not enforce.\n\n`--compress-literals` / `--no-compress-literals` moves from the\nsilently-accepted list to the rejected one: it changes the result.\n\n* fix(cli): decode whole streams, honour --long=N, guard the trained dictionary\n\nA zstd stream is a sequence of frames, and `cat a.zst b.zst` is a valid\narchive. The decoder's `Read` ends at the first frame, so `-d` emitted a\nprefix of the input and `-t` answered OK for it; the library walks frames\nonly in `read_to_end`, which buffers the whole stream. Decompression now\nre-initialises the decoder on whatever follows until the source runs out,\nstepping over skippable frames, so peak memory stays O(window).\n\nAlso in this round, each with a test seen failing first:\n\n- `--long=N` validated the window log and dropped it, so the encoder\n  reached back only as far as the level would have. The value now goes\n  through to the compression parameters, and is range-checked at parse\n  time.\n- `--train-cover` / `--train-legacy` ran FastCOVER under another\n  algorithm's name. Refused instead; `--train` and `--train-fastcover`\n  keep the upstream default.\n- `--train -o PATH` truncated an existing file with no `-f` and no\n  temporary, so an interrupted run destroyed the old dictionary (and a\n  sample named as the destination). It now takes the same overwrite gate\n  and atomic rename as every other output.\n- `-l` rejected any archive containing a skippable frame, which is how\n  seekable-zstd stores its index. The walk steps over them and reports\n  the count in a `Skips` column, matching `zstd -l`.\n- Training on a corpus of one repeated byte panicked: the synthetic\n  fallback alphabet was all 256 symbols, and a perfectly flat alphabet\n  that wide has neither an FSE nor a direct weight description. The\n  substitute stops at 128 symbols, and a sampled distribution of the same\n  shape falls back to it rather than reaching the assert.\n\n`-D` with a raw-content dictionary now names what is missing instead of\nsurfacing a magic-number mismatch. Support for those is a separate change:\nit needs the zero-id invariant relaxed on the forced-dictionary path in\nboth the encoder and the decoder.\n\n* feat(dict): accept raw-content dictionaries wherever a dictionary is taken\n\n`zstd -D` accepts any file as a dictionary: a blob without the dictionary\nmagic is used as raw content. That mode was unreachable here because a\nraw-content dictionary has no ID, and a zero ID was refused on every path\nincluding the one where it means nothing.\n\nThe ID matters only where a dictionary is looked up by it. Registration\n(`add_dict` / `add_dict_handle`) keeps refusing a zero ID, since that is\nthe key it stores under. Attaching one explicitly — for encoding, or as\nthe forced dictionary for decoding — no longer asks, because the frame\nidentifies nothing: the header now omits the field for a zero ID rather\nthan writing a zero, which is how RFC 8878 spells \"no dictionary ID\".\n\n`Dictionary::from_serialized_or_raw_content` loads whichever kind a blob\nholds, and `-D` goes through it on both sides. Verified against upstream\nin both directions: our raw-dictionary frames decode with `zstd -dc -D`,\nand frames it produced that way decode here.\n\n* fix(cli): honour memory units, block target and the frame this build can read\n\nSeven defects from review, each with a test seen failing first:\n\n- `-M` multiplied by a mebibyte even when the value carried a suffix, so\n  `-M1M` asked for a terabyte and sailed past the very check the flag\n  exists for. A bare number is MiB, a suffix means what it says, and the\n  long spelling now shares that unit.\n- A raw dictionary still wrote a zero `Dictionary_ID` through the\n  streaming encoder, which the CLI uses. The header omits the field, as\n  it already did for the one-shot compressor.\n- Decoding an empty input reported success, so `-t empty.zst` answered\n  OK for a file holding no frame. End of input before the first frame is\n  now an error; after one, it is the end of the stream.\n- `--target-compressed-block-size` was validated and discarded, leaving\n  the default geometry for a caller who asked for smaller blocks. It\n  reaches the encoder now.\n- `--long=28`..`30` produced frames declaring a window this build's own\n  decoder refuses, so the flag is capped at 27 with the reason named.\n- `-l` added declared content sizes unchecked, and those are declarations\n  rather than measurements: two crafted headers crashed the listing.\n- The docs on `Dictionary::id` and `EncoderDictionary::id` still said a\n  zero ID is rejected.\n\nThe compression entry points took ten positional arguments and were\nabout to take eleven, so they now take one `FrameSettings`.\n\n* fix(cli): keep source permissions, bare --long, and the promises -M makes\n\nSix defects from review, each with a test seen failing first:\n\n- A new output file was created at whatever the umask allowed, so\n  compressing a mode-0600 secret produced a world-readable archive.\n  Upstream applies the source's permissions; so do we now. An existing\n  destination still keeps its own, so nothing this tool writes is more\n  readable than what it was written from or over.\n- Bare `--long` set only the flag, leaving the level's own window, while\n  upstream documents it as `--long=27`. The window is the point of the\n  flag.\n- A named input that is a FIFO, device or socket had its stat length\n  pledged as exact, so `-c input.fifo` failed on the first byte instead\n  of compressing the stream. Only a regular file's length says how much\n  will be read.\n- `-M` was checked against the decoder alone and then discarded, but the\n  `-D` dictionary is held twice over: as read, and again as parsed. The\n  limit is kept and re-checked once that size is known.\n- `--fast=0` negated to level 0, which is the ordinary default rather\n  than a fast one; upstream calls it an incorrect parameter.\n- Stripping `.zst` went through a lossy string, so a path holding\n  non-UTF-8 bytes decompressed under a different, possibly colliding\n  name. The extension is dropped as a path component now.\n\nAn explicit window log is capped by a known source size, as\n`ZSTD_adjustCParams_internal` does upstream. Without it every `--long`\nframe, however small, asked its decoders to reserve 128 MiB: verified\nagainst the reference command, which compresses a 24-byte file with\n`--long=27` into a frame declaring 24 bytes.\n\n* fix(cli): keep --stream-size for unstattable inputs, refuse --long where it is inert\n\nTwo defects from review, each with a test seen failing first:\n\n- The per-input size overwrote `--stream-size`, so the pledge was lost for\n  exactly the inputs that option exists for: a FIFO or device has no\n  reliable length to stat, and the explicit one was then dropped rather\n  than used. It is the fallback now, and a named FIFO records its pledge\n  the way stdin already did.\n- Long-distance matching runs on the optimal parser here, so below level\n  16 `--long` widened the window and never ran the matcher it names.\n  Accepting it there reported success for work that did not happen, so it\n  is refused with the level named. Extending long-distance matching to\n  the other backends is its own change; until then the flag says what it\n  can and cannot do rather than passing quietly.\n\n* fix(cli): read the benchmark range for --long, bind -M to the runs that decode\n\nTwo defects from review, each with a test seen failing first:\n\n- Benchmark mode compresses the levels `-b` and `-e` name, not the one a\n  bare `-N` set, so the `--long` check read the wrong number in both\n  directions: `-b16 --long` was refused, and `-16 -b3 --long` was accepted\n  and then benchmarked a level with no long-distance matcher. It reads the\n  benchmark range now, and takes its lowest level, since the whole range\n  runs with the flag.\n- `-M` was checked while parsing, before the mode was final, so\n  `--memory=8 input` failed a compression that allocates no decoder at\n  all. The value is recorded and weighed once the mode is known, on the\n  runs the ceiling actually describes.\n\nThat second rule replaces the `-M` / `--train` conflict: training\nallocates no decoder either, so the limit says nothing there, and upstream\naccepts it. The flag now follows the house rule for the whole surface —\nkept where it binds, accepted where it describes nothing, refused only\nwhere it would promise something this build cannot keep.\n\n* fix(cli): build on stable Windows\n\nThe hard-link guard read the volume serial and file index, which std\nexposes only on nightly (`windows_by_handle`), so the Windows job failed\nto compile. Nothing available to a dependency-free binary answers that\nquestion on stable, so the guard now answers only where it can: Unix\ncompares device and inode, everywhere else it says no. The caller already\ncompares canonical paths, which catches the same file reached by another\nname — the case that comes up in practice; a Windows hard link goes\nunnoticed, and the function says so.\n\nReproduced and verified against `x86_64-pc-windows-gnu`: check, clippy\nand the test targets all build.\n\n* fix(cli): weigh -M and --long against what a run really does\n\nBenchmarking compresses at every level it measures and decompresses each\nresult, whatever mode was asked for, so the two guards read the wrong\nquestion:\n\n- `-b3 -M8` skipped the memory ceiling, since the mode stayed\n  `Compress`, and then decoded at every level anyway.\n- `-d -b3 --long` skipped the level check, since the mode was\n  `Decompress`, and then compressed at level 3, where no long-distance\n  matcher runs.\n\nBoth now ask what the run does rather than what the mode says, through\n`decodes` / `compresses`.\n\nThe dictionary is also weighed before it is read. A limit is a promise\nabout what the process will allocate, and loading the whole file only to\nreport that it does not fit performs exactly the allocation the caller\nasked to be spared. The size comes from the directory entry, the read is\nbounded by it, and a file that grew in between is an error rather than a\nsilent truncation.\n\n* fix(cli): cap dictionary-frame windows, size -T as a count, keep lengths in u64\n\nFour defects from review, each with a test seen failing first:\n\n- A dictionary frame kept the requested window log untouched: the branch\n  that preserves the CDict's search geometry skipped the source-size cap\n  the dictionary-free path applies, so `-D` with `--long=27` on a tiny\n  file still declared 128 MiB. The window half of\n  `ZSTD_adjustCParams_internal` is its own function now, applied to both\n  paths, while the dictionary's search shape stays untouched.\n- `-T4K` was accepted, because the attached value went through the size\n  parser it shares with `-B`. A thread count is a count, the way\n  `--threads=` already reads it.\n- `--train` read every sample and ran FastCOVER before noticing it could\n  not write the result. The destination is settled first, so a command\n  that is going to be refused is refused before the corpus is loaded.\n- A file's length was narrowed to `usize`, putting a 4 GiB ceiling on\n  32-bit targets that the work does not have: both directions stream, so\n  a file only has to fit the window. The progress counter carries `u64`\n  and converts only the per-read amounts.\n\n* fix(cli): count the benchmark's own buffers against the memory limit\n\nBenchmarking is the one path that holds whole files rather than streaming\nthem: the inputs concatenated, and a decompressed copy of them per pass.\nThose dwarf the decoder's workspace, so a ceiling that weighed only the\nwindow was kept in the small and broken in the large — `-b16 -M256` on a\n512 MiB file passed and then held over a gigabyte. The sizes come from the\ndirectory entries and are weighed before a byte is read, the way the `-D`\ndictionary already was; the parameter that carries them is named for what\nit is rather than for the one caller it used to have.\n\nTwo other findings in the same review were checked against zstd 1.5.7 and\ndeclined, with the reason recorded where the code makes the choice:\n\n- A bare `-M` sets no ceiling there too. The value is attached or it is\n  nothing, and the next argument stays a filename, so erroring would\n  refuse a command line the reference tool accepts.\n- `Frames` counts every frame and `Skips` says how many were skippable:\n  the reference prints `Frames 3, Skips 1` for two data frames around one\n  metadata frame, which is what we print.\n\n* fix(cli): read argv as bytes, honour -S, and keep the dictionary's own size\n\nFive defects from review, each with a test seen failing first:\n\n- A filename that is not UTF-8 panicked before the parser ran, so the\n  byte-preserving path handling could never be reached. Arguments are read\n  with `args_os`, matched through a lossy view, and kept as paths wherever\n  they name a file.\n- A serialized `-D` dictionary was parsed and handed over as content, which\n  keys the compression-parameter tier on the wrong length: the entropy\n  tables in between can put content and blob on opposite sides of a\n  boundary, and `-D` then compressed differently from the same dictionary\n  given to the library. `EncoderDictionary::from_serialized_or_raw_content`\n  loads either kind and keeps the blob's length.\n- `-S` was accepted and ignored, so several inputs were measured as one\n  concatenated stream and the reported ratio described neither file. Each\n  input is measured on its own now, as the reference does; only one is in\n  memory at a time, which the memory ceiling accounts for.\n- The dictionary and the benchmark's buffers were weighed separately, so\n  two allocations that each cleared the ceiling could exceed it together.\n- The `--ultra` gate read `-N` while benchmarking compresses the range\n  `-b`/`-e` name, so `-b20` ran an ultra level without the flag.\n\n* fix(cli): finish progress on the reader, not the stat, and count every buffer\n\nFour defects from review, each with a test seen failing first:\n\n- Benchmarking holds the input, the frame it compresses to and the\n  decompressed copy at once, but the ceiling counted two of the three. On\n  incompressible input the frame is no smaller than the input, so a limit\n  sized for two buffers was exceeded by a third. `check_memory_limit` now\n  takes how many forms are held rather than assuming two.\n- A size here is what a directory entry claims, not what was allocated, so\n  a sparse file's apparent length is a real input to that arithmetic: it\n  panicked with overflow checks on, or wrapped into a number that accepted\n  a limit nothing could keep. Checked throughout, with the overflow\n  reported as the refusal it is.\n- `-b` claimed to need regular files but only rejected stdin. A FIFO\n  reached the whole-file read and blocked there — the regression test hung\n  for 142 seconds before the guard — and a character device would have\n  grown the buffer until the allocator gave up. File types are settled\n  before anything is opened.\n- The progress summary was printed when the byte count met the total from\n  the directory entry. A FIFO reports zero and a file can shrink after it\n  was measured, so the bar kept redrawing and the summary never came. The\n  end of the work is the reader saying it has no more, and what it read is\n  what gets reported.\n\n* fix(cli): keep attached paths as bytes, refuse unseekable listings, check both range ends\n\nThree defects from review, each with a test seen failing first:\n\n- A path attached to its option — `-Dname`, `-oname`, `--use-dict=name` —\n  was taken from the lossy view the option spelling is matched against, so\n  a name that is not UTF-8 aimed the command at a replacement-character\n  file. It comes off the original argument now, split after the option's\n  ASCII prefix.\n- `--list` rejected only stdin, so a FIFO reached `File::open` and blocked\n  there waiting for a writer — the regression test hung for a minute\n  before the guard. File types are settled for every input before any is\n  opened, the way `-b` already does it.\n- A benchmark range was validated only at its highest level, so\n  `-b-200000 file` passed parsing and then stat'd and read every input\n  before the first pass refused it. Both ends are checked where the range\n  is settled.\n\n* fix(cli): stop an output landing on another input, and four smaller defects\n\nFive defects from review, each with a test seen failing first:\n\n- Inputs are processed one after another, so an output derived from an\n  early one could land on a file still waiting its turn: `-f foo foo.zst`\n  replaced `foo.zst` before it was ever read, and the original was gone.\n  `-f` permits overwriting the output, not destroying another input, so\n  the whole list is checked before the first byte is written. The\n  reference command destroys that file; refusing is the deliberate\n  difference.\n- `Read::read` answers `Ok(0)` for an empty buffer as well as for the end\n  of the stream. Taking the first as the second finished the progress\n  monitor before any bytes had moved, and the summary for the work that\n  followed never came.\n- `-b` set a switch nothing cleared, so `-b3 --list` benchmarked the file\n  the caller had just asked to list. Every operation flag now replaces the\n  previous choice, benchmarking included.\n- `-D` was read before the mode was dispatched, so `--list -D missing`\n  failed over a file neither listing nor training would have opened, and a\n  large one cost time and memory nothing looked at.\n- `--dictID=0` is how the dictionary API spells \"choose one for me\"\n  (`c-api/include/zdict.h`: \"force dictID value; 0 means auto mode\"), but\n  it was carried through as an explicit id the trainer then refused.\n\n* fix(cli): stop training and compression writing over files they read\n\nSix defects from review, each with a test seen failing first. Two destroy\ndata:\n\n- `--train -f dictionary` read the sample named `dictionary` and then\n  replaced it with what it had learned from it — the default destination\n  and a sample can carry the same name, and training was exempt from the\n  guard that covers ordinary outputs.\n- `-f -D data.zst data` wrote the compressed output over the dictionary it\n  had just loaded, deleting the file needed to read that output back. The\n  `-D` path is a resource this run depends on, so it is checked with the\n  inputs.\n\nThree options carry zero as a sentinel, and each said so in the parameter\ncontract this tool mirrors:\n\n- `--target-compressed-block-size=0` means no target, but the zero reached\n  a setter that clamps to the smallest block the format allows, cutting a\n  large input into ~1.3 KiB blocks.\n- `--size-hint=0` is not a hint, but it was recorded as one and sized the\n  encoder for an empty source, shrinking the window a real stream needs.\n- A zero-length `-D` file returns to no-dictionary mode; it was reported as\n  a dictionary too small to parse, failing before any input was read.\n\nAnd one allocation: training concatenated every sample and then handed the\nresult to a reader-based entry point that buffered it a second time, so the\ncorpus — the largest thing the run holds — existed twice throughout.\n`create_fastcover_dict_from_slice` does the same work from bytes already in\nhand, and the reader path now calls it after its own read.\n\n* fix(cli): compare output collisions by file, not by spelling\n\nTwo defects from review, each with a test seen failing first:\n\n- The guard that keeps an output off another input compared paths as\n  strings, so `-f foo ./foo.zst` walked straight past it and destroyed the\n  archive: one file, two spellings. Both sides are now resolved to a\n  canonical directory plus a file name, which collapses `.`, `..` and\n  symlinked components. The output usually does not exist yet, which is\n  why its directory rather than the file itself is what gets resolved.\n- Training read every sample whole without checking what it was reading. A\n  FIFO blocked there — the regression test hung for 57 seconds before the\n  guard — and a character device would have grown the corpus until the\n  allocator gave up. Samples take the same regular-file gate benchmarking\n  and listing already have.\n\n* fix(cli): guard training samples by file identity, end progress on EOF\n\nTwo defects from review, each with a test seen failing first:\n\n- The guard keeping the trained dictionary off its own samples compared\n  paths, and a path comparison collapses `.` but not `..` or a symlink.\n  `--train -f -o sub/../dictionary dictionary` walked past it: the run read\n  the 40 KB sample and replaced it with the 105-byte dictionary learned\n  from it. Samples are now compared to the output as files, the way the\n  compress-side scan already does, with the hard-link case covered where\n  the output exists.\n- The progress monitor finished when its byte count reached the total\n  taken from the directory entry. A file that grew after it was measured\n  therefore got its summary printed while the rest was still being read,\n  and those bytes went unreported. The reader saying it has no more is now\n  the only end; every caller here performs that read to find the end\n  anyway, so nothing waits longer than it did.\n\n* fix(cli): keep the output-collision scan out of test mode\n\n`structured-zstd -t archive.zst` panicked before decoding a byte. The scan\nthat keeps a derived output off another input asks each input what it would\nproduce, and testing produces nothing: it decodes into a sink and names no\ndestination, so the question reaches an arm that says as much and aborts.\nWidening that scan to a single input is what exposed it, since a lone `-t`\nargument had previously skipped the loop entirely.\n\nThe scan now runs for the two modes that write a file. Carries a regression\ntest that runs `-t` over a sound archive, seen failing on the panic.\n\n* fix(cli): require a regular file for -D, carry rounded seconds\n\nTwo defects from review, each with a test seen failing first:\n\n- The dictionary's size comes from its directory entry, and only a regular\n  file's says how many bytes there are to read. `-D fifo` reported zero,\n  cleared any memory limit with it, and then blocked in `File::open` until\n  someone opened the other end: the run stopped with nothing said about\n  what it was waiting for. The regression test hung the same way before the\n  check and now finishes in 13 ms. `-b`, `-l` and `--train` already gated\n  their inputs this way.\n- The summary's seconds are shown rounded, and rounding can reach a full\n  minute: 119.5 s printed as `1m 60s`. The components are normalized before\n  any is written, at the precision the value will actually be shown with, so\n  59.5 s keeps its decimal while 59.96 s becomes a minute and 59m 59.6s an\n  hour.\n\n* fix(cli): parse the dictionary once, guard three ways it was misused\n\nFour defects from review, each with a test seen failing first except the\nfirst, which changes no result:\n\n- The `-D` blob was parsed per frame on the compressing side and per\n  stream on the decoding one, which under `-b` meant once per timed\n  iteration: for a large dictionary over a small input, most of the\n  reported MB/s was the dictionary's own setup. It is parsed once per run\n  now, into whichever of the two forms that run uses, and the benchmark\n  prepares it past its own memory ceiling so the check still comes first.\n- `--rm` deleted an input that was also the `-D` dictionary: `--rm -D data\n  data` produced an archive and then removed the only copy of the bytes\n  needed to read it back. Refused by the same file-identity check that\n  already keeps an output off the dictionary.\n- `-c` and `-o` clear one another, so `--train -o wanted.dict -c` left no\n  destination and the default `dictionary` stood in: the run wrote a file\n  nobody named, and with `-f` over whatever was there. The reference\n  command fails on the pair too, so it is refused rather than redirected.\n- `--maxdict=1` cannot produce a dictionary, but nothing said so until\n  every sample had been read and concatenated. The floor a dictionary\n  cannot go below is now a documented constant on the library, checked\n  before the first sample is opened.\n\n* fix(cli): resolve links in the alias guards, keep hours, name no dictionary\n\nThree defects from review, each with a test seen failing first:\n\n- The guards resolved a path's directory but left its last component as\n  written, so a link to the dictionary read as a different file. `--rm -f\n  -D dict-link data` compressed 20 KB against itself, deleted `data`, and\n  left a dangling link beside a 24-byte archive nothing could ever open.\n  A path that exists is now resolved whole, and the removal guard asks the\n  filesystem for identity as well, since a hard link is a second name that\n  no amount of resolving tells apart.\n- Hours wrapped at 60, so a two-and-a-half-day run printed an empty\n  summary: no hours, no minutes, no seconds. They are the largest unit\n  here, so they count on.\n- The `DictID` column holds one id and reported the first frame's, which\n  for an archive built from several dictionaries names one that decodes\n  only its start — and reads as \"no dictionary needed\" when the first\n  frame uses none. Frames now have to agree, or the column says so and\n  shows none, which is how the reference tool answers the same file. The\n  walk was split from the printing so what a row claims can be tested.\n\nThe neighbouring `Check` column was raised too: it ORs across frames, so a\nmixed archive reports XXH64. That is what zstd 1.5.7 prints for the same\nfile in either order, reporting None only when no frame carries one, so\nthe column stays as it is and the reasoning sits beside it.\n\n* fix(cli): guard -o against the dictionary, keep trained dictionaries private\n\nThree defects from review, each with a test seen failing first:\n\n- The scan that keeps an output off the `-D` dictionary walks the named\n  inputs, and stdin is not one of them: `-f -D dict -o dict` reading stdin\n  loaded a 30 KB dictionary and replaced it with a 33-byte frame that\n  needs it. A destination named outright belongs to the whole run, so it\n  is checked before the input shape is looked at — which covers stdin, an\n  explicit `-`, and files alike.\n- A trained dictionary was left at whatever the umask allowed, so a 0600\n  corpus produced a 0644 dictionary. It holds stretches of that corpus\n  verbatim (62 bytes at a stretch even on random samples), which is the\n  same exposure the archive rule already prevents. With several samples\n  the strictest decides, since bytes of each are in there. Unix only:\n  elsewhere `Permissions` answers whether a file is read-only, a different\n  question that would make the dictionary unwritable rather than unread-\n  able.\n- A dictionary frame with an explicit window takes the source cap without\n  the floor that travels with it in the full adjuster, so a hint of a few\n  dozen bytes configured a 128-byte window where the format's smallest is\n  1 KiB, and a stream outgrowing the hint was matched through it.\n\n* fix(decode): refuse resume across dictionaries that carry no ID\n\nThree defects from review, each with a test seen failing first:\n\n- The resume guard tells dictionaries apart by the ID the decoder\n  recorded, and a raw-content dictionary has none: they are all ID 0. A\n  snapshot captured under one raw dictionary was accepted under another\n  and restored foreign entropy and repcode state, which the test showed\n  producing output rather than an error. Both sides refuse now — no such\n  snapshot is emitted, and none is accepted — with an error that says\n  what to do about it. Content hashing was rejected as the alternative:\n  it would cost a pass over the dictionary per frame, which is the whole\n  point of preparing one, and an address identity can repeat after a\n  free.\n- Replacing a file restored the destination's own mode over the one the\n  content asked for, so a `-f` retrain over a world-readable dictionary\n  published a private corpus, and compressing a private file over an\n  existing archive did the same. The source's mode decides in both\n  directions now, which is what the reference command does; only a source\n  with no mode of its own, stdin, leaves the destination's alone.\n- The dictionary-output guard also refused `-d -D dict -o dict`, where\n  the dictionary has already done its work and the plaintext never needed\n  it. The reference command allows that, so both dictionary guards are\n  compression-only now.\n\n* fix(cli): size benchmark buffers to their budget, name the group that may read\n\nTwo defects from review, each with a test seen failing first:\n\n- The `-M` ceiling for `-b` counts three copies of the input, but the\n  buffers holding them grew by doubling: two files of 8000 and 1000 bytes\n  left a 16000-byte buffer for 9000 bytes of data, and each file was read\n  into its own buffer before being appended, holding the largest twice.\n  A run admitted as fitting could take far more than it was allowed. The\n  input is now read into one buffer sized from the file lengths, and the\n  frame and decoded copy are allocated once at `compress_bound` and the\n  known decompressed length rather than inside the timed loops — which\n  also takes buffer growth out of the speeds the benchmark reports. The\n  ceiling names those three sizes instead of assuming the frame equals\n  the input.\n- Permission bits do not say who they admit. Two samples at `0640` can\n  belong to different groups, and the dictionary belongs to whichever\n  group the directory it was created in gave it, so keeping the group\n  bits would open corpus fragments to a group that could not read the\n  sample they came from. They survive only when every sample names one\n  group and the dictionary is in it; the owner's bits stand, since every\n  sample was read, and the world's name no one in particular.\n\n* perf(encode): share a prepared dictionary with its clones\n\nA prepared dictionary is attached to a compressor by value, and it owned\nits content and parsed entropy tables outright, so every frame it primed\ncopied the whole thing — on exactly the path where one dictionary is\nprepared once in order to serve many small frames. It now holds the same\nshared dictionary the decoder side already used, so a clone is a handle.\nEvery use was a read, so nothing else changed: frames come out\nbyte-identical at levels 1, 3, 9 and 19 against a build from before.\n\nThat was the largest of the copies a benchmark held while claiming to\nhold one. The rest are accounted for now: the blob a dictionary is parsed\nout of is released once the parsed forms exist rather than kept beside\nthem for the run, and `-M` counts what stands at the peak — the blob and\nthe two forms being built from it — instead of a single copy. A run with\n`-b -D -M` was admitted as holding one dictionary while it held four.\n\n* fix(cli): refuse resume up front, keep -M0 and the stdin marker honest\n\nFour defects from review, each with a test seen failing first:\n\n- The refusal of a resume under a dictionary with no ID was checked on the\n  emitting side only after the decode loop had run, so a call consumed its\n  input and advanced the decoder and then returned an error, discarding the\n  output it had produced and leaving nothing the caller could retry from.\n  The function's own contract says resume errors are rejected up front, and\n  the answer does not depend on the blocks: both directions are refused\n  before a block is read or a buffer reserved. The test asserts the source\n  is untouched.\n- Permission bits on an archive had the same blind spot the trainer's did\n  until last round: a `0640` source in one group compressed into a setgid\n  directory of another produced an archive its own group could read while\n  the source's could not. One source is the same question as many samples,\n  so the rule is now one function for both, and the trainer's is renamed\n  after what it computes rather than who calls it.\n- `-M0` was stored as a limit of zero bytes and refused every run. Zero is\n  the parameter's own sentinel for the default ceiling (`zstd.h`,\n  `ZSTD_d_windowLogMax`: \"value 0 means use default maximum windowLog\"),\n  which is what this build enforces anyway, and the reference command\n  accepts it. It now records no custom limit; a genuinely tight `-M8` is\n  still refused.\n- `-b -` stat'ed and benchmarked a file named `-` when one happened to sit\n  in the working directory, and meant stdin everywhere else in the same\n  command line. The marker is refused, and the message names `./-` for the\n  file it shadows.\n\n* fix(cli): count the encoder in -M, refuse the stdin marker when listing\n\nTwo defects from review, each with a test seen failing first:\n\n- The `-M` ceiling for `-b` counted the input, the frame, the decoded copy\n  and a fixed decoder allowance, but not the match finder every\n  compression pass builds — the largest thing in the run at the higher\n  levels. `--ultra -b22` on 1 MiB was admitted under a 132 MiB limit while\n  holding some 21 MiB of buffers and tables; it now reports that figure\n  and refuses, and runs under a limit that covers it. The workspace is\n  sized by the level and by the source, since both cap the window and the\n  tables, so the estimator gained a form that takes the source size the\n  benchmark already knows — the existing one reports the arbitrary-stream\n  figure and would have refused small inputs that fit.\n- `--list -` stat'ed and listed a file named `-` when one sat in the\n  working directory, and meant stdin everywhere else in the same command\n  line. The benchmark path was given this check last round; the listing\n  walk needs it for the same reason, since it seeks between frame headers\n  and cannot read a stream. `./-` still names the file.\n\nThe four memory-ceiling tests spelled the budget out inline, which is how\nthey came to disagree with it. They share one helper now, written the way\nthe run computes it.\n\n* fix(cli): budget what --long adds, describe the file actually opened\n\nFour defects from review:\n\n- The `-M` ceiling weighed the level's own preset, and `--long` is not a\n  preset: it widens the window the frame keeps and adds a matcher with a\n  hash table of its own. `--ultra -b16 --long=27` on 32 MiB asked for the\n  preset's 132 MiB while holding 168. The estimator gained a form that\n  takes the effective window and whether long-distance matching is on, and\n  the LDM table is budgeted beside its own allocation so the two evolve\n  together. Regression test carries both the estimate and the refusal.\n- Training took its samples' permissions from their paths after the corpus\n  was built, so a sample replaced or chmodded during a long run could\n  grant its mode to bytes that came from a different file. Each sample is\n  opened once now, and the mode and group are the handle's, read from the\n  same open file as the bytes.\n- The `-D` dictionary's type and size came from its path and the bytes\n  from a later open, so the two need not describe the same file. Both are\n  re-asked of the opened handle. The path is still asked first, because\n  that is what refuses a FIFO without waiting for a writer and what lets\n  the ceiling be weighed before a descriptor is spent; a swap to a FIFO\n  inside the remaining window still blocks in the open, which would need a\n  non-blocking open and a platform flag this crate has no dependency to\n  name.\n- That size then bounded the read through a cast that truncates on a\n  32-bit target, where 4 GiB becomes a capacity of zero and the read grows\n  into an allocation that aborts instead of refusing. It is converted with\n  `usize::try_from` and refused when it does not fit. No test: the failure\n  needs a 32-bit target, which CI compiles but does not run.\n\n* fix(dictionary): compute the statistics stride in 64 bits\n\nHuffman statistics are gathered by striding across the corpus, and the\nstride was `i * len`: a product that reaches 2^48 for an addressable\ncorpus and so cannot be held by a 32-bit `usize`. Any corpus past 64 KiB\noverflowed there and the multiply panicked instead of sampling —\n`attempt to multiply with overflow` in the cross-compiled i686 job, from\na training test whose two samples add up to 80 KB. Both sampling loops\nnow index through one helper that does the arithmetic in 64 bits; the\nquotient is always below the corpus length, so narrowing back is exact.\n\nThe failure also corrects the previous commit's note that a 32-bit\ntarget is compiled but not run: it runs the whole suite. The dictionary\nsize that no 32-bit machine can address therefore does get a test after\nall, which asserts nothing on a 64-bit host and everything on that job.\n\n* fix(encode): size a dictionary frame's window by its source alone\n\nFour findings from review, three fixed and one refuted by measurement —\nwhich turned up a divergence of its own:\n\n- The window a dictionary frame declares counted the dictionary's size\n  along with the source, so `--ultra -22 --long=27` on a 2 KiB file with a\n  256 KiB dictionary declared 512 KiB where the reference command declares\n  2 KiB. The dictionary is reachable regardless: the format lets sequences\n  point into it at offsets beyond the window while the output so far is\n  within it, which is why the reference declares the same window for a\n  4 KiB dictionary and a 256 KiB one. Counting it only made every decoder\n  of our frames reserve up to 256x what it needs. Verified both ways: the\n  reference decodes our frame and we decode its.\n- The review asked for the opposite — that an explicit window survive the\n  source cap on dictionary frames. Measured against the reference: it caps\n  too, and harder than we did (2 KiB for that same input). The cap stays,\n  and why it stays is now written where the code is.\n- The workspace estimate answered for any window log it was handed,\n  including ones no encoder accepts, and `1 << 64` is undefined rather\n  than large. It is bounded to the maximum the public parameter API takes\n  before anything shifts by it.\n- That estimate also wrapped on a 32-bit target, where the widest\n  long-distance table alone outgrows a `usize`. Every shift, product and\n  sum saturates now: a figure that wraps understates the memory it exists\n  to bound, telling a caller a run fits when it cannot.\n- A serialized dictionary parsed for the encoder built the decoder's\n  lookup tables, which the encoder never reads. It takes the encoder-only\n  parser, which produces an identical frame without them.\n\n* fix(cli): keep set-user-ID bits out of output, bound the benchmark read\n\nThree findings from review, each with the test that fails without the fix:\n\n- An output file inherited the whole mode of its sources, set-user-ID and\n  set-group-ID included. Those bits do not say who may read a file; they\n  say who a program runs as. Carrying them from a compressed input onto a\n  file this tool writes hands a caller a way to run code as somebody else.\n  The output now keeps only the read, write and execute bits, which is\n  what the mode was being copied for.\n- `-b` weighed each input against the memory ceiling with the length its\n  directory entry reported, then read it with no bound at all. A file that\n  grew between the two, or one whose reported length was never a byte\n  count, filled a buffer nobody had approved. The read now stops one byte\n  past the figure it was weighed against and says the file changed.\n- `--train` accepted `-` among its samples and then opened a file by that\n  name, or failed obscurely if none existed. Training reads each sample\n  more than once, which a stream cannot do, so it now says so and points\n  at `./-` for a file actually called that.\n\n* fix(cli): weigh a benchmark's match finder against its dictionary\n\nA dictionary does not merely sit beside the match finder, it decides how\nbig the match finder is. Past the size at which a dictionary stops being\nsearched in place, the frame runs the dictionary's own table geometry\nrather than the source's, so the tables are sized for the dictionary and\nnot for the file being measured. `-b` weighed them on the file alone: at\nlevel 5, 64 KiB of input against a 256 KiB dictionary asks for 3.4 MiB\nwhere the file alone asks for 1.2 MiB, and a 64 MiB dictionary at level 22\nasks for 672 MiB where the file alone asks for 2 MiB. An explicit `-M`\ntherefore passed on runs that went on to take hundreds of times what it\nallowed.\n\nThe estimate now answers for the dictionary as well as for the window and\nthe long-distance matcher, resolving the same dictionary-aware parameters\nthe frame will run. The benchmark weighs it from the blob's own length,\nwhich is what those parameters are chosen by: the content of a trained\ndictionary is smaller than the blob it arrived in, and overstating it can\nonly pick the larger of the two geometries, which is the safe direction\nfor a ceiling.\n\nCarries the regression test that fails without it: a level-5 benchmark of\n64 KiB against a 256 KiB dictionary, under a ceiling holding every buffer\nand all three copies of the dictionary but only the file's own tables, was\naccepted before and is refused now.",
          "timestamp": "2026-09-03T19:44:09+03:00",
          "tree_id": "cb2878187813c5bab16b01c74033f80532bdfc72",
          "url": "https://github.com/structured-world/structured-zstd/commit/2a8676d95c8dcbd88d55a7cad6d97e90e93f7cfc"
        },
        "date": 1788456668440,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.068,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.067,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 181.418,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 172.571,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.842,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.368,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.398,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.692,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.438,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.702,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.131,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.132,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 8.927,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.599,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.105,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.145,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.201,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.903,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.331,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.973,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.02,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.13,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "8e99f1b2bc38ce74d50c271e1c14c0af8bb4e2aa",
          "message": "perf(encode): take the matcher's buffers once per frame, not by growth (#482)\n\n* perf(encode): take the matcher's buffers once per frame, not by growth\n\nA fresh compressor took its buffers by growing into them, and since it\nstarts empty, every frame climbed the same doubling ladder and handed the\npages back at the end of it. The reference carves one workspace per\ncontext, so its pages survive the frame; ours were re-faulted by the\nkernel each time.\n\nTwo places did it. The buffer blocks are read into was sized up front only\nwhen the caller's size was exact, which no reader-fed frame ever is; it is\nnow sized from the hint on every path, and the Fast matcher, which had no\nsizing hook at all, gained one. The row finder held its positions, slot\ncursors and hash tags as three vectors of three different size classes;\nthey now share one buffer, the cursors and tags packed into the byte tail\nof the positions, with their lengths carried as fields so the search reads\nthem without recomputing a shift per candidate.\n\nMeasured on the i9 over a 1 MB frame with a fresh compressor per frame.\nFaults are a slope over frame count, so start-up is not counted as\nper-frame cost:\n\n  level 3    601 faults per frame  ->  0\n  level 5  1,025                   ->  0\n  level 8    0.04                  ->  0\n  level 13   0.37                  ->  0\n  level 16   0.06                  ->  0\n  level 19   0                     ->  0\n  level 22   0                     ->  0\n\nThe reference pays 0.02 on the same shape. Level 5 was re-faulting about\n4 MB per frame without saturation: 103,099 faults over 100 frames against\nthe reference's 924. Cycles fall 5.0% at level 3 and 4.6% at level 5, and\nthe reused-compressor shape is unchanged.\n\nThe first form of the row carve read its regions through bounds-checked\naccessors that recomputed their widths, which cost 9-13% at level 5 —\nmore than the churn it removed. Carrying the widths as fields turned that\ninto the 4.6% gain above; a shared buffer is only cheaper if reaching into\nit is as cheap as reaching into a vector.\n\nLevels 1 and 2 still re-fault on the streaming path, where the dominant\nper-frame allocation is the buffer blocks accumulate into before the\nframe header can be written. That belongs to output sizing, whose growth\nis a deliberate trade for peak memory, not to the match finder, and it is\ntracked in #481. The one-shot path those levels take when a caller\ncompresses a slice is already flat, and below the reference in absolute\nterms.\n\nCarries the regression test that fails without it: a 700,000-byte frame\nleaves the ingest buffer at exactly 1,048,576 when it grew into it, a\npower of two no reservation lands on, checked on one level per backend.\n\nOutput is byte-identical across levels 1-22, compared as md5 of the frames\nthemselves rather than their lengths.\n\nCloses #478\n\n* perf(encode): probe the guard restoration\n\n* fix(encode): bound an advisory hint's reservation by the level's window\n\n* fix(encode): bound an advisory hint, keep the row tail out of the rebase\n\n* test(encode): add the rebase tests the row carve is proven by\n\n* perf(encode): leave the matcher unsized for raw frames",
          "timestamp": "2026-09-03T23:31:03+03:00",
          "tree_id": "23fb0311b9e29901239d4eaaa99777abbbdab947",
          "url": "https://github.com/structured-world/structured-zstd/commit/8e99f1b2bc38ce74d50c271e1c14c0af8bb4e2aa"
        },
        "date": 1788470076722,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.082,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.109,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 201.424,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 245.359,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.582,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.242,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.771,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.983,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.803,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 2.011,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.027,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.157,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.006,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.005,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 8.964,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 4.626,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.105,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.146,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.001,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.206,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 0.893,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.343,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 0.968,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.133,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.021,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.13,
            "unit": "ms"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mail@polaz.com",
            "name": "Dmitry Prudnikov",
            "username": "polaz"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "45685abca498a46e6d019b9cbddf3aa055a1530c",
          "message": "fix(encode): search blocks that only look incompressible, and trim the dfast scan (#483)\n\n* perf(encode): reuse the huffman weight builder's buffers across blocks\n\n* perf(encode): recycle the discarded huffman table into the next build\n\n* perf(encode): recycle the emitter's discarded huffman table too\n\n* perf(encode): keep every displaced huffman table, not just the first\n\n* perf(encode): hold the FSE next-state table inline\n\n* perf(encode): build huffman weights on the stack, not a fresh vector\n\n* refactor(huff0): keep the owning weights helper for tests only\n\n* perf(huff0): hold the code tables inline, as the reference does\n\n* perf(huff0): pass the code tables by pointer, not by copy\n\n* perf(huff0): re-fill a finished table instead of zeroing a new one\n\n* revert(huff0): drop the inline-code-table experiment\n\nHolding the Huffman code tables inline — the reference's shape — measured\nas a consistent 3-5% cost at level 3 across three paired runs on the i9,\nfor 16 fewer allocations per frame out of 269. The profile attributes it\nto no single symbol: the delta spreads across matcher functions the change\nnever touched, which reads as code layout rather than new work.\n\nThat shape pays for the reference because its tables live in block states\nwhich persist and are re-initialised only up to the alphabet in hand.\nReproducing both halves here — inline arrays, then a pointer handoff, then\nre-filling a finished table instead of zeroing a new one — still cost on\nevery measurement, so the experiment stops at a recorded result rather\nthan a merged regression. The commits stay reachable on\nexp/481-c-shape-negative.\n\nThis restores the state measured at 962 -> 269 allocations per frame with\nno cycle change.\n\n* fix(encode): count the weight scratch and keep it across split probes\n\nThree defects the scratch introduced, each with the test that fails\nwithout the fix where one can exist:\n\n- The no-std weight paths called a helper that had been narrowed to\n  tests, so a build without `std` did not compile at all. They take the\n  same stack buffer the std paths do.\n- `heap_size` did not count the scratch, on either the frame compressor\n  or the streaming one. It is kept between blocks and frames by design,\n  so a reused context reported less than it holds — including through\n  the C API's `ZSTD_sizeof_CCtx`, where a caller budgeting around a\n  context is told a figure it cannot rely on. The regression test takes\n  the scratch away and asserts the reported total falls by exactly what\n  it held; comparing against a pre-frame total would not catch it, since\n  the matcher's tables grow over the same frame and would hide it.\n- The block splitter's estimator built its own scratch per post-split\n  block and dropped it at the end, so that path went on allocating the\n  buffers this change exists to reuse. It borrows the compressor's and\n  hands it back, which also leaves the emitter that follows using the\n  buffers the probe already warmed.\n\n* perf(encode): resolve the complementary insert coordinates once, not per slot\n\nUpstream writes the four post-match insertions as four inline hash-and-store\npairs against coordinates the block already holds. Ours routed each through\nthe same insertion helper, so every one of the four re-derived the scan\nsource, re-ran the rebase guard, rebuilt a bounds-checked slice and assembled\nits hash key byte-wise through `try_into().unwrap()`.\n\nThat put the two wrappers at 7.6% of a level-3 encode on the bench host,\nagainst nothing separable on the reference profile, which inlines the same\nwork. They are one function now: the source pointer, the rebase check and the\nslot bias are resolved once for all four, and the keys come from unaligned\nloads in the form the fast loop already builds them in.\n\nThe rebase check moves to the furthest of the three positions. It is monotone\nin its argument, so a base with room for that one has room for the nearer two,\nand all four then pack against a single base rather than possibly straddling\none.\n\nOutput is byte-identical across levels 1-19 on z000033, and the const-generic\ntable mask the asymmetric wrappers needed is gone with them.\n\n* perf(encode): gate dfast candidates in slot space, as upstream does\n\nUpstream admits a candidate with one unsigned compare against\n`prefixLowestIndex` (`zstd_double_fast.c:213`), which answers \"populated\"\nand \"in window\" together. Ours asked five questions per candidate: compare\nagainst the empty sentinel, subtract the slot bias, add the rebase base, then\ntwo compares in position space, and it decoded every candidate that way even\nthough the great majority are rejected.\n\nCarrying the window floor in slot space instead collapses that to the single\ncompare. The empty sentinel is zero and the floor is at least one, so\nemptiness comes for free, and the decode into position space now happens only\nfor candidates that survive.\n\nThe upper bound goes with it. Every slot the loop reads names a strictly\nearlier position by construction: the short index is loaded before this\niteration writes its own, and the long one is carried from the previous\niteration. That is a `debug_assert` now, which the suite exercises at every\nlevel without firing.\n\nAn owned window's floor is fixed for the whole block, so it is hoisted out of\nthe loop; only a borrowed window wider than its advertised size still moves\nper position.\n\nOutput is byte-identical across levels 1-19 on two fixtures.\n\n* perf(encode): drop the dfast candidate upper-bound compare, keep the floor\n\nSplits the previous commit's two halves after measuring them apart.\n\nThe upper bound goes. Every slot the loop reads names a strictly earlier\nposition by construction: the short index is loaded before this iteration\nwrites its own, and the long one is carried from the previous iteration. It is\na `debug_assert` now, which the suite exercises at every level without firing.\n\nThe slot-space fusion comes back out. Answering \"populated\" and \"in window\"\nwith one unsigned compare, as upstream does against `prefixLowestIndex`,\nrequires the window floor to stay live in a register across a loop that already\nspills 31 distinct slots; the emptiness test against the zero sentinel needs no\nregister. Measured on a flat control arm it removed 0.75% of the instructions\nand added 1.0-1.5% to the cycles, so the sentinel compare stays and the reason\nis recorded where the fusion would go.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): hoist the dfast rep offset out of the search loop\n\nIt was the only field read left inside the loop, and it cannot change while\nthat loop runs: `offset_hist` is written by `emit_candidate`, which the outer\narm reaches only after a match has broken out. Left in place it was reloaded\nonce per scanned position, because the table stores go through raw pointers\nderived from the same receiver and nothing lets the optimizer prove they miss\nthis field.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): sink the dfast ip1 coordinates into the paths that read them\n\n`abs_ip1` and its window floor were computed on every scanned position, and\nevery one of their readers is a match path. Holding two more values live across\na body that already spills 31 distinct slots costs more than recomputing them\nwhere they are read, which is the trade the literal lengths beside them were\nalready subject to.\n\nThe rep offset goes back to a per-position read for the same reason, with the\nmeasurement recorded beside it: hoisting it above the loop removed the reload\nand cost 1.9% in cycles on a flat control arm.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): restore the dfast ip1 bindings, keep what measuring them taught\n\nBoth attempts to relieve the search loop measured worse, so the code goes back\nto what it was and the numbers go in beside it.\n\nSinking `abs_ip1` and its window floor into the match paths that read them\nremoves two values from the live set and 0.8% of the loop's instructions, and\ncost 2.7% in cycles. Hoisting the rep offset above the loop removes a reload\nper position and cost 1.9%.\n\nBoth were measured by alternating two prebuilt binaries in one session, three\ntimes each, which resolves to 0.1-0.3%. Comparing across rebuilds does not: the\nsame commit read 952M and 965M in two sessions whose control arms differed by\n0.13%, so a 1.4% drift sits in the build, not in the machine, and any single\nbefore-and-after pair spanning a rebuild can invent a result of that size.\n\nThis commit changes comments only. Diffed against the reverted state, not one\nline of code differs.\n\n* perf(encode): drop the dfast short-hash prefetch\n\nIts address needs a hash of the same bytes the next iteration hashes again\nas its own short index, so warming it computed that hash twice per scanned\nposition and discarded the first. Upstream prefetches neither hash slot.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): select the dfast long candidate address, do not branch on it\n\nUpstream's `ZSTD_selectAddr` shape (`zstd_double_fast.c:203`): an out-of-window\ncandidate reads a harmless stand-in, the 8-byte compare runs unconditionally,\nand the only branch left is the one taken when those eight bytes agree, which\nis rare and therefore predictable. Upstream's own comment records the gate it\nreplaces as \"(somewhat) unpredictable\", which is the opposite of what the\ncomment here claimed.\n\nThe stand-in is `block_ptr` rather than a dedicated dummy buffer: eight bytes\nare always readable there while the loop runs, and it is already live, so the\nselect costs no register. If those eight bytes happen to equal the probe,\nvalidity still rejects them.\n\nOutput is byte-identical across levels 1-19.\n\n* revert(encode): keep the branchy dfast long gate, upstream's select loses here\n\nReverts the `ZSTD_selectAddr` port. Selecting the candidate address and\ncomparing unconditionally measured 2.0% worse in cycles across three\ninterleaved alternations of the two binaries (962-972 vs 979-994, no overlap),\nso the branch stays and the number goes in the comment that predicted it.\n\nUpstream's premise does not hold for our table: it calls the gate\n\"(somewhat) unpredictable\", but ours is predictable enough that branching\nskips the candidate load outright, which beats waiting on a conditional move to\naddress it.\n\n* fix(copy): let x86_64 take the SSE2 exact-copy arm\n\nThe arm was gated on `target_arch = \"x86\"`, which is 32-bit only, while SSE2\nis baseline on x86_64 and AVX2 is not. So a stock\n`x86_64-unknown-linux-gnu` build matched neither SIMD arm and fell through to\n`copy_nonoverlapping`, which lowers to a `memcpy` CALL for a runtime length --\nthe opposite of what this function exists to avoid.\n\nCallgrind puts that call at 92,628 invocations in two frames of z000033 at\nlevel 3, reached from the dfast match loop, once per match, 73 instructions\neach. Upstream stores the same literals inline (`ZSTD_copy16` first, wildcopy\nonly past 16 bytes) and has no such call in its profile at all.\n\nIt stayed invisible on aarch64, where NEON is baseline and the arm below fires,\nand on any build with `-C target-cpu` raising the baseline to AVX2.\n\nThe helper carried the same 32-bit-only gate and is widened with it.\n\n* perf(encode): look the length codes up instead of walking their ranges\n\nBoth length coders ran a 22-arm range match, which compiles to a chain of\ncomparisons, once per sequence from inside the sequence-encoding loop, and\nneither was inlined there: callgrind counts 96,004 calls to each over two\nframes of z000033 at level 3, at 31 instructions a call for the match-length\none. Upstream reaches the same answer with a table index (`ZSTD_LLcode` /\n`ZSTD_MLcode`) and precomputes all three codes in one pass before encoding.\n\nTable-driven now, and inlined. Every code's baseline is a multiple of its own\nextra-bit width, so masking those bits off is the same subtraction the ranges\nspelled out, and lengths past the tables take the high bit plus the delta.\n\nThe range form survives as the oracle in a test that walks every encodable\nlength of both -- 131,072 literal lengths and 131,072 match lengths -- and\nasserts the triples agree. It is also the readable statement of the format,\nwhich a pair of numeric tables is not.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(fse): shrink the encoder transition to what the encoder reads\n\n`next_state` returned four fields, three times per sequence, from the\nsequence-encoding loop. Two of them were not worth carrying:\n\n`last_index` was written and read nowhere. Its doc named a consumer in the\ntable builder that had already been retired, and the field was carrying an\n`allow(dead_code)` to say so.\n\n`baseline` is the state index with the transition's low bits cleared, so the\n\"index - baseline\" every call site then computed is just those low bits.\nMasking there instead drops the field, which is what upstream does when it\nemits `statePtr->value` under a width without subtracting anything\n(`FSE_encodeSymbol`).\n\nWhat is left is a width and a landing index, small enough to come back in\nregisters rather than through the stack. The parity check that compared the\nencoder's baseline against the decoder's rebuilds it locally, so it still\ncompares the same quantity.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): carry the FSE states as indices, not options\n\nA component has a state exactly when it has a table, and the three tables are\nresolved once above the sequence loop. Wrapping the states in options as well\nmeant re-asking that settled question three times per sequence -- as a\ntwo-element tuple destructure each time -- and re-wrapping every answer.\n\nThe states are bare indices now, so each component is one option test on its\ntable. An absent component's index is never read: every site that touches one\nalready sits inside its table's arm.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(bitio): take the flush length from the bit counter, not the vec header\n\n`bit_idx` counts the bits already committed to the output, and every path that\nappends bytes advances both it and the buffer, so it is that buffer's length in\nbits at all times -- `reset_to` resizes to match. The bulk flush was\nnonetheless loading the length back out of the `Vec` header on every call, and\nit runs two to three times per encoded sequence.\n\nThe invariant is asserted where it is now relied on, which the suite exercises\nacross every level.\n\nOutput is byte-identical across levels 1-19.\n\n* perf(encode): give the stepped seeder the hoisted insert body\n\nThe dense seeder already resolved the scan source, the rebase check and the\nslot bias once for a whole range. The stepped one, which runs when a block is\njudged incompressible, instead called the per-position helper in a loop, so\neach of its positions re-derived all three and hashed through a bounds-checked\nslice.\n\nOn a megabyte of random bytes at level 3 that put 57.7% of the whole encode in\nthat helper, 65,536 calls per frame at 91 instructions each, against 13.2% for\nthe reference's entire double-fast function, which simply scans the block with\nits growing step and finds nothing. Skipping the search cost us four times what\nsearching costs upstream.\n\nThe two seeders are one routine now, taking the step as a parameter; the dense\none passes 1. Output is byte-identical across levels 1-19 on both a\ndecodecorpus frame and a megabyte of random bytes.\n\n* fix(encode): search every block, sample the literals instead\n\nA block was sampled for entropy and, if the sample looked random, emitted raw\nwithout being searched at all. Input that looks random but repeats was\ntherefore never compared against itself: 256 KiB of noise followed by the same\n256 KiB came out at 524,317 bytes where the reference emits 262,188. Levels 1\nthrough 9, every one of them. The reference has no such pre-check.\n\nIt also bought nothing. Truly random input still reaches a raw block, by the\nordinary route of the compressed form not being smaller, and comes out the same\nsize to the byte. The pre-check only ever removed the chance of finding\nsomething.\n\nWhat made skipping the search look worthwhile was the histogram behind it: a\nblock that finds no matches becomes one long literal run, and counting a\nmegabyte of it only to reject it is expensive. The reference answers that where\nit arises rather than by refusing to look. When a block yields no sequences, or\nso few that the literals dwarf them, it counts 4 KiB from each end and emits\nraw literals if those look flat (`huf_compress.c:1367`,\n`zstd_compress.c:2924`). Eight kilobytes decide what a megabyte of counting\nwould have.\n\nSo the search always runs and the literals are sampled, which is faster than\nwhat it replaces as well as better: a megabyte of random input encodes in 0.73\nms against 0.84 before.\n\nThe whole raw-fast-path apparatus goes with it — the strict and dict-aware\nprobe variants, the probe selector, the window-size gate, and the `dict_active`\nplumbing that existed to hold the skip off when a dictionary might have\nsupplied the matches the block's own content could not.\n\nTwo tests change. The new one is the bug: a repeated noise payload must come\nout far below its input size, which fails on the old code at 524,309 of\n524,288. The dictionary-contribution test keeps its guarantee at a smaller\nmargin, because its no-dict baseline now finds the payload's internal\nrecurrences itself (102,444 -> 82,581) while the dictionary arm is unchanged to\nthe byte; the reasoning is recorded beside the assertion.\n\n* perf(encode): index dfast candidates off one pointer, as upstream does\n\nThe search loop carried three coordinate systems at once, a block-relative\ncursor, an absolute position and an index into the history concat, and every\nconstant that converts between them. Decoding one slot walked all three: the\nrebase base into position space, the history start out of it, then the source\npointer and its offset back into bytes. Four constants, and they had to stay\nlive for the whole loop.\n\nThey did not fit. The loop reloads a dozen invariants from the stack on every\niteration, and its body compiles to 167 instructions against the reference's\n60, with the same proportion of stack traffic in both. The gap is the number of\nvalues wanting registers, not the allocator.\n\nFolded, the four collapse into one pointer a slot indexes straight off, which\nis what upstream's base-plus-index is, and the window floor collapses with them\ninto a single unsigned compare in slot space. That compare also subsumes the\nempty-slot test, since the sentinel is zero and the floor is at least one.\nPosition space is reconstructed only where a match was actually found.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* perf(encode): stop carrying two dfast values the scan does not read\n\nBoth were computed on every scanned position and spilled to the stack for the\nbenefit of paths that run once in five.\n\nThe short probe's 4-byte key was carried from the 8 bytes the hash already\nloaded, then wanted once, tens of instructions later. The reference reads it\nfresh at the comparison instead, and a load that leaves no register occupied\nis cheaper here than one that does.\n\n`abs_ip1` and its window floor have no reader at all outside a match; the\ndisassembly showed both of them stored to the stack each iteration and read\nback only in the branches.\n\nBoth were tried before and both lost, by 2.7% and by nothing worth keeping,\nwhen the loop had four coordinate constants pinned in registers. They win now\nthat those are one pointer: what a change costs in this loop is the NET\nmovement in live values, and neither of these had anywhere to go before.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* revert(encode): keep the two dfast values, freeing their registers loses\n\nReverts the previous commit. Removing them measured 2.8% worse in cycles across\nthree interleaved alternations, and cost instructions as well.\n\nThe reasoning that sent me back to them was that this loop is judged by the net\nmovement in live values, so a change that lost while four coordinate constants\nwere pinned should win once those became one pointer. It does not. Reading the\nshort probe's key fresh, as the reference does, puts a second load in a loop\nwhose ports are already busy with the probes; recomputing `abs_ip1` from a\ncursor the loop advances anyway is not cheaper than the spill it replaces.\n\nBoth numbers now sit beside the code that keeps them, so the next reader has\nthe measurement rather than the argument for it.\n\n* perf(encode): read the dfast rep candidate off the block cursor\n\nUpstream compares MEM_read32(ip+1-offset_1) against MEM_read32(ip+1), both\nhanging off the cursor it is already advancing. Ours routed the candidate\nthrough the concat index instead, adding the history start offset to a\nblock-relative index built from the block bias, which is two more adds on every\nscanned position and keeps that offset live through the whole loop for the sake\nof one read.\n\nThe concat index is rebuilt where the extension and the emit want it, which is\nonly after the four bytes have agreed.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* revert(encode): keep the concat index for the rep candidate\n\nReverts the previous commit. Addressing the candidate off the block cursor, as\nupstream does, removed two adds per scanned position and 0.31% of the program's\ninstructions, and measured 1.3% WORSE in cycles across three interleaved\nalternations.\n\nThe offset is signed there, since the candidate sits before the cursor whenever\nthe match reaches back past this block, so it costs a sign-extend and a worse\naddressing mode than the unsigned add chain it replaced. Fewer instructions,\ndearer ones.\n\nWorth stating plainly because this branch has been leaning on instruction counts\nas a stand-in for time: they are comparable only within a shared boundary, and\neven then only as a diagnostic. This change moved the two in opposite\ndirections.\n\n* perf(encode): compare the dfast cursor against a precomputed limit\n\nUpstream stops the scan with a plain comparison against an ilimit it worked out\nonce, the last cursor position with a full hash key still readable. Ours added\nthe lookahead to the cursor on every scanned position to reach the same answer.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* docs(encode): record what the scan-limit measurement did and did not show\n\nThe cycle reading for the previous commit is half code layout: the same two\nbinaries differ by 0.68% at a level where that line cannot execute. The\nremainder sits inside this host's build-to-build drift, so the change rests on\nits instruction count and on matching upstream, not on a speed claim.\n\n* perf(encode): resolve the rep-chain source once, not per link\n\nThe immediate-repcode chain rebuilt its scan source and its slice on every\niteration, and the first of those iterations runs on every emitted match, most\nof them only to fail the four-byte gate and leave. Upstream tests the same four\nbytes inline off the cursor it already holds.\n\nBoth are resolved once for the chain now. Nothing the loop does moves them: the\nonly mutation reaching the buffer is the rebase inside the position insert,\nwhich shifts slot values and the base this loop already discards, and never\nreallocates the history.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* perf(encode): inline the short literal append at its call sites\n\nThe literal run is appended once per emitted sequence, from a dozen-odd sites\ninside the match loop, and the runs are short: a level-3 encode of a\ndecodecorpus frame averages about eight bytes. All of them went through one\nout-of-line call carrying the whole size ladder, worth 1.7% of the encode as\nits own symbol before the call overhead.\n\nUpstream calls nothing here. ZSTD_storeSeq stores sixteen bytes inline and only\nreaches for a copy routine past that.\n\nRuns of sixteen bytes or fewer now go inline, everything else to a tail that\nstays out of line, so the sites carry a couple of stores instead of a call\nwithout each of them growing a copy of the ladder.\n\nOutput is byte-identical on z000033 across levels 1-19.\n\n* revert(encode): keep the literal append behind a call\n\nReverts the previous commit. Splitting the short case out to\n`#[inline(always)]` and leaving the size ladder behind an out-of-line tail cost\n2.16% in cycles at level 3, on three non-overlapping readings, while removing\n1.04% of the program's instructions.\n\nThe match loop appends from a dozen-odd sites, so inlining even a short body\nthere buys decode pressure in the loop worth more than the calls it saves. That\nupstream inlines the equivalent does not carry over: its store sits in one\nplace, ours would sit in a dozen.\n\nThe control arm is weaker than usual here because this function is shared by\nevery level, so the level that would normally serve as one is not a path the\nchange cannot reach; it moved 0.20%, which leaves the level-3 signal intact\neither way.\n\nThe number is recorded above the function.\n\n* fix(encode): drop the lsm post-split gate's reference to the removed raw path\n\nThe post-split decision still excluded blocks headed for the raw fast path,\nwhich no longer exists, so the crate did not compile with `lsm` enabled. The\ncondition it guarded is unchanged: the raw path was never a post-split\ncandidate, and now nothing reaches that branch by that route.\n\n* fix(huff0): count only the parked table's heap bytes\n\nThe recycled table's own storage sits inline in the scratch, as the three\nbuffers' headers do, so adding `size_of_val` reported bytes nothing allocated\n— overstating the scratch by the size of a `HuffmanTable` whenever one was\nparked, and every other term in the same function counts capacity alone.\n\nThe figure reaches callers through the C API's context-size query, which they\nbudget against, so it has to be the one they can rely on.\n\nThe regression test parks a table and asserts the scratch grows by exactly that\ntable's heap bytes; it reports 168 against 80 without the fix. The existing\ntest could not catch this, since it compares the scratch against its own\naccounting and is satisfied either way.\n\n* fix(encode): keep the dfast slot gate and pointer sound, and the estimator honest\n\nFour defects from review, three of them mine from the coordinate fold.\n\nThe folded slot base stepped before its allocation. It is `-1` whenever the\nrebase base and the history origin coincide, which is every borrowed scan and\nthe first owned one, and `offset` requires each intermediate to stay in bounds\neven when nothing dereferences it. Wrapping arithmetic now, with the gates\nplacing the dereference inside live history.\n\nThe cursor upper bound comes back, and the argument for removing it was wrong.\nIt held within one frame, but a reused borrowed frame inherits the previous\nframe's table while its scan descriptor reports the origin as zero again, so\nthe floor-advance that retires those slots on the owned path does nothing\nthere. A shorter following frame then finds slots naming positions past its own\ninput, and a lower-bound-only gate admitted them. The regression test compresses\ntwo frames of the same size bucket, longer then shorter, through one borrowed\ncompressor: it reads a candidate at 393,217 with the cursor at 1. The bound\ncosts nothing to restore, since the cursor is already in slot space for the\nstores.\n\nThe cached weight description is retained memory, not a transient. It is built\nlazily but lives as long as its table, and tables now outlive the block that\nmade them, so it belongs in the figure `ZSTD_sizeof_CCtx` reports. Its test\ngrows the table's description and asserts the reported total grows with it: 74\nbytes went uncounted before.\n\nThe splitter's cost estimator did not get the literal-sampling decision the\nemitter makes, so a section with flat ends and a biased interior costs as\nHuffman-compressed there and is emitted raw here, letting the planner pick a\npartition on a price nothing can produce. Both paths call one sampling helper\nnow, and the estimator applies it in the same position in its branch order.\n\n* docs: add AGENTS.md, a performance checklist for this codebase\n\nThe hot paths here run per input byte, per scanned position and per emitted\nsequence, and a change that is correct and readable can still be wrong in ways\nan ordinary read does not look for.\n\nCollects what this codebase has actually been caught by: borrowing versus\ncloning in both directions, allocation per unit of work and what retained\nmemory the context-size query has to report, why saturating arithmetic is not a\nsafety measure and what a hot-path gate should look like instead, where CPU\nfeature dispatch belongs relative to a loop, how a hot loop's coordinate systems\nturn into register pressure, and what a performance claim has to carry before it\ncan be believed.\n\n* perf(kernel): resolve the CPU tier before the work, not during it\n\nWhich kernels exist is a compile-time question; which one runs is a\nproperty of the CPU executing the binary. Two places had that backwards.\n\nThe decoder re-asked `detect_cpu_kernel()` for every literals section and\nevery sequence section, so a cached selector's atomic load and branch were\npaid twice per block for an answer fixed at process start. The tag is now\nresolved once when the block decoder is built and handed to both section\ndecoders, which match it to their per-tier monomorph exactly as before.\n`decode_literals` is its own entry point with no owner above it, so it\nresolves on the way in.\n\n`copy_exact_medium` had no runtime selection at all: its tiers were chosen\nby `cfg(target_feature = ...)`, which reflects the build's baseline rather\nthan the running CPU. On a stock x86_64 build the AVX2 kernel was not\ncompiled at all and SSE2 won permanently, so the widest kernel was\nunreachable on every CPU that had it. The kernels now carry\n`#[target_feature(enable = ...)]` and are present regardless of baseline,\nand the tier is resolved once per compressor into `CompressState`, hoisted\nout of the emit closure and passed down. NEON keeps its `cfg` gate: it is\narchitectural on aarch64, so there is nothing to detect. Builds with no\ndetection available (`no_std`) fall back to the baseline, which is the only\nevidence they have.\n\nWidening this to a per-tier monomorph of the whole emit loop was not taken:\nit would replicate the match loop for the sake of one predictable branch,\nand monomorphising that loop has measured as an instruction-cache\nregression here before.\n\nEvery tier now lives in the artifact, so all of them can finally be tested\non one machine rather than only whichever the build baked in. The sweep\nholds each against the scalar reference across the medium size range, and a\nsecond test pins that the resolver never names a tier the CPU cannot run,\nwhich would be an illegal instruction in production.\n\nAGENTS.md described the compile-time gate as failing because of the build\nmachine's CPU. That is the wrong mechanism, and the wrong mechanism is what\nlet this through: the flag follows the target baseline, so the common\nfailure is not a crash but a silent fall to a narrower path. The rule also\nsaid \"never\", which would flag the legitimate NEON and SSE2 gates, and\nasked for a test of two kernels where there are several.\n\n* test: cover the paths the accounting and estimator fixes added\n\nFour paths this branch introduced ran under no test. All four are places\nwhere being wrong is silent, which is why the gap mattered rather than the\npercentage.\n\nThe estimator's end-sample shortcut had no test at all, despite being the\nfix for a divergence: a section with flat ends and a biased interior is\nemitted raw, and the estimator has to reach that decision at the same point\nin the branch order or the splitter prices a partition nothing can produce.\nWithout the shortcut the estimator costs the new fixture at 14,207 bytes\nagainst the 48,196 the emitter writes, so the test fails by a factor of\nthree on the unfixed code.\n\n`StreamingEncoder::heap_size` was untested as a whole. It backs\n`ZSTD_sizeof_CCtx`, callers budget against it, and a term dropped from the\nsum is invisible to every roundtrip test. The test compresses, then holds\nthe reported total against the parts it is made of.\n\n`FSETable::heap_size` returning zero is what lets the dictionary-entropy\nfootprint report a cached table as its inline size alone. That is only true\nwhile every array it carries is fixed-size, so it is pinned against a table\nbuilt from real counts.\n\nThe weight builder's fallback for a degenerate distribution, taken when the\nheight limiter cannot restore a full canonical code, is a correctness net\nrather than dead code: without it the caller gets a weight sum that is not a\npower of two and the table builder rejects it. Fibonacci counts are the\nstandard worst case for Huffman depth and reach it across the tight end of\nthe table-log range.\n\n* docs(simd-copy): record what runtime tier selection costs\n\nThe dispatch trades an inlinable baseline-gated kernel for a called\ntarget_feature one. Measured near-neutral, so the note says plainly that\nthe reason to dispatch at runtime is reach rather than speed, and what\nthe compile-time gate did instead.\n\n* test(encode): pin that incompressible input stays its own size\n\nRemoving the pre-search skip rests on genuinely random input reaching a\nraw block through the ordinary compressed-is-not-smaller fallback. That\nfallback had no test: the repeated-noise regression would still pass if\nraw emission broke and random input began expanding.\n\nCloses the last open acceptance criterion of #484.\n\n* fix(huff0): pin the height limiter's cost shift against overflow\n\nThe limiter computes `1usize << (largest_bits - target_nb_bits)`. On a\n32-bit target a large enough gap masks the shift instead of panicking in\nrelease, so the cost comes out wrong and the weights follow it, silently.\n\nThe gap is bounded in practice: a code of depth d needs Fib(d) symbols\ncounted, and a literals section is at most 128 KiB, which caps depth near\n25 against a table log of at least 5. That reasoning now sits in a\ndebug_assert rather than in nobody's head.\n\nFound by the degenerate-distribution test failing on i686 only. Its\nFibonacci fixture ran to 40 terms, which describes a section of hundreds\nof megabytes and a depth no input can produce; it is cut to a length that\nfits one section, which also stops it asserting on unreachable inputs.\n\n* test(streaming): prove the weight scratch is in the reported footprint\n\nThe check compared the total against a sum it contains, which the\nmatch-finder's share satisfies on its own, so it would have passed with\nthe weight scratch missing from the accounting entirely. It now removes\nthe scratch and requires the total to fall by exactly its size; without\nthe term it reports 0 against 4608.\n\nThe fixture was also drawn from the full byte range, so its literals went\nout raw and the weight builder never ran. Narrowed to 32 symbols, which\nkeeps the literals worth coding.",
          "timestamp": "2026-09-04T17:13:28+03:00",
          "tree_id": "4a827d5d0fe50fcf1d974ec2688e6b5c87d9e709",
          "url": "https://github.com/structured-world/structured-zstd/commit/45685abca498a46e6d019b9cbddf3aa055a1530c"
        },
        "date": 1788533774991,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/pure_rust",
            "value": 0.08,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/small-4k-log-lines/matrix/c_ffi",
            "value": 0.084,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/pure_rust",
            "value": 222.151,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/decodecorpus-z000033/matrix/c_ffi",
            "value": 222.993,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/pure_rust",
            "value": 0.956,
            "unit": "ms"
          },
          {
            "name": "compress/level_22_btultra2/low-entropy-1m/matrix/c_ffi",
            "value": 1.667,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 2.744,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.951,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 2.773,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.974,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_22_btultra2/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.126,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/pure_rust",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/small-4k-log-lines/matrix/c_ffi",
            "value": 0.007,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/pure_rust",
            "value": 10.339,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/decodecorpus-z000033/matrix/c_ffi",
            "value": 6.443,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/pure_rust",
            "value": 0.137,
            "unit": "ms"
          },
          {
            "name": "compress/level_3_dfast/low-entropy-1m/matrix/c_ffi",
            "value": 0.176,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/rust_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/pure_rust",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/small-4k-log-lines/c_stream/matrix/c_ffi",
            "value": 0.002,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/pure_rust",
            "value": 1.552,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/rust_stream/matrix/c_ffi",
            "value": 1.211,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/pure_rust",
            "value": 1.711,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/decodecorpus-z000033/c_stream/matrix/c_ffi",
            "value": 1.303,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/pure_rust",
            "value": 0.026,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/rust_stream/matrix/c_ffi",
            "value": 0.156,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/pure_rust",
            "value": 0.025,
            "unit": "ms"
          },
          {
            "name": "decompress/level_3_dfast/low-entropy-1m/c_stream/matrix/c_ffi",
            "value": 0.187,
            "unit": "ms"
          }
        ]
      }
    ]
  }
}