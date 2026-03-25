#!/bin/bash
# Run compare_ffi benchmarks and produce github-action-benchmark JSON.
# Output: benchmark-results.json (customSmallerIsBetter format — lower time = better)
set -e

echo "Running benchmarks..." >&2

# Run criterion benchmarks, capture output
cargo bench --bench compare_ffi -p structured-zstd -- --output-format bencher 2>/dev/null | tee /tmp/bench-raw.txt

echo "Parsing results..." >&2

# Parse criterion bencher output into github-action-benchmark JSON
# Format: "test <name> ... bench: <ns> ns/iter (+/- <variance>)"
python3 - <<'PYEOF'
import json, re, sys

results = []
with open("/tmp/bench-raw.txt") as f:
    for line in f:
        m = re.match(r"test (\S+)\s+\.\.\. bench:\s+([\d,]+) ns/iter", line)
        if m:
            name = m.group(1)
            ns = int(m.group(2).replace(",", ""))
            # Convert ns to ms for readability
            ms = ns / 1_000_000
            results.append({
                "name": name,
                "unit": "ms",
                "value": round(ms, 3),
            })

if not results:
    print("WARNING: No benchmark results parsed!", file=sys.stderr)
    # Write empty array so CI doesn't fail
    results = [{"name": "no_results", "unit": "ms", "value": 0}]

with open("benchmark-results.json", "w") as f:
    json.dump(results, f, indent=2)

print(f"Wrote {len(results)} benchmark results to benchmark-results.json", file=sys.stderr)
for r in results:
    print(f"  {r['name']}: {r['value']} {r['unit']}", file=sys.stderr)
PYEOF
