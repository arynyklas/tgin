"""Plot tgin benchmark results.

Reads the typed CSV schema produced by `tgin-bench` / `benchmark.sh` and emits
separate PNG sets for overhead and scale benchmark families into ./generated/.
Rows are aggregated only when their scenario identity matches, and failed runs
remain visible in loss plots without being plotted as 0 ms latency.

By default, the newest ../results/*.csv run file is plotted. Pass one or more
CSV paths explicitly to render specific campaigns.

Run from this directory:
    uv run main.py [../results/<run_id>.csv ...]
"""

from __future__ import annotations

import argparse
import csv
import sys
from collections import defaultdict
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

import matplotlib.pyplot as plt

REQUIRED_COLUMNS: tuple[str, ...] = (
    "run_id",
    "git_sha",
    "scenario_family",
    "transport",
    "route_path",
    "scenario",
    "mode",
    "bot_count",
    "rps_target",
    "rps_actual",
    "duration_seconds",
    "drain_timeout_seconds",
    "sent",
    "received",
    "send_errors",
    "http_errors",
    "pending_end",
    "loss_percent",
    "min_ms",
    "mean_ms",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
)

SERIES_COLUMNS: tuple[str, ...] = (
    "scenario_family",
    "transport",
    "route_path",
    "scenario",
    "mode",
    "bot_count",
)

LATENCY_METRICS = frozenset(
    {"min_ms", "mean_ms", "p50_ms", "p95_ms", "p99_ms", "max_ms"}
)

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


@dataclass(frozen=True)
class ScenarioKey:
    scenario_family: str
    transport: str
    route_path: str
    scenario: str
    mode: str
    bot_count: str

    @classmethod
    def from_row(cls, row: dict[str, str]) -> ScenarioKey:
        return cls(*(row[column] for column in SERIES_COLUMNS))

    @property
    def label(self) -> str:
        return f"{self.scenario} bots={self.bot_count}"

    @property
    def is_baseline(self) -> bool:
        return self.scenario.endswith("-direct")


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


@dataclass(frozen=True)
class Series:
    key: ScenarioKey
    points: tuple[tuple[float, float], ...]


def read_csv(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as f:
        return _read_csv_rows(f)


def read_csv_files(paths: list[Path]) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    for path in paths:
        records.extend(read_csv(path))
    return records


def default_csv_paths(performance_dir: Path) -> list[Path]:
    run_files = sorted(
        (performance_dir / "results").glob("*.csv"),
        key=lambda path: path.stat().st_mtime,
    )
    if run_files:
        return [run_files[-1]]
    return [performance_dir / "results.csv"]


def _read_csv_rows(lines: Iterable[str]) -> list[dict[str, str]]:
    reader = csv.DictReader(lines)
    if reader.fieldnames is None:
        raise ValueError("results CSV is empty; expected typed tgin-bench header")

    missing = [column for column in REQUIRED_COLUMNS if column not in reader.fieldnames]
    if missing:
        joined = ", ".join(missing)
        raise ValueError(f"results CSV is missing required columns: {joined}")

    return [
        {column: row[column] or "" for column in REQUIRED_COLUMNS} for row in reader
    ]


def _parse_float(row: dict[str, str], column: str) -> float | None:
    value = row[column].strip()
    if not value:
        return None
    try:
        return float(value)
    except ValueError:
        return None


def _parse_int(row: dict[str, str], column: str) -> int | None:
    value = row[column].strip()
    if not value:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def _aggregate(records: list[dict[str, str]], metric: str) -> list[Series]:
    """Return series sorted by label, with points sorted by target RPS."""
    if metric not in REQUIRED_COLUMNS:
        raise ValueError(f"unknown benchmark metric: {metric}")

    by_series: dict[ScenarioKey, dict[float, _Accum]] = defaultdict(
        lambda: defaultdict(_Accum)
    )

    for row in records:
        rps = _parse_float(row, "rps_target")
        value = _parse_float(row, metric)
        received = _parse_int(row, "received")
        if rps is None or value is None:
            continue
        if metric in LATENCY_METRICS and received == 0:
            continue

        by_series[ScenarioKey.from_row(row)][rps].add(value)

    return [
        Series(key, tuple(sorted((rps, acc.mean) for rps, acc in rps_map.items())))
        for key, rps_map in sorted(by_series.items(), key=lambda item: item[0].label)
    ]


def create_plot(
    filename: Path,
    title: str,
    x_label: str,
    y_label: str,
    records: list[dict[str, str]],
    metric: str,
    scenario_family: str | None = None,
    transport: str | None = None,
) -> None:
    filtered_records = [
        row
        for row in records
        if (scenario_family is None or row["scenario_family"] == scenario_family)
        and (transport is None or row["transport"] == transport)
    ]
    series_list = _aggregate(filtered_records, metric)

    fig, ax = plt.subplots(figsize=(12, 8))
    ax.set_title(title, fontsize=20)
    ax.set_xlabel(x_label, fontsize=14)
    ax.set_ylabel(y_label, fontsize=14)
    ax.grid(True)

    for i, series in enumerate(series_list):
        xs = [point[0] for point in series.points]
        ys = [point[1] for point in series.points]
        color = COLORS[i % len(COLORS)]
        linewidth = 6 if series.key.is_baseline else 2
        ax.plot(
            xs,
            ys,
            color=color,
            linewidth=linewidth,
            marker="o",
            markersize=6,
            label=series.key.label,
        )

    if series_list:
        ax.legend(loc="upper left", fontsize=12, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(filename, dpi=100)
    plt.close(fig)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Plot typed tgin benchmark CSV results."
    )
    parser.add_argument(
        "csv_paths",
        nargs="*",
        type=Path,
        help="CSV files to render; defaults to newest ../results/*.csv, then ../results.csv",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    here = Path(__file__).resolve().parent
    performance_dir = here.parent
    csv_paths = args.csv_paths or default_csv_paths(performance_dir)
    missing_paths = [path for path in csv_paths if not path.exists()]
    if missing_paths:
        joined = ", ".join(str(path) for path in missing_paths)
        print(f"cannot open benchmark CSV path(s): {joined}", file=sys.stderr)
        return 1

    out_dir = here / "generated"
    out_dir.mkdir(exist_ok=True)
    for stale_chart in out_dir.glob("*.png"):
        stale_chart.unlink()

    try:
        records = read_csv_files(csv_paths)
    except ValueError as exc:
        print(f"invalid benchmark results CSV: {exc}", file=sys.stderr)
        return 1

    plots = [
        ("loss", "Loss (%)", "loss_percent"),
        ("mean", "Time (ms)", "mean_ms"),
        ("max", "Time (ms)", "max_ms"),
        ("median", "Time (ms)", "p50_ms"),
    ]
    chart_sets = [
        ("webhook", "overhead", "Webhook routing overhead, equal backend count"),
        ("webhook", "scale", "Webhook tgin scale-out by backend count"),
        ("longpoll", "overhead", "Long-poll routing overhead, equal backend count"),
        ("longpoll", "scale", "Long-poll tgin scale-out by backend count"),
    ]
    for transport, scenario_family, title in chart_sets:
        for metric_slug, y_label, metric in plots:
            create_plot(
                out_dir / f"{transport}-{scenario_family}-{metric_slug}.png",
                title,
                "Target RPS",
                y_label,
                records,
                metric,
                scenario_family=scenario_family,
                transport=transport,
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
