#!/bin/bash
# Runs on one bench host, from /data/src, with nothing else on it. Answers whether the engine's tail
# latency at large MAX_ORDERS is the engine or the machine, and what symbol count really costs.
#
# Each of the 4 cores has 2 threads: thread N and N+4 share a core, so the siblings stay idle.
set -euo pipefail

export PREBUILT=1 ENGINE_CPUS=1 BENCH_CPUS=2,3 DRIVER_CPUS=0 AERON_JAR=/data/aeron-all.jar PERF_DIR=/data/perf
out=/data/engine-results
mkdir -p "$out" "$PERF_DIR"

for cpu in 0 1 2 3; do
    echo performance | sudo tee /sys/devices/system/cpu/cpu$cpu/cpufreq/scaling_governor > /dev/null 2>&1 || true
done

sudo sysctl -q kernel.perf_event_paranoid=-1

hugepages() {
    echo "$1" | sudo tee /sys/kernel/mm/transparent_hugepage/enabled > /dev/null
}

# The machine alone: if this shows the same stalls, the tail isn't the engine's.
for cpu in 1 2; do
    taskset -c $cpu target/release/mmx-jitter --seconds 30 > "$out/jitter-cpu$cpu.json"
done

for mode in never always; do
    hugepages "$mode"
    python3 tools/scripts/profile.py run --sweeps capacity --seed 1 | tee "$out/capacity-$mode.log"
done
hugepages always
python3 tools/scripts/profile.py run --sweeps symbols,book_depth --seed 1 | tee "$out/symbols.log"

cp -r .run/profile "$out/runs"
for data in "$PERF_DIR"/*.data; do
    perf report -i "$data" --no-children --sort symbol --stdio 2> /dev/null | grep -v '^#' | sed -n 1,25p > "$data.txt"
done
cp "$PERF_DIR"/*.txt "$out/"
touch "$out/done"
