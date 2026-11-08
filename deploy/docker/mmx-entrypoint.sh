#!/bin/sh
# Starts a Rust service. Waits for its media driver when AERON_DIR is set, works out the engine's
# per-member channels, and with PIN_THREADS=true pins the engine's main thread to a CPU of its own.
set -eu
. "${MMX_LIB:-/usr/local/lib/mmx/mmx-lib.sh}"

if [ -n "${AERON_DIR:-}" ]; then
    log "waiting for the media driver in $AERON_DIR"
    until [ -f "$AERON_DIR/cnc.dat" ]; do
        sleep 0.1
    done
    sleep "${AERON_SETTLE_SECONDS:-2}"
fi

# Environment variables can't do arithmetic, so a member's ports are worked out here. The offsets
# match tools/scripts/cluster.py, and the mmx-cluster-* tools depend on them.
if [ -n "${CLUSTER_ID:-}" ] && [ -n "${CLUSTER_NAME:-}" ]; then
    id=$(node_id)
    me=$(member_host "$id")
    PUB_CHANNEL="aeron:udp?control=$me:$((40450 + id))|control-mode=dynamic|term-length=${REPORT_TERM_LENGTH:-64m}"
    ARCHIVE_CONTROL="aeron:udp?endpoint=$me:$((8010 + id))"
    export PUB_CHANNEL ARCHIVE_CONTROL
fi

if [ "${PIN_THREADS:-false}" = true ]; then
    cpus=$(allowed_cpus)
    count=$(printf '%s\n' "$cpus" | wc -l)
    if [ "$count" -lt 2 ]; then
        log "PIN_THREADS needs 2 CPUs and found $count, so nothing is pinned"
    else
        # Pin only the main thread, after the client's other threads have started on the rest, as
        # tools/bench/ec2/bench.py does. The process keeps this PID across exec.
        main=$(printf '%s\n' "$cpus" | head -n 1)
        rest=$(cpus_from "$cpus" 2)
        log "pinning the main thread to CPU $main, and other threads to $rest"
        pid=$$
        ( sleep "${PIN_DELAY_SECONDS:-1}"; taskset -p -c "$main" "$pid" >/dev/null ) &
        exec taskset -c "$rest" "$@"
    fi
fi

exec "$@"
