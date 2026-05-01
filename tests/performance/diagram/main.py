"""Plot tgin benchmark results from ../results.csv.

Reads the CSV produced by `tgin-bench` / `benchmark.sh` and emits four PNGs
(loss, mean, max, median) into ./generated/, plotting each metric vs RPS with
one line per mode. Rows sharing the same (mode, rps) are averaged.

Run from this directory:
    uv run main.py
"""

from __future__ import annotations

import csv
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import matplotlib.pyplot as plt

# CSV column indices (0-based), matching the header in results.csv:
# mode,rps,sent,recv,errors,loss_%,min_ms,mean_ms,p50_ms,p95_ms,p99_ms,max_ms
COL_MODE = 0
COL_RPS = 1
COL_LOSS = 5
COL_MEAN_MS = 7
COL_MEDIAN = 8
COL_MAX_MS = 11

# Stable color palette (RGB 0-1) — kept identical to the previous Go script
# so regenerated plots stay visually consistent with prior outputs.
COLORS: list[tuple[float, float, float]] = [
    (255 / 255, 0 / 255, 0 / 255),
    (0 / 255, 0 / 255, 255 / 255),
    (0 / 255, 128 / 255, 0 / 255),
    (255 / 255, 165 / 255, 0 / 255),
    (128 / 255, 0 / 255, 128 / 255),
    (255 / 255, 215 / 255, 0 / 255),
    (0 / 255, 255 / 255, 255 / 255),
    (165 / 255, 42 / 255, 42 / 255),
    (0 / 255, 0 / 255, 0 / 255),
    (255 / 255, 0 / 255, 255 / 255),
    (128 / 255, 128 / 255, 128 / 255),
    (34 / 255, 139 / 255, 34 / 255),
]

# Modes that represent the single-bot baseline; rendered with a thicker line
# so the comparison against any tgin-clustered configuration stands out.
EMPHASIZED_MODES = frozenset({"longpull-direct", "webhook-direct"})


@dataclass
class _Accum:
    total: float = 0.0
    count: int = 0

    def add(self, value: float) -> None:
        self.total += value
        self.count += 1

    @property
    def mean(self) -> float:
        return self.total / self.count


def read_csv(path: Path) -> list[list[str]]:
    with path.open(newline="") as f:
        return list(csv.reader(f))


def _aggregate(
    records: list[list[str]], val_col: int
) -> dict[str, list[tuple[float, float]]]:
    """Return {mode: [(rps, mean_value), ...]} sorted by rps ascending.

    Rows that fail numeric parsing are skipped (matching the prior behavior).
    """
    by_mode: dict[str, dict[float, _Accum]] = defaultdict(lambda: defaultdict(_Accum))

    # records[0] is the header row.
    for row in records[1:]:
        try:
            rps = float(row[COL_RPS])
            val = float(row[val_col])
        except (ValueError, IndexError):
            continue
        by_mode[row[COL_MODE]][rps].add(val)

    return {
        mode: sorted((rps, acc.mean) for rps, acc in rps_map.items())
        for mode, rps_map in by_mode.items()
    }


def create_plot(
    filename: Path,
    title: str,
    x_label: str,
    y_label: str,
    records: list[list[str]],
    val_col: int,
) -> None:
    data_by_mode = _aggregate(records, val_col)

    fig, ax = plt.subplots(figsize=(12, 8))
    ax.set_title(title, fontsize=20)
    ax.set_xlabel(x_label, fontsize=14)
    ax.set_ylabel(y_label, fontsize=14)
    ax.grid(True)

    # Sort modes lexicographically so color assignment is stable across runs.
    for i, mode in enumerate(sorted(data_by_mode)):
        points = data_by_mode[mode]
        xs = [p[0] for p in points]
        ys = [p[1] for p in points]
        color = COLORS[i % len(COLORS)]
        linewidth = 6 if mode in EMPHASIZED_MODES else 2
        ax.plot(
            xs,
            ys,
            color=color,
            linewidth=linewidth,
            marker="o",
            markersize=6,
            label=mode,
        )

    ax.legend(loc="upper left", fontsize=12, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(filename, dpi=100)
    plt.close(fig)


def main() -> int:
    here = Path(__file__).resolve().parent
    csv_path = here.parent / "results.csv"
    if not csv_path.exists():
        print(f"cannot open {csv_path}: file not found", file=sys.stderr)
        return 1

    out_dir = here / "generated"
    out_dir.mkdir(exist_ok=True)

    records = read_csv(csv_path)

    plots = [
        ("loss.png", "Loss Rate (%)", "Loss (%)", COL_LOSS),
        ("mean.png", "Mean Latency (ms)", "Time (ms)", COL_MEAN_MS),
        ("max.png", "Max Latency (ms)", "Time (ms)", COL_MAX_MS),
        ("median.png", "Median Latency (ms)", "Time (ms)", COL_MEDIAN),
    ]
    for filename, title, y_label, col in plots:
        create_plot(out_dir / filename, title, "RPS", y_label, records, col)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
