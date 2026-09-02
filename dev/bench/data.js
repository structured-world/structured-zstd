window.BENCHMARK_DATA = {
  "lastUpdate": 1788391401924,
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
      }
    ]
  }
}