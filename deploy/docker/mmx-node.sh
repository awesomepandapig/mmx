#!/bin/sh
# Runs Aeron's Java side.
#
#   mmx-node cluster    a cluster member: media driver, Archive and consensus module
#   mmx-node driver     a media driver for a gateway or consumer
#   mmx-node render     print a cluster member's properties and exit
#
# Properties come from tools/bench/ec2/bench.py and tools/scripts/cluster.py, with each setting as an
# environment variable so the chart can change it. EXTRA_PROPS (separated by ";") and
# EXTRA_PROPS_FILE come last, so they win.
set -eu
. "${MMX_LIB:-/usr/local/lib/mmx/mmx-lib.sh}"

JAR=${AERON_JAR:-/opt/aeron/aeron-all.jar}
PROPERTIES=${NODE_PROPERTIES:-/tmp/node.properties}
AERON_DIR=${AERON_DIR:-/dev/shm/aeron}
DATA_DIR=${DATA_DIR:-/data}
CLUSTER_SIZE=${CLUSTER_SIZE:-3}
DNS_WAIT_SECONDS=${DNS_WAIT_SECONDS:-300}

# Java caches lookups for 30 seconds, but a restarted member comes back on a new IP.
JAVA_FLAGS="--add-opens java.base/jdk.internal.misc=ALL-UNNAMED
--add-opens java.base/java.util.zip=ALL-UNNAMED
-XX:+UseZGC -XX:-UsePerfData
-Dsun.net.inetaddr.ttl=${DNS_CACHE_SECONDS:-5} -Dsun.net.inetaddr.negative.ttl=1"

members() {
    i=0
    list=""
    while [ "$i" -lt "$CLUSTER_SIZE" ]; do
        h=$(member_host "$i")
        entry="$i,$h:$((20000 + i)),$h:$((20010 + i)),$h:$((20020 + i)),$h:$((20030 + i)),$h:$((8010 + i))"
        list="$list${list:+|}$entry"
        i=$((i + 1))
    done
    echo "$list"
}

extra_properties() {
    if [ -n "${EXTRA_PROPS:-}" ]; then
        printf '%s\n' "$EXTRA_PROPS" | tr ';' '\n'
    fi
    if [ -n "${EXTRA_PROPS_FILE:-}" ] && [ -f "$EXTRA_PROPS_FILE" ]; then
        cat "$EXTRA_PROPS_FILE"
    fi
}

cluster_properties() {
    id=$(node_id)
    me=$(member_host "$id")
    sync=${SYNC:-0}
    cat <<END
aeron.dir=$AERON_DIR
aeron.dir.delete.on.start=true
aeron.threading.mode=${THREADING:-DEDICATED}
aeron.term.buffer.sparse.file=${SPARSE_TERM_BUFFERS:-false}
aeron.spies.simulate.connection=true
aeron.socket.so_sndbuf=${SOCKET_BUFFER:-4m}
aeron.socket.so_rcvbuf=${SOCKET_BUFFER:-4m}
aeron.rcv.initial.window.length=${SOCKET_BUFFER:-4m}
aeron.archive.dir=$DATA_DIR/archive
aeron.archive.control.channel=aeron:udp?endpoint=$me:$((8010 + id))
aeron.archive.control.response.channel=aeron:udp?endpoint=$me:0
aeron.archive.replication.channel=aeron:udp?endpoint=$me:0
aeron.archive.threading.mode=SHARED
aeron.archive.recording.events.enabled=false
aeron.archive.file.sync.level=$sync
aeron.archive.catalog.file.sync.level=$sync
aeron.cluster.dir=$DATA_DIR/cluster
aeron.cluster.id=${CLUSTER_ID:-0}
aeron.cluster.member.id=$id
aeron.cluster.members=$(members)
aeron.cluster.ingress.channel=aeron:udp
aeron.cluster.log.channel=aeron:udp?term-length=${LOG_TERM_LENGTH:-16m}|control-mode=manual|control=$me:$((20040 + id))
aeron.cluster.replication.channel=aeron:udp?endpoint=$me:0
aeron.cluster.clock=io.aeron.cluster.NanosecondClusterClock
aeron.cluster.leader.heartbeat.timeout=${LEADER_HEARTBEAT_TIMEOUT:-500ms}
aeron.cluster.leader.heartbeat.interval=${LEADER_HEARTBEAT_INTERVAL:-100ms}
aeron.cluster.session.timeout=${SESSION_TIMEOUT:-30s}
aeron.cluster.startup.canvass.timeout=${STARTUP_CANVASS_TIMEOUT:-15s}
aeron.cluster.election.timeout=${ELECTION_TIMEOUT:-500ms}
aeron.cluster.service.count=1
END
    if [ -n "${IDLE:-}" ]; then
        cat <<END
aeron.sender.idle.strategy=$IDLE
aeron.receiver.idle.strategy=$IDLE
aeron.cluster.idle.strategy=$IDLE
END
    fi
    extra_properties
}

driver_properties() {
    cat <<END
aeron.dir=$AERON_DIR
aeron.dir.delete.on.start=true
aeron.threading.mode=${THREADING:-SHARED}
aeron.socket.so_sndbuf=${SOCKET_BUFFER:-4m}
aeron.socket.so_rcvbuf=${SOCKET_BUFFER:-4m}
aeron.rcv.initial.window.length=${SOCKET_BUFFER:-4m}
END
    extra_properties
}

# Starting early would crash the driver on a name that isn't published yet. A member's own name must
# also resolve to this pod, because a restarted pod can briefly see its old address.
await_members() {
    deadline=$(($(date +%s) + DNS_WAIT_SECONDS))
    self=$(node_id)
    i=0
    while [ "$i" -lt "$CLUSTER_SIZE" ]; do
        h=$(member_host "$i")
        while :; do
            address=$(getent ahostsv4 "$h" 2>/dev/null | head -n 1 | cut -d' ' -f1) || address=""
            if [ -n "$address" ]; then
                if [ "$i" != "$self" ] || [ -z "${POD_IP:-}" ] || [ "$address" = "$POD_IP" ]; then
                    break
                fi
            fi
            if [ "$(date +%s)" -gt "$deadline" ]; then
                log "$h didn't resolve within ${DNS_WAIT_SECONDS}s"
                exit 1
            fi
            sleep 1
        done
        i=$((i + 1))
    done
}

# Moves the first thread of $1 whose name contains $2 to CPU $3. Thread names are cut to 15
# characters, which is why consensus-module is matched on a prefix.
pin_thread() {
    for task in /proc/"$1"/task/*; do
        comm=$(cat "$task/comm" 2>/dev/null) || continue
        case "$comm" in
            *"$2"*)
                taskset -p -c "$3" "${task##*/}" >/dev/null
                log "pinned $comm to CPU $3"
                return 0
                ;;
        esac
    done
    return 1
}

# The latency-critical threads each get a CPU, as in tools/bench/ec2/bench.py. Java starts on the
# remaining CPUs, so every other thread stays off these.
pin_threads() {
    pending="consensus-mo:$2 sender:$3 receiver:$4"
    deadline=$(($(date +%s) + 120))
    while [ -n "$pending" ] && [ "$(date +%s)" -lt "$deadline" ]; do
        left=""
        for entry in $pending; do
            pin_thread "$1" "${entry%:*}" "${entry##*:}" || left="$left $entry"
        done
        pending=${left# }
        sleep 1
    done
    if [ -n "$pending" ]; then
        log "these threads never appeared: $pending"
    fi
}

run_java() {
    main_class=$1
    # JAVA_FLAGS is split on purpose.
    # shellcheck disable=SC2086
    set -- $JAVA_FLAGS -Xms"${JAVA_HEAP_MIN:-512m}" -Xmx"${JAVA_HEAP_MAX:-2g}" -cp "$JAR" \
        "$main_class" "$PROPERTIES"
    if [ "${PIN_THREADS:-false}" = true ] && [ "$main_class" = io.aeron.cluster.ClusteredMediaDriver ]; then
        cpus=$(allowed_cpus)
        count=$(printf '%s\n' "$cpus" | wc -l)
        if [ "$count" -lt 4 ]; then
            log "PIN_THREADS needs 4 CPUs and found $count, so nothing is pinned"
        else
            consensus=$(printf '%s\n' "$cpus" | sed -n 1p)
            sender=$(printf '%s\n' "$cpus" | sed -n 2p)
            receiver=$(printf '%s\n' "$cpus" | sed -n 3p)
            pid=$$
            pin_threads "$pid" "$consensus" "$sender" "$receiver" &
            exec taskset -c "$(cpus_from "$cpus" 4)" java "$@"
        fi
    fi
    exec java "$@"
}

case "${1:-cluster}" in
    render)
        cluster_properties
        ;;
    cluster)
        mkdir -p "$DATA_DIR/archive" "$DATA_DIR/cluster"
        await_members
        cluster_properties >"$PROPERTIES"
        run_java io.aeron.cluster.ClusteredMediaDriver
        ;;
    driver)
        driver_properties >"$PROPERTIES"
        run_java io.aeron.driver.MediaDriver
        ;;
    *)
        exec "$@"
        ;;
esac
