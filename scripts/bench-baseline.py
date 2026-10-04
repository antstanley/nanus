#!/usr/bin/env python3
"""Fold a saved criterion baseline into one markdown table.

Criterion keeps its results under `target/criterion/`, which is not committed, so a
baseline recorded on one machine is gone on the next clone. This reads a named baseline
(`cargo bench -p nanus-bench -- --save-baseline <name>`) and prints one row per benchmark
with its three measurements side by side — wall time, allocations, and bytes allocated —
which is the form `docs/benchmarks.md` records.

    scripts/bench-baseline.py main            # the baseline saved as `main`
    scripts/bench-baseline.py new             # whatever the last run produced
    scripts/bench-baseline.py main --by-area  # one table per area, under `###` headings

The `±` column is half the width of criterion's 95% confidence interval for the time, as a
percentage of the estimate: how far apart two runs can land before a difference means
anything. The counts carry no such column because they repeat exactly.

The `nanus-bench` ids are `<measurement>/<area>/<benchmark>`, so the measurement prefix
is what joins the three runs of one benchmark into one row. Standard library only.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

MEASUREMENTS = ("time", "allocs", "bytes")


def estimate(path: Path) -> tuple[float, float]:
    """The figure criterion reports — the regression slope when it fitted one, else the
    mean — and its 95% confidence interval's half-width as a fraction of it."""
    data = json.loads(path.read_text())
    chosen = data.get("slope") or data["mean"]
    point = float(chosen["point_estimate"])
    interval = chosen["confidence_interval"]
    spread = (float(interval["upper_bound"]) - float(interval["lower_bound"])) / 2.0
    return point, (spread / point if point else 0.0)


def scaled(value: float, step: float, units: tuple[str, ...]) -> str:
    rung = 0
    while rung < len(units) - 1 and abs(value) >= step:
        value /= step
        rung += 1
    return f"{value:.2f} {units[rung]}".rstrip()


def fmt(measurement: str, value: float | None) -> str:
    if value is None:
        return "—"
    if measurement == "time":
        return scaled(value, 1000.0, ("ns", "µs", "ms", "s"))
    if measurement == "allocs":
        return f"{value:,.0f}" if value >= 10 else f"{value:.2f}"
    return scaled(value, 1024.0, ("B", "KiB", "MiB", "GiB"))


def collect(root: Path, baseline: str) -> dict[str, dict[str, tuple[float, float]]]:
    rows: dict[str, dict[str, tuple[float, float]]] = {}
    for marker in root.rglob(f"{baseline}/benchmark.json"):
        meta = json.loads(marker.read_text())
        full_id: str = meta["full_id"]
        measurement, _, rest = full_id.partition("/")
        if measurement not in MEASUREMENTS:
            continue
        rows.setdefault(rest, {})[measurement] = estimate(marker.parent / "estimates.json")
    return rows


def table(rows: dict[str, dict[str, tuple[float, float]]], names: list[str], strip: str) -> None:
    print("| Benchmark | Time | ± | Allocations | Bytes allocated |")
    print("|---|---:|---:|---:|---:|")
    for name in names:
        row = rows[name]
        time = row.get("time")
        cells = [
            fmt("time", time[0] if time else None),
            f"{time[1] * 100:.1f}%" if time else "—",
            fmt("allocs", row["allocs"][0] if "allocs" in row else None),
            fmt("bytes", row["bytes"][0] if "bytes" in row else None),
        ]
        print(f"| `{name.removeprefix(strip)}` | " + " | ".join(cells) + " |")


def main() -> int:
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    by_area = "--by-area" in sys.argv[1:]
    baseline = args[0] if args else "new"
    root = Path(args[1]) if len(args) > 1 else Path("target/criterion")
    if not root.is_dir():
        print(f"no criterion results under {root}; run `cargo bench -p nanus-bench` first",
              file=sys.stderr)
        return 1
    rows = collect(root, baseline)
    if not rows:
        print(f"no benchmarks saved as `{baseline}` under {root}", file=sys.stderr)
        return 1
    if not by_area:
        table(rows, sorted(rows), "")
        return 0
    areas: dict[str, list[str]] = {}
    for name in sorted(rows):
        areas.setdefault(name.partition("/")[0], []).append(name)
    for index, (area, names) in enumerate(areas.items()):
        if index:
            print()
        print(f"### `{area}`")
        print()
        table(rows, names, f"{area}/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
