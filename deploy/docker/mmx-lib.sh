#!/bin/sh
# Shared by mmx-entrypoint and mmx-node.

log() {
    echo "mmx: $*" >&2
}

# Prints the CPUs this process may use, one per line, from a list such as "0-3,8".
allowed_cpus() {
    list=$(sed -n 's/^Cpus_allowed_list:[[:space:]]*//p' /proc/self/status)
    old_ifs=$IFS
    IFS=,
    for range in $list; do
        cpu=${range%-*}
        last=${range#*-}
        while [ "$cpu" -le "$last" ]; do
            echo "$cpu"
            cpu=$((cpu + 1))
        done
    done
    IFS=$old_ifs
}

# Prints lines $2 to the end of the newline-separated list $1, comma separated.
cpus_from() {
    printf '%s\n' "$1" | tail -n +"$2" | paste -sd, -
}

# The address of cluster member $1, published by the StatefulSet's headless Service.
member_host() {
    echo "$CLUSTER_NAME-$1.$CLUSTER_NAME"
}

# StatefulSet pods end in their ordinal.
node_id() {
    if [ -n "${NODE_ID:-}" ]; then
        echo "$NODE_ID"
        return
    fi
    host=${HOSTNAME:-$(cat /proc/sys/kernel/hostname)}
    echo "${host##*-}"
}
