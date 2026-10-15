"""Plot tgin benchmark results.

Reads the CSV written by `benchmark.sh` (one row per run) and renders, per
transport, achieved and delivered against target RPS and the latency
percentiles, with one series per engine and scenario. Achieved RPS is the
generator's send rate; delivered RPS is the reply rate, which shows whether
the engine kept up. Runs of the same series and rate are averaged. Direct
baselines do not go through tgin, so their rows form one `direct` series
whatever engine the run was for.

Without arguments the newest ../results/<run_id>.csv is plotted, newest by
`run_id` (the file name), not by modification time. Pass CSV paths to plot
specific runs, for example a v0 and a v1 run side by side.

Run from this directory:
    uv run main.py [../results/<run_id>.csv ...]
"""

import argparse
import csv
import sys
from collections import defaultdict
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

import matplotlib.pyplot as plt

REQUIRED_COLUMNS: tuple[str, ...] = (
    "scenario",
    "engine",
    "target_rps",
    "achieved_rps",
    "delivered_rps",
    "p50_ms",
    "p95_ms",
    "p99_ms",
    "max_ms",
    "unreceived",
    "duplicate_replies",
    "dropped_by_generator",
    "lag_p99_ms",
)

DIRECT_ENGINE = "direct"

# Stable palette (RGB 0-1), one color per series.
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

# Each engine gets its own line style, so engines stay apart in print.
ENGINE_STYLES: dict[str, str] = {DIRECT_ENGINE: ":", "v0": "--", "v1": "-"}


@dataclass(frozen=True, order=True)
class SeriesKey:
    engine: str
    scenario: str

    @classmethod
    def from_row(cls, row: dict[str, str]) -> "SeriesKey":
        scenario = row["scenario"]
        engine = DIRECT_ENGINE if is_direct(scenario) else row["engine"]
        return cls(engine, scenario)

    @property
    def label(self) -> str:
        if self.engine == DIRECT_ENGINE:
            return self.scenario
        return f"{self.engine} {self.scenario}"


@dataclass(frozen=True)
class Series:
    key: SeriesKey
    points: tuple[tuple[float, float], ...]


def is_direct(scenario: str) -> bool:
    return "-direct-" in scenario


def transport(scenario: str) -> str:
    return scenario.split("-", 1)[0]


def read_csv(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as f:
        return _read_csv_rows(f)


def read_csv_files(paths: list[Path]) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    for path in paths:
        records.extend(read_csv(path))
    return records


def default_csv_path(results_dir: Path) -> Path | None:
    """The CSV whose name, the run id, sorts last."""
    run_files = sorted(results_dir.glob("*.csv"), key=lambda path: path.stem)
    return run_files[-1] if run_files else None


def _read_csv_rows(lines: Iterable[str]) -> list[dict[str, str]]:
    reader = csv.DictReader(lines)
    if reader.fieldnames is None:
        raise ValueError("results CSV is empty; expected the benchmark.sh header")

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


def aggregate(records: list[dict[str, str]], metric: str) -> list[Series]:
    """Series sorted by engine and scenario, points sorted by target RPS.

    Rows without a value for `metric` (latency of a run without a single
    reply) are left out rather than plotted as 0.
    """
    if metric not in REQUIRED_COLUMNS:
        raise ValueError(f"unknown benchmark metric: {metric}")

    values: dict[SeriesKey, dict[float, list[float]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for row in records:
        rps = _parse_float(row, "target_rps")
        value = _parse_float(row, metric)
        if rps is None or value is None:
            continue
        values[SeriesKey.from_row(row)][rps].append(value)

    return [
        Series(
            key,
            tuple(sorted((rps, sum(runs) / len(runs)) for rps, runs in by_rps.items())),
        )
        for key, by_rps in sorted(values.items())
    ]


def create_plot(
    filename: Path,
    title: str,
    y_label: str,
    series_list: list[Series],
    reference_line: bool = False,
) -> None:
    fig, ax = plt.subplots(figsize=(12, 8))
    ax.set_title(title, fontsize=18)
    ax.set_xlabel("Target RPS", fontsize=14)
    ax.set_ylabel(y_label, fontsize=14)
    ax.grid(True)

    if reference_line and series_list:
        top = max(rps for series in series_list for rps, _ in series.points)
        ax.plot(
            [0, top],
            [0, top],
            color="#999",
            linestyle="--",
            linewidth=1,
            label="target",
        )

    for i, series in enumerate(series_list):
        ax.plot(
            [rps for rps, _ in series.points],
            [value for _, value in series.points],
            color=COLORS[i % len(COLORS)],
            linestyle=ENGINE_STYLES.get(series.key.engine, "-."),
            linewidth=2,
            marker="o",
            markersize=6,
            label=series.key.label,
        )

    if series_list:
        ax.legend(loc="upper left", fontsize=12, framealpha=0.9)

    fig.tight_layout()
    fig.savefig(filename, dpi=100)
    plt.close(fig)


PLOTS: tuple[tuple[str, str, str], ...] = (
    ("p50", "Median latency (p50)", "p50_ms"),
    ("p95", "Latency p95", "p95_ms"),
    ("p99", "Latency p99", "p99_ms"),
    ("max", "Max latency", "max_ms"),
)


def render(records: list[dict[str, str]], out_dir: Path) -> list[Path]:
    """Write every chart for every transport present; return their paths."""
    written: list[Path] = []
    for name in sorted({transport(row["scenario"]) for row in records}):
        rows = [row for row in records if transport(row["scenario"]) == name]

        for metric, title in (
            ("achieved_rps", "Achieved RPS (sent)"),
            ("delivered_rps", "Delivered RPS (replied)"),
        ):
            path = out_dir / f"{name}-{metric.replace('_', '-')}.png"
            create_plot(
                path,
                f"{name}: {title} vs target RPS",
                title,
                aggregate(rows, metric),
                reference_line=True,
            )
            written.append(path)

        for slug, title, metric in PLOTS:
            path = out_dir / f"{name}-latency-{slug}.png"
            create_plot(
                path,
                f"{name}: {title}, from scheduled send to reply",
                f"{title} (ms)",
                aggregate(rows, metric),
            )
            written.append(path)

        path = out_dir / f"{name}-unreceived.png"
        create_plot(
            path,
            f"{name}: updates without a reply after the completion wait",
            "Unreceived updates",
            aggregate(rows, "unreceived"),
        )
        written.append(path)
    return written


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Plot tgin benchmark CSV results.")
    parser.add_argument(
        "csv_paths",
        nargs="*",
        type=Path,
        help="CSV files to render; defaults to the ../results/<run_id>.csv with the newest run id",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        help="where to write the charts, default ./generated; PNGs already there are removed",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    here = Path(__file__).resolve().parent
    csv_paths: list[Path] = args.csv_paths
    if not csv_paths:
        newest = default_csv_path(here.parent / "results")
        if newest is None:
            print(
                "no ../results/*.csv to plot; run benchmark.sh first", file=sys.stderr
            )
            return 1
        csv_paths = [newest]
    missing_paths = [path for path in csv_paths if not path.exists()]
    if missing_paths:
        joined = ", ".join(str(path) for path in missing_paths)
        print(f"cannot open benchmark CSV path(s): {joined}", file=sys.stderr)
        return 1

    try:
        records = read_csv_files(csv_paths)
    except ValueError as exc:
        print(f"invalid benchmark results CSV: {exc}", file=sys.stderr)
        return 1

    out_dir: Path = args.out_dir or here / "generated"
    out_dir.mkdir(parents=True, exist_ok=True)
    for stale_chart in out_dir.glob("*.png"):
        stale_chart.unlink()

    for path in render(records, out_dir):
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
