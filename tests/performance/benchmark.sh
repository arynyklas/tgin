#!/usr/bin/env bash
# Written in [Amber](https://amber-lang.com/)
# version: 0.5.1-alpha
# Amber is not currently available in this workspace; keep this generated script
# synchronized with benchmark.ab when editing either file.
set -euo pipefail

usecases=(
    "webhook-direct" "webhook-tgin" "webhook-tgin-3" "webhook-tgin-4" "webhook-tgin-5" "webhook-tgin-10"
    "longpull-direct" "longpull-tgin" "longpull-tgin-3" "longpull-tgin-4" "longpull-tgin-5" "longpull-tgin-10"
)
rps_values=(500 1000 2000 5000 8000 10000)

duration="${DURATION:-10}"
run_id="${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
git_sha="${GIT_SHA:-$(git rev-parse --short HEAD)}"
result_dir="${RESULT_DIR:-results}"
scenario_family="${SCENARIO_FAMILY:-performance}"
result_path="${RESULT_PATH:-${result_dir}/${run_id}.csv}"

scenario_metadata() {
    local mode=$1

    case "${mode}" in
        webhook*)
            transport="webhook"
            route_path="/webhook"
            ;;
        longpull-direct)
            transport="longpoll"
            route_path="/bot123:test/getUpdates"
            ;;
        longpull*)
            transport="longpoll"
            route_path="/bot*/getUpdates"
            ;;
        *)
            echo "unknown benchmark mode: ${mode}" >&2
            return 1
            ;;
    esac

    case "${mode}" in
        *-direct)
            bot_count=1
            ;;
        *-tgin)
            bot_count=2
            ;;
        *-tgin-*)
            bot_count="${mode##*-}"
            ;;
        *)
            echo "cannot infer bot count for benchmark mode: ${mode}" >&2
            return 1
            ;;
    esac
}

mkdir -p "${result_dir}"
make clean
make build

docker compose up -d bench
docker compose exec -T bench tgin-bench --format csv-header > "${result_path}"
docker compose down

for rps in "${rps_values[@]}"; do
    for mode in "${usecases[@]}"; do
        scenario_metadata "${mode}"
        echo "Running ${mode} at ${rps} RPS -> ${result_path}"
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

echo "Wrote benchmark results to ${result_path}"
