#!/bin/sh
# Chaos for the live-gate soak: induced restarts of the account container
# and cuts of its network, on a fixed schedule from the soak's start (t0,
# kept in /data/chaos-t0, so a restart of this container keeps the
# schedule). Events go to /data/chaos.csv as `unix_s,kind,detail`.
#
# Schedule, in seconds after t0:
#   restarts  900, then every 21600 (6 h); alternately hard (`docker kill
#             -s KILL`, then `docker start`: the account gets no SIGTERM)
#             and graceful (`docker restart -t 10`: SIGTERM, then SIGKILL
#             after 10 s), starting hard;
#   cuts      1800 for 90 s, 2400 for 600 s, then 10800 + k * 21600,
#             alternately 90 s and 600 s (`docker network disconnect`);
#   604800    the 7-day window ends: no more chaos; the polling goes on.
# The offset of the last event carried out is kept in /data/chaos-done: a
# restart of this container goes on after it. An event more than 120 s
# overdue (this container was down) is logged as skipped. Every 30 s the
# account's restart count and state are polled and logged when they change.
set -u

ACCOUNT="${ACCOUNT:-tgin-soak-account}"
NET="${NET:-tgin-soak-net}"
LOG=/data/chaos.csv
T0FILE=/data/chaos-t0
DONEFILE=/data/chaos-done
WINDOW=604800
EVERY=21600

now() { date -u +%s; }
log() { echo "$(now),$1,$(echo "$2" | tr ',' ';')" >>"$LOG"; }

[ -s "$T0FILE" ] || now >"$T0FILE"
T0=$(cat "$T0FILE")
log chaos-start "t0 $T0"
DONE=$(cat "$DONEFILE" 2>/dev/null || echo -1)
# A hard kill whose `docker start` this container did not get to: start it.
if [ "$(docker inspect -f '{{.State.Running}}' "$ACCOUNT" </dev/null 2>&1)" = false ]; then
    docker start "$ACCOUNT" </dev/null >/dev/null 2>&1 && log started "the account was not running"
fi
# A cut cut short by a restart of this container: connect again.
if docker network connect "$NET" "$ACCOUNT" </dev/null >/dev/null 2>&1; then
    log reconnect "a cut was left open"
fi

schedule() {
    k=0
    t=900
    while [ "$t" -lt "$WINDOW" ]; do
        if [ $((k % 2)) -eq 0 ]; then m=hard; else m=graceful; fi
        echo "$t restart $m"
        k=$((k + 1))
        t=$((t + EVERY))
    done
    echo "1800 cut 90"
    echo "2400 cut 600"
    k=0
    t=10800
    while [ "$t" -lt "$WINDOW" ]; do
        if [ $((k % 2)) -eq 0 ]; then d=90; else d=600; fi
        echo "$t cut $d"
        k=$((k + 1))
        t=$((t + EVERY))
    done
    echo "$WINDOW end -"
}

LAST=""
poll() {
    s=$(docker inspect -f '{{.RestartCount}} {{.State.Status}} {{.State.StartedAt}}' "$ACCOUNT" </dev/null 2>&1)
    if [ "$s" != "$LAST" ]; then
        log account-state "restarts/status/started $s"
        LAST="$s"
    fi
}

wait_until() {
    while [ "$(now)" -lt "$1" ]; do
        poll
        left=$(($1 - $(now)))
        [ "$left" -gt 30 ] && left=30
        [ "$left" -gt 0 ] && sleep "$left"
    done
}

schedule | sort -n >/tmp/schedule
while read -r off kind arg; do
    at=$((T0 + off))
    [ "$off" -le "$DONE" ] && continue
    if [ "$(now)" -gt $((at + 120)) ]; then
        log skipped "$kind $arg due $at"
        continue
    fi
    wait_until "$at"
    case "$kind" in
    restart)
        log "restart-$arg" "$arg"
        if [ "$arg" = hard ]; then
            docker kill -s KILL "$ACCOUNT" </dev/null >/dev/null 2>&1 || log error "kill failed"
            sleep 1
            docker start "$ACCOUNT" </dev/null >/dev/null 2>&1 || log error "start failed"
        else
            docker restart -t 10 "$ACCOUNT" </dev/null >/dev/null 2>&1 || log error "restart failed"
        fi
        ;;
    cut)
        log cut-start "$arg s"
        docker network disconnect "$NET" "$ACCOUNT" </dev/null >/dev/null 2>&1 || log error "disconnect failed"
        wait_until $(($(now) + arg))
        docker network connect "$NET" "$ACCOUNT" </dev/null >/dev/null 2>&1 || log error "connect failed"
        log cut-end "$arg s"
        ;;
    end)
        log window-end "7 d"
        ;;
    esac
    echo "$off" >"$DONEFILE"
done </tmp/schedule

while :; do
    poll
    sleep 60
done
