#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 2 ]; then
    echo "Usage: $0 <port> <restart_script> [restart_script_args...]"
    echo "  Watchdog: probe /health every 3s (2s timeout)."
    echo "  Restart after 3 consecutive failures."
    exit 1
fi

PORT="$1"
RESTART_SCRIPT="$2"
shift 2

URL="http://127.0.0.1:${PORT}/health"
LOG="boom_gateway_watchdog.log"
RUN_LOG="boom_gateway_run.log"
FAIL=0
RESTARTS=0
START_EPOCH=$(date +%s)
LAST_HEARTBEAT=$(date +%s)

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG"
}

log "watchdog started — port=${PORT} restart=${RESTART_SCRIPT}"

while true; do
    NOW=$(date +%s)

    # Heartbeat: print uptime every ~60s when healthy.
    if [ $((NOW - LAST_HEARTBEAT)) -ge 60 ] && [ "$FAIL" -eq 0 ]; then
        UPTIME=$(( NOW - START_EPOCH ))
        DAYS=$(( UPTIME / 86400 ))
        HOURS=$(( (UPTIME % 86400) / 3600 ))
        MINS=$(( (UPTIME % 3600) / 60 ))
        log "heartbeat — uptime ${DAYS}d${HOURS}h${MINS}m, restarts ${RESTARTS}"
        LAST_HEARTBEAT=$NOW
    fi

    START=$(date +%s%N)
    if curl -sf --max-time 2 "$URL" > /dev/null 2>&1; then
        if [ "$FAIL" -gt 0 ]; then
            log "recovered after ${FAIL} failure(s)"
        fi
        FAIL=0
    else
        FAIL=$((FAIL + 1))
        log "health check FAIL ${FAIL}/3"
        if [ "$FAIL" -ge 3 ]; then
            RESTARTS=$((RESTARTS + 1))
            log "RESTART #${RESTARTS} — running ${RESTART_SCRIPT}"
            bash "$RESTART_SCRIPT" "$@" >> "$RUN_LOG" 2>&1 &
            RESTART_PID=$!
            log "restart script running as PID ${RESTART_PID}, logs >> ${RUN_LOG}"
            wait "$RESTART_PID" 2>/dev/null || true
            log "restart script exited, waiting 10s before resuming probes"
            sleep 10
            FAIL=0
            LAST_HEARTBEAT=$(date +%s)
        fi
    fi

    ELAPSED=$(( ($(date +%s%N) - START) / 1000000 ))
    REMAINING=$(( 3000 - ELAPSED ))
    if [ "$REMAINING" -gt 0 ]; then
        sleep $(( REMAINING / 1000 )).$(( REMAINING % 1000 ))
    fi
done
