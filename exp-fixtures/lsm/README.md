# Data blocks of the point-read benchmark, as frames

The data: 70,000 keys of 16 bytes with 256-byte values cut from a 1 MiB pool
of 100-byte pieces (50 random printable bytes written twice), the workload of
`point_read_zstd22` in `tools/compare-rocksdb` of coordinode-lsm-tree. Written
once into our engine and once into RocksDB 11.8.1, both at zstd level 22 with
a 4 KiB block size and no dictionary, then every data block taken out of the
table file each wrote.

## Files

Every file has the same layout: for each block a little-endian `u32` frame
length, a little-endian `u32` decoded length, then the frame's bytes. `lib.rs`
here reads and writes it (`read_frames`, `write_frames`).

| File | Blocks | What |
|---|---|---|
| `ours.frames` | 4375 | the frames in our table file, written by structured-zstd 0.4.4 `compress_independent_frame` |
| `ours.raw` | 4375 | the same blocks decoded (frame length equals decoded length) |
| `ours-by-c.frames` | 4375 | our raw blocks compressed by libzstd 1.6.0 `ZSTD_compress` at level 22 |
| `rocksdb.frames` | 5000 | the frames in RocksDB's table file, written by its libzstd |
| `rocksdb.raw` | 5000 | the same blocks decoded |
| `rocksdb-by-ours.frames` | 5000 | RocksDB's raw blocks compressed by structured-zstd at level 22 |

## What the sets show already

- Our blocks are larger: 19,663,464 raw bytes in 4375 blocks, 4494 bytes a
  block, against RocksDB's 19,847,162 in 5000, 3969 a block. Our table writer
  closes a block after it passes 4 KiB, RocksDB before. A point read that
  misses the cache decodes one block, so ours decodes 13% more bytes.
- The encoders agree on most blocks: 4318 of our 4375 frames and 4563 of
  RocksDB's 5000 are byte-identical between structured-zstd and libzstd.
  Total frame bytes: ours 11,292,628 by structured-zstd against 11,292,448 by
  libzstd; RocksDB's 11,134,378 by libzstd against 11,135,899 by
  structured-zstd.

## Block layouts

- Ours: the table file is a run of blocks from its first byte, each a header
  (magic, block type, checksum, payload length, decoded length) and a payload;
  a zstd data block's payload is the frame.
- RocksDB: the table file is a run of blocks from its first byte, each
  followed by a one-byte codec tag (7 for zstd) and a four-byte checksum; a
  zstd block is the decoded length as a varint, then the frame.
