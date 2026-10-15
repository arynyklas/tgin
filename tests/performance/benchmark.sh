#!/usr/bin/env bash
# Run the scenario matrix against one engine and write results/<run_id>.csv.
#
#   ENGINE       v1 (default) or v0; see the makefile
#   RPS_VALUES   target rates, default "1000 5000 10000"
#   SCENARIOS    default: every scenario in the makefile
#   DURATION     send phase per run, seconds (default 30)
#   RUNS         runs per scenario and rate (default 3)
#   RUN_ID       default: the UTC start time, e.g. 20261015T120000Z
#   BOT_SLEEP_MS per-update bot work, milliseconds (default 0)
set -euo pipefail
cd "$(dirname "$0")"

engine="${ENGINE:-v1}"
read -r -a rps_values <<<"${RPS_VALUES:-1000 5000 10000}"
read -r -a scenarios <<<"${SCENARIOS:-webhook-direct-1 webhook-tgin-1 webhook-tgin-3 longpoll-direct-1 longpoll-tgin-1 longpoll-tgin-3}"
duration="${DURATION:-30}"
runs="${RUNS:-3}"
run_id="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
result_dir="${RESULT_DIR:-results}"
result_path="${result_dir}/${run_id}.csv"

case "${engine}" in
    v0|v1) ;;
    *)
        echo "ENGINE must be v0 or v1" >&2
        exit 1
        ;;
esac

case "${runs}" in
    ""|*[!0-9]*|0)
        echo "RUNS must be a positive integer" >&2
        exit 1
        ;;
esac

mkdir -p "${result_dir}"
make build ENGINE="${engine}"
make image ENGINE="${engine}"

docker compose run --rm --no-deps -T bench tgin-bench csv-header >"${result_path}"

failures=0
for rps in "${rps_values[@]}"; do
    for scenario in "${scenarios[@]}"; do
        for run in $(seq 1 "${runs}"); do
            echo "Running ${scenario} on ${engine} at ${rps} RPS (run ${run}/${runs}) -> ${result_path}"
            if ! make "${scenario}" \
                ENGINE="${engine}" \
                RPS="${rps}" \
                DURATION="${duration}" \
                BENCH_FORMAT=csv-row \
                BENCH_OUTPUT="${result_path}"; then
                echo "FAILED: ${scenario} on ${engine} at ${rps} RPS (run ${run}/${runs}); no row written" >&2
                failures=$((failures + 1))
            fi
        done
    done
done
make clean ENGINE="${engine}"

echo "Wrote benchmark results to ${result_path}"
if [ "${failures}" -gt 0 ]; then
    echo "${failures} run(s) failed" >&2
    exit 1
fi
