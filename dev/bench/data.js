window.BENCHMARK_DATA = {
  "lastUpdate": 1782477942311,
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
      }
    ]
  }
}