#!/usr/bin/env bash
set -euo pipefail

usecases=(
    "webhook-direct-1" "webhook-tgin-1" "webhook-tgin-scale-2" "webhook-tgin-scale-3" "webhook-tgin-scale-5" "webhook-tgin-scale-10"
    "longpoll-direct-1" "longpoll-tgin-1" "longpoll-tgin-scale-2" "longpoll-tgin-scale-3" "longpoll-tgin-scale-5" "longpoll-tgin-scale-10"
)
rps_values=(500 1000 2000 5000 8000 10000)

duration="${DURATION:-30}"
runs="${RUNS:-3}"
run_id="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
git_sha="${GIT_SHA:-$(git rev-parse --short HEAD)}"
result_dir="${RESULT_DIR:-results}"
result_path="${RESULT_PATH:-${result_dir}/${run_id}.csv}"

scenario_metadata() {
    local mode=$1

    case "${mode}" in
        *-scale-*)
            scenario_family="scale"
            ;;
        *)
            scenario_family="overhead"
            ;;
    esac

    case "${mode}" in
        webhook*)
            transport="webhook"
            route_path="/webhook"
            ;;
        longpoll-direct-1)
            transport="longpoll"
            route_path="/bot123:test/getUpdates"
            ;;
        longpoll-tgin-1)
            transport="longpoll"
            route_path="/bot1/getUpdates"
            ;;
        longpoll-tgin-scale-*)
            transport="longpoll"
            route_path="/bot*/getUpdates"
            ;;
        *)
            echo "unknown benchmark mode: ${mode}" >&2
            return 1
            ;;
    esac

    case "${mode}" in
        *-scale-*)
            bot_count="${mode##*-}"
            ;;
        *-1)
            bot_count=1
            ;;
        *)
            echo "cannot infer bot count for benchmark mode: ${mode}" >&2
            return 1
            ;;
    esac
}

case "${runs}" in
    ""|*[!0-9]*|0)
        echo "RUNS must be a positive integer" >&2
        exit 1
        ;;
esac

mkdir -p "${result_dir}"
make clean
make build

docker compose up -d bench
docker compose exec -T bench tgin-bench --format csv-header > "${result_path}"
docker compose down

for rps in "${rps_values[@]}"; do
    for mode in "${usecases[@]}"; do
        scenario_metadata "${mode}"
        for run in $(seq 1 "${runs}"); do
            echo "Running ${mode} at ${rps} RPS (run ${run}/${runs}) -> ${result_path}"
            make "${mode}" \
                RPS="${rps}" \
                DURATION="${duration}" \
                BENCH_FORMAT=csv-row \
                RUN_ID="${run_id}" \
                GIT_SHA="${git_sha}" \
                SCENARIO_FAMILY="${scenario_family}" \
                TRANSPORT="${transport}" \
                ROUTE_PATH="${route_path}" \
                SCENARIO="${mode}" \
                BOT_COUNT="${bot_count}" \
                BENCH_OUTPUT="${result_path}"
        done
    done
done

echo "Wrote benchmark results to ${result_path}"
