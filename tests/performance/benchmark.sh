#!/usr/bin/env bash
# Run the scenario matrix and write results/<run_id>.csv.
#
#   ENGINE       v1 (default), v0, or a list such as "v0 v1": the engines
#                take turns run by run, so they share the machine's state
#   BOT_IMPL     rust (default) or py; see the makefile
#   RPS_VALUES   target rates, default "1000 5000 10000"
#   SCENARIOS    default: every scenario in the makefile; the direct
#                baselines (*-direct-*) run once per rate and run, under
#                the first engine's label
#   DURATION     send phase per run, seconds (default 30)
#   RUNS         runs per scenario, rate and engine (default 3)
#   RUN_ID       default: the UTC start time, e.g. 20261015T120000Z
#   BOT_SLEEP_MS per-update bot work, milliseconds (default 0)
#
# Next to the CSV: <run_id>.log (every run's output, including the counter
# summary the CSV leaves out) and <run_id>.stats.log (`docker stats` every
# 5 s, with the start and end of each run marked).
set -euo pipefail
cd "$(dirname "$0")"

read -r -a engines <<<"${ENGINE:-v1}"
read -r -a rps_values <<<"${RPS_VALUES:-1000 5000 10000}"
read -r -a scenarios <<<"${SCENARIOS:-webhook-direct-1 webhook-tgin-1 webhook-tgin-3 longpoll-direct-1 longpoll-tgin-1 longpoll-tgin-3}"
duration="${DURATION:-30}"
runs="${RUNS:-3}"
run_id="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
result_dir="${RESULT_DIR:-results}"
result_path="${result_dir}/${run_id}.csv"
log_path="${result_dir}/${run_id}.log"
stats_path="${result_dir}/${run_id}.stats.log"

for engine in "${engines[@]}"; do
    case "${engine}" in
        v0|v1) ;;
        *)
            echo "ENGINE must be v0, v1 or a list of them" >&2
            exit 1
            ;;
    esac
done

case "${runs}" in
    ""|*[!0-9]*|0)
        echo "RUNS must be a positive integer" >&2
        exit 1
        ;;
esac

mkdir -p "${result_dir}"
# The makefile takes one engine; build and clean do not depend on it.
make build ENGINE="${engines[0]}"
for engine in "${engines[@]}"; do
    make image ENGINE="${engine}"
done

docker compose run --rm --no-deps -T bench tgin-bench csv-header >"${result_path}"

# CPU and memory of the stack, every 5 s, for the whole matrix.
(
    while :; do
        echo "== $(date -u +%H:%M:%S)"
        docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}' | grep '^tgin-bench' || true
        sleep 5
    done
) >>"${stats_path}" 2>&1 &
sampler=$!
trap 'kill "${sampler}" 2>/dev/null || true' EXIT

one_run() { # engine scenario rps run
    local marker="$2 $1 $3 RPS (run $4/${runs})"
    echo "## $(date -u +%H:%M:%S) start ${marker}" >>"${stats_path}"
    echo "Running ${marker} -> ${result_path}"
    if ! make "$2" \
        ENGINE="$1" \
        RPS="$3" \
        DURATION="${duration}" \
        BENCH_FORMAT=csv-row \
        BENCH_OUTPUT="${result_path}"; then
        echo "FAILED: ${marker}; no row written"
    fi
    echo "## $(date -u +%H:%M:%S) end ${marker}" >>"${stats_path}"
}

{
    for rps in "${rps_values[@]}"; do
        for run in $(seq 1 "${runs}"); do
            for scenario in "${scenarios[@]}"; do
                case "${scenario}" in
                    *-direct-*) one_run "${engines[0]}" "${scenario}" "${rps}" "${run}" ;;
                    *)
                        for engine in "${engines[@]}"; do
                            one_run "${engine}" "${scenario}" "${rps}" "${run}"
                        done
                        ;;
                esac
            done
        done
    done
    make clean ENGINE="${engines[0]}"
} 2>&1 | tee -a "${log_path}"
failures=$(grep -c '^FAILED: ' "${log_path}" || true)

echo "Wrote benchmark results to ${result_path}"
if [ "${failures}" -gt 0 ]; then
    echo "${failures} run(s) failed" >&2
    exit 1
fi
