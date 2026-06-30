window.BENCHMARK_DATA = {
  "lastUpdate": 1782817728032,
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
      }
    ]
  }
}