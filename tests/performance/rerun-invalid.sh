#!/usr/bin/env bash
# Rerun the invalid runs of a matrix until every point has RUNS valid runs
# per engine. A run is invalid when the generator fell behind its schedule:
# lag_p99_ms above MAX_LAG_MS (0.5; undisturbed runs show 0.02-0.08).
#
#   ./rerun-invalid.sh results/<run_id>.csv
#
# New rows go to results/<run_id>-rerun.csv (output in <run_id>-rerun.log);
# the original CSV is left as it is. Gives up on a point after MAX_TRIES
# reruns of one engine (default 6).
set -euo pipefail
cd "$(dirname "$0")"

csv="$1"
runs="${RUNS:-3}"
max_lag="${MAX_LAG_MS:-0.5}"
max_tries="${MAX_TRIES:-6}"
duration="${DURATION:-30}"
rerun_csv="${csv%.csv}-rerun.csv"
log_path="${csv%.csv}-rerun.log"

[ -f "${rerun_csv}" ] || head -n 1 "${csv}" >"${rerun_csv}"

# Valid runs per scenario,engine,rate over both files.
valid() {
    tail -q -n +2 "${csv}" "${rerun_csv}" |
        awk -F, -v s="$1" -v e="$2" -v r="$3" -v m="${max_lag}" \
            '$1 == s && $2 == e && $3 == r && $13 + 0 <= m { n++ } END { print n + 0 }'
}

points=$(tail -n +2 "${csv}" | cut -d, -f1-3 | sort -u)
failed=0
{
    for point in ${points}; do
        IFS=, read -r scenario engine rps <<<"${point}"
        tries=0
        while [ "$(valid "${scenario}" "${engine}" "${rps}")" -lt "${runs}" ]; do
            if [ "${tries}" -ge "${max_tries}" ]; then
                echo "GAVE UP: ${scenario} ${engine} ${rps} RPS after ${tries} reruns"
                failed=1
                break
            fi
            tries=$((tries + 1))
            echo "Rerunning ${scenario} ${engine} ${rps} RPS (rerun ${tries})"
            make "${scenario}" ENGINE="${engine}" RPS="${rps}" DURATION="${duration}" \
                BENCH_FORMAT=csv-row BENCH_OUTPUT="${rerun_csv}" || true
        done
    done
    make clean
    echo "failed=${failed}"
} 2>&1 | tee -a "${log_path}"
tail -n 1 "${log_path}" | grep -q '^failed=0$'
