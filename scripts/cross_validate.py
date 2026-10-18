#!/usr/bin/env python3
"""Cross-validate our `zstd` against the reference `zstd` on whole corpora.

For every file of every corpus at every level:

  A1  our frame decodes through the reference to the input,
  A2  the reference's frame decodes through ours to the input,
  A3  our frame decodes through ours to the input,

and per (corpus, level) the summed compressed size and compression time of
ours over the reference's are reported, and held to the given limits.

A file's compression time is the fastest of --repeat runs of each tool, the
two alternating, so a run the machine preempted does not decide the ratio.

Usage:
  cross_validate.py --ours BIN --reference BIN --corpus NAME=DIR [...]
                    [--levels 1,3,9,19,22] [--ratio-limit 1.05]
                    [--time-limit 1.5] [--repeat 3] [--json OUT]

A limit of 0 reports the figure without holding it. Exits non-zero when any
roundtrip fails or any figure is past its limit.

--json writes the rows as one dashboard snapshot (one record per corpus and
level), stamped with GITHUB_SHA and STRUCTURED_ZSTD_BENCH_GENERATED_AT when
set, in the layout .github/scripts/merge-timeseries.py folds into history.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path


def level_args(level):
    # Levels past 19 need --ultra on both tools.
    return (["--ultra"] if level > 19 else []) + [f"-{level}"]


def compress(binary, level, src, dst):
    # --single-thread on both: the reference otherwise compresses in worker
    # threads, so its time would measure thread overlap, not the codec.
    start = time.perf_counter()
    with open(dst, "wb") as out:
        subprocess.run(
            [binary, *level_args(level), "--single-thread", "-q", "-c", "--", str(src)],
            stdout=out,
            check=True,
        )
    return time.perf_counter() - start


def decodes_to(binary, frame, original):
    result = subprocess.run(
        [binary, "-d", "-q", "-c", "--", str(frame)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.returncode == 0 and result.stdout == original


def corpus_files(directory):
    # A `.zst` beside its source (the decodecorpus fixtures keep both) is a
    # frame, not an input.
    return sorted(
        p
        for p in Path(directory).rglob("*")
        if p.is_file() and p.suffix != ".zst" and p.stat().st_size > 0
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--ours", required=True)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--corpus", action="append", required=True, metavar="NAME=DIR")
    parser.add_argument("--levels", default="1,3,9,19,22")
    parser.add_argument("--ratio-limit", type=float, default=1.05)
    parser.add_argument("--time-limit", type=float, default=1.5)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--json")
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")

    levels = [int(level) for level in args.levels.split(",")]
    corpora = []
    for spec in args.corpus:
        name, _, directory = spec.partition("=")
        files = corpus_files(directory)
        if not name or not files:
            parser.error(f"corpus {spec!r} names no files")
        corpora.append((name, files))

    failures = []
    rows = []
    with tempfile.TemporaryDirectory() as work:
        ours_frame = Path(work) / "ours.zst"
        reference_frame = Path(work) / "reference.zst"
        for name, files in corpora:
            for level in levels:
                row = {
                    "corpus": name,
                    "level": level,
                    "files": len(files),
                    "input_bytes": 0,
                    "ours_bytes": 0,
                    "reference_bytes": 0,
                    "ours_seconds": 0.0,
                    "reference_seconds": 0.0,
                }
                for path in files:
                    original = path.read_bytes()
                    row["input_bytes"] += len(original)
                    ours_best = reference_best = float("inf")
                    for _ in range(args.repeat):
                        ours_best = min(ours_best, compress(args.ours, level, path, ours_frame))
                        reference_best = min(
                            reference_best, compress(args.reference, level, path, reference_frame)
                        )
                    row["ours_seconds"] += ours_best
                    row["reference_seconds"] += reference_best
                    row["ours_bytes"] += ours_frame.stat().st_size
                    row["reference_bytes"] += reference_frame.stat().st_size
                    for check, binary, frame in (
                        ("A1", args.reference, ours_frame),
                        ("A2", args.ours, reference_frame),
                        ("A3", args.ours, ours_frame),
                    ):
                        if not decodes_to(binary, frame, original):
                            failures.append(f"{check} {name} level {level}: {path}")
                row["size_ratio"] = row["ours_bytes"] / row["reference_bytes"]
                row["time_ratio"] = row["ours_seconds"] / row["reference_seconds"]
                rows.append(row)
                print(
                    f"{name:<12} -{level:<3} files {row['files']:>3}  "
                    f"size {row['ours_bytes']:>11} / {row['reference_bytes']:>11} = {row['size_ratio']:.4f}  "
                    f"time {row['ours_seconds']:8.2f}s / {row['reference_seconds']:8.2f}s = {row['time_ratio']:.2f}",
                    flush=True,
                )
                if args.ratio_limit and row["size_ratio"] > args.ratio_limit:
                    failures.append(
                        f"size {name} level {level}: {row['size_ratio']:.4f} > {args.ratio_limit}"
                    )
                if args.time_limit and row["time_ratio"] > args.time_limit:
                    failures.append(
                        f"time {name} level {level}: {row['time_ratio']:.2f} > {args.time_limit}"
                    )

    if args.json:
        commit_sha = os.environ.get("GITHUB_SHA")
        generated_at = (
            os.environ.get("STRUCTURED_ZSTD_BENCH_GENERATED_AT")
            or datetime.now(timezone.utc).isoformat()
        )
        records = [
            {
                "kind": "cli",
                "scenario": row.pop("corpus"),
                "engine": "ours",
                **row,
                "commit_sha": commit_sha,
                "generated_at": generated_at,
            }
            for row in rows
        ]
        payload = {
            "version": 1,
            "reference_engine": "reference",
            "engines": ["ours"],
            "limits": {"size_ratio": args.ratio_limit, "time_ratio": args.time_limit},
            "records": records,
        }
        Path(args.json).write_text(json.dumps(payload, indent=2) + "\n")
    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        sys.exit(1)
    print("== cross-validation passed")


if __name__ == "__main__":
    main()
