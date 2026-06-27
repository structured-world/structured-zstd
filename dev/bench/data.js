window.BENCHMARK_DATA = {
  "lastUpdate": 1782591353112,
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
      }
    ]
  }
}