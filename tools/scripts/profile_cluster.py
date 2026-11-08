#!/usr/bin/env python3
"""Sweep the three-node cluster's settings and report them with charts.

    tools/scripts/profile_cluster.py plan --target ec2      print the runs and what they would cost
    tools/scripts/profile_cluster.py run --target local --quick
    tools/scripts/profile_cluster.py run --target ec2 --stop-after 200
    tools/scripts/profile_cluster.py run --target ec2 --resume 20261105T120000Z
    tools/scripts/profile_cluster.py report RUN [--findings FILE]

This measures what a deployment gets: one order per message from a paced client, no batching, every
order through consensus and back as a report. The cluster starts empty for every run, and the most
important questions run first, so a run stopped early still answers them.

Latency is measured from each order's scheduled send, so a stall can't hide the queue behind it.
The first seconds of each run are left out, because the JVM is still compiling. A run counts only if
the cluster stayed on one leader, nothing was rejected and the client kept up with the offered rate.
"""
import argparse
import html
import json
import math
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

from profile import CSS, SCRIPT, latency_text, load_chart, shell

ROOT = Path(__file__).resolve().parents[2]
RESULTS = ROOT / ".run" / "profile-cluster"
ENGINES = RESULTS / "engines"
TOOLS = ROOT / "target" / "release"
IS_MAC = platform.system() == "Darwin"

REPEATS = 3
FAILOVER_REPEATS = 5
SEED = 1
STOP_AFTER_FAILURES = 3
SUSTAINABLE_P99_US = 5000
SUSTAINABLE_ACHIEVED = 0.99
BACKOFF = "org.agrona.concurrent.BackoffIdleStrategy"
BUSY_SPIN = "org.agrona.concurrent.BusySpinIdleStrategy"

# What the Helm chart ships, apart from sync, which every comparison runs both ways.
SHIPPED = {"symbols": 1, "term": "16m", "threading": "DEDICATED", "idle": "backoff", "pinned": False, "store": "nvme",
           "heartbeat": "500ms"}

# Each one changes a single setting from what ships. The target says which it can run.
VARIATIONS = {
    "term": ("Log term length", ["64m", "256m"]),
    "threading": ("Driver threads", ["SHARED_NETWORK", "SHARED"]),
    "idle": ("Idle strategy", ["busy-spin"]),
    "pinned": ("Threads pinned to cores", [True]),
    "store": ("Tape disk", ["ebs"]),
    "symbols": ("Symbols", [10000]),
}

INSTANCE_DOLLARS_PER_HOUR = 0.4032
ADDRESS_DOLLARS_PER_HOUR = 0.005
HOSTS = 4


class Target:
    """A place to run the cluster. Both drive a script that takes its settings from the environment."""
    script = None
    can_vary = ()
    node_log = ""

    def __init__(self):
        self.env = {}

    def control(self, *command, timeout=120):
        return subprocess.run([sys.executable, "-I", self.script, *map(str, command)], capture_output=True, text=True,
                              env={**os.environ, **self.env}, timeout=timeout, stdin=subprocess.DEVNULL, cwd=ROOT)

    def up(self, settings, sync, engine, max_orders):
        self.env = self.settings_env(settings, sync, engine, max_orders)
        result = self.control("up", timeout=180)
        if result.returncode:
            raise RuntimeError(f"cluster didn't start: {(result.stdout + result.stderr)[-300:]}")

    def leader(self):
        deadline = time.time() + 45
        while time.time() < deadline:
            found = self.control("leader").stdout.split()
            if len(found) == 1:
                return int(found[0])
            time.sleep(0.5)
        raise RuntimeError("no single leader after 45s")

    def down(self):
        self.control("down")

    def kill(self, node):
        self.control("kill", node)

    def overhead_seconds(self):
        return self.startup_seconds


class Local(Target):
    name = "local"
    script = str(ROOT / "tools/scripts/cluster.py")
    can_vary = ("term", "threading", "idle", "symbols")
    rates = [5_000, 10_000, 25_000, 50_000, 100_000]
    reference_rate = 25_000
    seconds = 6
    warmup_seconds = 1
    soak_seconds = 20
    soak_rate = 25_000
    max_orders = 1_000_000
    tape_orders = 1_000_000
    soak_orders = 1_000_000
    heartbeats = ["500ms", "1s"]
    startup_seconds = 16
    tape_dir = ROOT / ".run" / "tapes" / "cluster"
    tool_env = {"AERON_DIR": "/Volumes/DevShm/cluster-client"}

    def preflight(self):
        for name in ("aeronmd", "mmx-bench"):
            if shell("pgrep", "-x", name):
                sys.exit(f"{name} is running, which would skew every result")
        if shell("pgrep", "-f", "[C]lusteredMediaDriver"):
            sys.exit("a cluster is running, which would skew every result")
        self.tape_dir.mkdir(parents=True, exist_ok=True)

    def settings_env(self, settings, sync, engine, max_orders):
        idle = BUSY_SPIN if settings["idle"] == "busy-spin" else BACKOFF
        props = [f"aeron.archive.file.sync.level={sync}", f"aeron.archive.catalog.file.sync.level={sync}",
                 f"aeron.sender.idle.strategy={idle}", f"aeron.receiver.idle.strategy={idle}",
                 f"aeron.cluster.idle.strategy={idle}"]
        env = {"THREADING": settings["threading"], "LOG_TERM_LENGTH": settings["term"], "MAX_ORDERS": str(max_orders),
               "LEADER_HEARTBEAT_TIMEOUT": settings["heartbeat"], "EXTRA_PROPS": ";".join(props),
               "ENGINE": str(ENGINES / engine / "matching-engine")}
        if engine != "diagnostics":
            env["NO_DIGEST"] = "1"
        return env

    def client(self, tool, *args):
        command = [TOOLS / tool, *args]
        return subprocess.Popen([str(c) for c in command], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                env={**os.environ, **self.tool_env}, stdin=subprocess.DEVNULL)

    def roles(self):
        total = 0
        for node in range(3):
            log = ROOT / ".run" / "cluster" / f"engine{node}.log"
            if log.exists():
                total += sum(line.startswith("role") for line in log.read_text(errors="replace").splitlines())
        return total

    def digests(self):
        found = []
        for node in range(3):
            path = ROOT / ".run" / "cluster" / f"digest{node}"
            found.append(path.read_text().strip() if path.exists() else "")
        return found

    def threads(self, leader):
        return None

    def disk(self):
        return {}


class Ec2(Target):
    name = "ec2"
    script = str(ROOT / "tools/bench/ec2/bench.py")
    can_vary = tuple(VARIATIONS)
    rates = [50_000, 100_000, 200_000, 300_000, 400_000, 500_000, 600_000, 800_000]
    reference_rate = 100_000
    seconds = 18
    warmup_seconds = 3
    soak_seconds = 100
    soak_rate = 200_000
    max_orders = 5_000_000
    tape_orders = 12_000_000
    soak_orders = 20_000_000
    heartbeats = ["250ms", "500ms", "1s"]
    startup_seconds = 45
    tape_dir = Path("/data")

    def preflight(self):
        if not (Path(self.script).parent / "hosts.json").exists():
            sys.exit("no hosts.json, so there are no hosts to run on")

    def settings_env(self, settings, sync, engine, max_orders):
        env = {"SYNC": str(sync), "THREADING": settings["threading"], "LOG_TERM_LENGTH": settings["term"],
               "IDLE": BUSY_SPIN if settings["idle"] == "busy-spin" else BACKOFF,
               "PINNED": "1" if settings["pinned"] else "0", "MAX_ORDERS": str(max_orders),
               "STORE": "/var/bench" if settings["store"] == "ebs" else "/data",
               "LEADER_HEARTBEAT_TIMEOUT": settings["heartbeat"],
               "ENGINE": f"engines/{engine}/matching-engine"}
        if engine != "diagnostics":
            env["NO_DIGEST"] = "1"
        return env

    def client(self, tool, *args):
        return subprocess.Popen([sys.executable, "-I", self.script, "client", tool, *map(str, args)], text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                env={**os.environ, **self.env}, cwd=ROOT)

    def on_node(self, node, command):
        return self.control("shell", node, command).stdout

    def roles(self):
        return sum(int(self.on_node(node, "grep -c '^role' /data/engine.log").strip() or 0) for node in range(3))

    def digests(self):
        return [self.on_node(node, "cat /data/digest").strip() for node in range(3)]

    def threads(self, leader):
        """The busiest threads on the leader, to name what limits the peak."""
        return self.on_node(leader, "top -H -b -d 4 -n 2 -w 120 | awk '/^top/{n++} n==2' | sed -n 8,17p")

    def disk(self):
        results = {}
        for name, path in (("nvme", "/data/fio"), ("ebs", "/var/bench-fio")):
            command = (f"sudo mkdir -p {path}; sudo chmod 777 {path}; fio --name=fsync --directory={path} --rw=write "
                       "--bs=64k --size=256m --fdatasync=1 --output-format=json; sudo rm -rf " + path)
            output = self.on_node(0, command)
            try:
                results[name] = json.loads(output[output.index("{"):])["jobs"][0]["sync"]["lat_ns"]["percentile"]
            except (ValueError, KeyError, IndexError):
                results[name] = {"failed": output[-200:]}
        return results


TARGETS = {"local": Local, "ec2": Ec2}


def build(target):
    """The measured engine is the deployed build. A second build proves the nodes agree."""
    if target.name != "local":
        return
    for name, features in (("deployed", "silent"), ("diagnostics", "silent,diagnostics")):
        subprocess.run(["cargo", "build", "--profile", "dist", "-p", "matching-engine", "--features", features],
                       cwd=ROOT, check=True, capture_output=True)
        (ENGINES / name).mkdir(parents=True, exist_ok=True)
        shutil.copy(ROOT / "target/dist/matching-engine", ENGINES / name / "matching-engine")
    subprocess.run(["cargo", "build", "--release", "-p", "tools"], cwd=ROOT, check=True, capture_output=True)


def tape(target, orders, symbols=1):
    path = target.tape_dir / f"cluster-{orders}-{symbols}.tape"
    if target.name == "local" and path.exists():
        return path
    process = target.client("mmx-tape", "--out", path, "--orders", orders, "--seed", SEED, "--symbol", "ITEM",
                             "--symbols", symbols)
    output, _ = process.communicate(timeout=300)
    if process.returncode:
        sys.exit(f"tape failed: {output[-300:]}")
    return path


def parse(output):
    fields = dict(re.findall(r"([\w/.]+)=([\w.]+)", output))
    found = {"achieved": float(fields["orders/s"]), "reports_per_second": float(fields["reports/s"]),
             "rejected": int(fields["rejected"]), "ingress_retries": int(fields["ingress_retries"]),
             "seconds": float(fields["seconds"])}
    if "p50" in fields:
        found["samples"] = int(fields["samples"])
        found["latency_us"] = {key: float(fields[key].rstrip("us")) for key in ("p50", "p99", "p99.9", "p99.99", "max")}
    return found


def bench_args(target, tape_path, rate, orders, warmup, leader):
    return ["--tape", tape_path, "--limit", orders, "--rate", rate, "--warmup", warmup, "--source", leader]


def run_bench(target, step, tapes):
    settings = {**SHIPPED, **step["settings"]}
    rate = step["rate"]
    seconds = target.soak_seconds if step["section"] == "soak" else target.seconds
    orders = min(tapes["size"], rate * seconds) if rate else tapes["size"]
    if step["section"] == "peak":
        orders = min(orders, 8 * target.reference_rate * target.seconds)
    target.up(settings, step["sync"], step.get("engine", "deployed"), target.max_orders)
    leader = target.leader()
    roles = target.roles()
    args = bench_args(target, tapes["path"], rate, orders, rate * target.warmup_seconds, leader)
    process = target.client("mmx-cluster-bench", *args)

    sampled = {}
    sampler = None
    if step["section"] == "peak":
        sampler = threading.Thread(target=lambda: sampled.update(threads=target.threads(leader)))
        sampler.start()
    try:
        output, _ = process.communicate(timeout=max(60, 4 * orders / max(rate, target.reference_rate) + 30))
    except subprocess.TimeoutExpired:
        process.kill()
        return {"failed": "no answer before the timeout, so reports stopped arriving"}
    finally:
        if sampler:
            sampler.join()
    if "orders/s=" not in output:
        return {"failed": output[-300:]}
    result = {**parse(output), "orders": orders, **sampled}
    result["elections"] = target.roles() - roles
    if step.get("engine") == "diagnostics":
        time.sleep(1)
        result["digests"] = target.digests()
    return result


def run_failover(target, step, tapes):
    settings = {**SHIPPED, **step["settings"]}
    rate = target.reference_rate
    orders = min(tapes["size"], rate * target.seconds)
    target.up(settings, step["sync"], "diagnostics", target.max_orders)
    leader = target.leader()
    # Reads a survivor, because the tool waits seconds before leaving a dead node, which isn't failover.
    process = target.client("mmx-cluster-load", "--tape", tapes["path"], "--limit", orders, "--rate", rate,
                            "--source", (leader + 1) % 3)
    time.sleep(target.seconds / 3)
    target.kill(leader)
    try:
        output, _ = process.communicate(timeout=target.seconds * 8 + 60)
    except subprocess.TimeoutExpired:
        process.kill()
        return {"failed": "the load client never finished"}
    found = re.search(r"failovers_ms=\[([\d., ]*)\]", output)
    counts = dict(re.findall(r"(duplicates|gaps|violations)=(\d+)", output))
    if not found or len(counts) < 3:
        return {"failed": output[-300:]}
    time.sleep(1)
    digests = [d for node, d in enumerate(target.digests()) if node != leader]
    return {"failover_ms": [float(v) for v in found.group(1).replace(",", " ").split()],
            **{key: int(value) for key, value in counts.items()}, "digests": digests}


def step_id(step):
    return json.dumps({k: step.get(k) for k in ("section", "sync", "rate", "settings", "engine", "attempt")}, sort_keys=True)


def shuffled(steps, shuffle):
    """Runs every configuration once per round, so drift in the hosts lands on all of them."""
    rounds = {}
    for step in steps:
        rounds.setdefault(step["attempt"], []).append(step)
    ordered = []
    for attempt in sorted(rounds):
        shuffle.shuffle(rounds[attempt])
        ordered += rounds[attempt]
    return ordered


def first_steps(target, quick):
    """Answers which sync level to ship, and what each can carry."""
    repeats = 1 if quick else REPEATS
    rates = target.rates[:3] if quick else target.rates
    steps = []
    for sync in (0, 1):
        steps.append({"section": "integrity", "sync": sync, "rate": target.reference_rate, "settings": {},
                      "engine": "diagnostics", "attempt": 0})
        for attempt in range(repeats):
            steps += [{"section": "ladder", "sync": sync, "rate": rate, "settings": {}, "attempt": attempt} for rate in rates]
            steps.append({"section": "peak", "sync": sync, "rate": 0, "settings": {}, "attempt": attempt})
    return steps


def later_steps(target, quick, sustainable):
    """Chooses the rest once the ladder has shown what each sync level can sustain."""
    repeats = 1 if quick else REPEATS
    steps = []
    for sync in (0, 1):
        high = sustainable.get(sync) or target.reference_rate
        soak_rate = min(target.soak_rate, high)
        steps += [{"section": "soak", "sync": sync, "rate": soak_rate, "settings": {}, "attempt": attempt}
                  for attempt in range(repeats)]
    for name in target.can_vary:
        values = VARIATIONS[name][1][:1] if quick else VARIATIONS[name][1]
        for value in values:
            for sync in (0, 1):
                rates = {target.reference_rate, int((sustainable.get(sync) or target.reference_rate) * 0.8)}
                for rate in sorted(rates):
                    steps += [{"section": "setting", "sync": sync, "rate": rate, "settings": {name: value},
                               "attempt": attempt} for attempt in range(repeats)]
    heartbeats = target.heartbeats[:1] if quick else target.heartbeats
    steps += [{"section": "failover", "sync": 0, "rate": target.reference_rate, "settings": {"heartbeat": heartbeat},
               "attempt": attempt} for heartbeat in heartbeats
              for attempt in range(1 if quick else FAILOVER_REPEATS)]
    return steps


def step_seconds(target, step):
    if step["section"] == "failover":
        return target.startup_seconds + target.seconds * 2.5
    if step["section"] == "soak":
        return target.startup_seconds + target.soak_seconds
    if step["section"] == "peak":
        return target.startup_seconds + 25
    return target.startup_seconds + target.seconds


def good(row):
    return "failed" not in row and not problems(row)


def problems(row):
    found = []
    if row.get("failed"):
        found.append(row["failed"][:120])
    if row.get("rejected"):
        found.append(f"{row['rejected']:,} rejected")
    if row.get("elections"):
        found.append(f"{row['elections']} role changes during the run")
    if len(set(row.get("digests", []))) > 1:
        found.append("nodes' digests differ")
    if not row.get("digests", True) or "" in row.get("digests", []):
        found.append("a node wrote no digest")
    for key in ("duplicates", "gaps", "violations"):
        if row.get(key):
            found.append(f"{row[key]} {key}")
    return found


def keeps_up(row, rate):
    latency = row.get("latency_us")
    return good(row) and latency and row["achieved"] >= rate * SUSTAINABLE_ACHIEVED and latency["p99"] <= SUSTAINABLE_P99_US


def sustainable_rates(rows):
    """Highest offered rate that every repeat kept up with, and every lower rate too."""
    found = {}
    for sync in (0, 1):
        ladder = [row for row in rows if row["section"] == "ladder" and row["sync"] == sync]
        best = 0
        for rate in sorted({row["rate"] for row in ladder}):
            runs = [row for row in ladder if row["rate"] == rate]
            if not all(keeps_up(row, rate) for row in runs):
                break
            best = rate
        found[sync] = best
    return found


def load(directory):
    path = directory / "runs.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def cost(seconds):
    return seconds / 3600 * HOSTS * (INSTANCE_DOLLARS_PER_HOUR + ADDRESS_DOLLARS_PER_HOUR)


def show_plan(args):
    target = TARGETS[args.target]()
    assumed = {0: target.rates[-3], 1: target.rates[-5]}
    steps = first_steps(target, args.quick) + later_steps(target, args.quick, assumed)
    sections = {}
    for step in steps:
        entry = sections.setdefault(step["section"], [0, 0])
        entry[0] += 1
        entry[1] += step_seconds(target, step)
    total = sum(seconds for _, seconds in sections.values())
    for name, (count, seconds) in sections.items():
        print(f"{name:10} {count:4} runs {seconds / 60:6.0f} min")
    print(f"{'total':10} {len(steps):4} runs {total / 60:6.0f} min", end="")
    print(f", ${cost(total):.2f} on {HOSTS} hosts" if target.name == "ec2" else "")


def execute(target, directory, steps, done, stop_at):
    """Runs the steps that haven't run yet, and returns False if it stopped early."""
    rows = directory / "runs.jsonl"
    failures = 0
    tapes = {"size": target.tape_orders, "path": tape(target, target.tape_orders)}
    soak_tapes = {"size": target.soak_orders, "path": tape(target, target.soak_orders)}
    many_symbols = {}
    remaining = [step for step in steps if step_id(step) not in done]
    for index, step in enumerate(remaining):
        if stop_at and time.time() > stop_at:
            print(f"out of time with {len(remaining) - index} runs left", flush=True)
            return False
        chosen = soak_tapes if step["section"] == "soak" else tapes
        symbols = step["settings"].get("symbols")
        if symbols:
            if symbols not in many_symbols:
                many_symbols[symbols] = {"size": target.tape_orders, "path": tape(target, target.tape_orders, symbols)}
            chosen = many_symbols[symbols]
        try:
            result = (run_failover if step["section"] == "failover" else run_bench)(target, step, chosen)
        except (RuntimeError, subprocess.TimeoutExpired) as err:
            result = {"failed": str(err)}
        row = {**result, **step}
        with rows.open("a") as out:
            out.write(json.dumps(row) + "\n")
        done.add(step_id(step))
        failures = failures + 1 if "failed" in result else 0
        latency = result.get("latency_us") or result.get("failover_ms") or result.get("failed", "")
        print(f"{index + 1}/{len(remaining)} {step['section']} sync={step['sync']} rate={step['rate']} "
              f"{step['settings'] or ''} {latency}", flush=True)
        if failures >= STOP_AFTER_FAILURES:
            print(f"stopping: {failures} failures in a row, so something is wrong", flush=True)
            return False
    return True


def run(args):
    target = TARGETS[args.target]()
    target.preflight()
    build(target)
    directory = RESULTS / args.resume if args.resume else RESULTS / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    directory.mkdir(parents=True, exist_ok=True)
    if not args.resume:
        (directory / "run.json").write_text(json.dumps({
            "target": target.name, "quick": args.quick, "seed": args.seed,
            "commit": shell("git", "rev-parse", "--short", "HEAD"),
            "dirty": bool(shell("git", "status", "--porcelain", "--", "services", "crates", "tools", "Cargo.lock")),
            "date": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "disk": target.disk()}, indent=2))
    print(f"results in {directory}", flush=True)
    stop_at = time.time() + args.stop_after * 60 if args.stop_after else None
    shuffle = random.Random(args.seed)
    done = {step_id(row) for row in load(directory)}
    try:
        finished = execute(target, directory, shuffled(first_steps(target, args.quick), shuffle), done, stop_at)
        if finished:
            sustainable = sustainable_rates(load(directory))
            print(f"sustainable by sync level: {sustainable}", flush=True)
            execute(target, directory, shuffled(later_steps(target, args.quick, sustainable), shuffle), done, stop_at)
    finally:
        target.down()
    write_report(directory, None)


def median_latency(rows, key):
    return statistics.median(row["latency_us"][key] for row in rows)


def ladder_section(rows, sync):
    runs = [row for row in rows if row["section"] == "ladder" and row["sync"] == sync and row.get("latency_us")]
    adapted = [{"mode": "load", "rate": row["rate"],
                "latency_ns": {"p50": row["latency_us"]["p50"] * 1000, "p99": row["latency_us"]["p99"] * 1000,
                               "p99_9": row["latency_us"]["p99.9"] * 1000}} for row in runs]
    chart, _ = load_chart(adapted, low=4)
    return chart


def effect_chart(items):
    """One row per setting: p99 at each sync level, on a shared log axis, against what ships."""
    width, row, left, right = 820, 34, 230, 90
    height = row * len(items) + 26
    values = [value for item in items for value in item["values"].values() if value]
    if not values:
        return ""
    low = math.floor(math.log10(min(values)))
    high = math.ceil(math.log10(max(values)))
    high = max(high, low + 1)
    x = lambda value: left + (width - left - right) * (math.log10(value) - low) / (high - low)
    colors = {0: "var(--series-1)", 1: "var(--series-2)"}
    parts = [f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="p99 latency by setting, log scale">']
    for power in range(low, high + 1):
        parts.append(f'<line class="grid" x1="{x(10 ** power):.1f}" x2="{x(10 ** power):.1f}" y1="0" y2="{height - 22}"/>')
        parts.append(f'<text x="{x(10 ** power):.1f}" y="{height - 6}" text-anchor="middle">{latency_text(10 ** power * 1000)}</text>')
    for index, item in enumerate(items):
        y = row * index + row / 2
        parts.append(f'<text x="{left - 12}" y="{y + 4}" text-anchor="end">{html.escape(item["label"])}</text>')
        for sync, value in sorted(item["values"].items()):
            if not value:
                continue
            tip = f"{item['label']}, sync {sync}: p99 {latency_text(value * 1000)}"
            parts.append(f'<circle class="dot" fill="{colors[sync]}" cx="{x(value):.1f}" cy="{y}" r="6"/>')
            parts.append(f'<circle class="hit" cx="{x(value):.1f}" cy="{y}" r="14" data-tip="{html.escape(tip, quote=True)}"/>')
    parts.append("</svg>")
    return "".join(parts)


def rows_table(head, lines):
    header = "".join(f"<th>{html.escape(cell)}</th>" for cell in head)
    body = "".join("<tr>" + "".join(f"<td>{html.escape(str(cell))}</td>" for cell in line) + "</tr>" for line in lines)
    return f"<table><tr>{header}</tr>{body}</table>"


def us(value):
    return latency_text(value * 1000)


def achieved_table(rows):
    lines = []
    for rate in sorted({row["rate"] for row in rows if row["section"] == "ladder"}):
        line = [f"{rate:,}/s"]
        for sync in (0, 1):
            runs = [row for row in rows if row["section"] == "ladder" and row["sync"] == sync and row["rate"] == rate]
            ok = [row for row in runs if good(row)]
            line.append(f"{statistics.median(row['achieved'] for row in ok):,.0f}/s" if ok else "failed")
            line.append(f"{sum(keeps_up(row, rate) for row in runs)}/{len(runs)}")
        lines.append(line)
    return rows_table(["Offered", "Sync 0 achieved", "Kept up", "Sync 1 achieved", "Kept up"], lines)


def settings_section(rows, sustainable):
    parts = []
    base = [row for row in rows if row["section"] == "ladder"]
    for rate_name, chooser in (("the reference rate", min), ("80% of sustainable", max)):
        setting_rows = [row for row in rows if row["section"] == "setting" and good(row)]
        if not setting_rows:
            continue
        items = []
        for name, (title, values) in VARIATIONS.items():
            for value in values:
                entry = {"label": f"{title}: {value}", "values": {}}
                for sync in (0, 1):
                    runs = [row for row in setting_rows if row["sync"] == sync and row["settings"].get(name) == value]
                    if runs:
                        rate = chooser(row["rate"] for row in runs)
                        chosen = [row for row in runs if row["rate"] == rate]
                        entry["values"][sync] = median_latency(chosen, "p99")
                        entry["rate"] = rate
                if entry["values"]:
                    items.append(entry)
        if items:
            parts.append(f'<div class="sub">p99 at {rate_name}, sync 0 and sync 1. Log scale.</div>{effect_chart(items)}')
    return "".join(parts)


def thread_section(rows):
    parts = []
    for sync in (0, 1):
        sample = next((row["threads"] for row in rows if row["section"] == "peak" and row["sync"] == sync and row.get("threads")), None)
        if sample:
            parts.append(f"<h3>Sync {sync}</h3><pre>{html.escape(sample)}</pre>")
    return "".join(parts)


def write_report(directory, findings):
    meta = json.loads((directory / "run.json").read_text())
    rows = load(directory)
    sustainable = sustainable_rates(rows)
    peak = {sync: [row for row in rows if row["section"] == "peak" and row["sync"] == sync and good(row)] for sync in (0, 1)}
    stats = "".join(f'<div class="stat"><b>{value}</b><span>{label}</span></div>' for value, label in [
        (f"{sustainable[0]:,}/s", "sustained at sync 0"), (f"{sustainable[1]:,}/s", "sustained at sync 1"),
        *[(f"{statistics.median(row['achieved'] for row in peak[sync]):,.0f}/s", f"unpaced peak at sync {sync}")
          for sync in (0, 1) if peak[sync]]])

    flagged = [(row["section"], row["sync"], row["rate"], "; ".join(problems(row))) for row in rows if problems(row)]
    integrity = [row for row in rows if row["section"] == "integrity"]
    soak = [[f"sync {row['sync']}", f"{row['rate']:,}/s", f"{row['seconds']:.0f}s", us(row["latency_us"]["p50"]),
             us(row["latency_us"]["p99"]), us(row["latency_us"]["p99.9"]), us(row["latency_us"]["max"])]
            for row in rows if row["section"] == "soak" and row.get("latency_us")]
    failover = [[row["settings"].get("heartbeat"), row["attempt"], ", ".join(f"{v:,.0f}ms" for v in row["failover_ms"]),
                 row["duplicates"], row["gaps"], row["violations"], "match" if len(set(row["digests"])) == 1 else "DIFFER"]
                for row in rows if row["section"] == "failover" and "failover_ms" in row]
    disk = []
    for name, percentiles in meta.get("disk", {}).items():
        if "failed" not in percentiles:
            disk.append([name] + [us(percentiles[key] / 1000) for key in ("50.000000", "99.000000", "99.900000")])

    sections = [
        f"<h2>Offered load, sync 0</h2>{ladder_section(rows, 0)}",
        f"<h2>Offered load, sync 1</h2>{ladder_section(rows, 1)}",
        f"<h2>Did the cluster keep up?</h2>{achieved_table(rows)}",
    ]
    if soak:
        sections.append("<h2>Soak</h2><div class=\"sub\">Long enough for the kernel to start writing dirty pages, which short runs at sync 0 never reach.</div>"
                        + rows_table(["Sync", "Rate", "Length", "p50", "p99", "p99.9", "Max"], soak))
    chart = settings_section(rows, sustainable)
    if chart:
        sections.append(f"<h2>Settings</h2>{chart}")
    if failover:
        sections.append("<h2>Leader failure</h2>" + rows_table(
            ["Heartbeat timeout", "Run", "Failover", "Duplicates", "Gaps", "Violations", "Survivors' digests"], failover))
    threads = thread_section(rows)
    if threads:
        sections.append(f"<h2>Busiest threads at peak</h2>{threads}")
    if disk:
        sections.append("<h2>Raw disk sync</h2><div class=\"sub\">fio, 64K writes, each followed by a sync.</div>"
                        + rows_table(["Disk", "p50", "p99", "p99.9"], disk))
    if integrity:
        lines = [[f"sync {row['sync']}", "match" if good(row) else "; ".join(problems(row))] for row in integrity]
        sections.append("<h2>Nodes agree</h2>" + rows_table(["Run", "Report digest on all three nodes"], lines))
    if flagged:
        sections.append("<h2>Flagged runs</h2>" + rows_table(["Section", "Sync", "Rate", "Problem"], flagged))

    notes = f"<h2>Findings</h2>{findings.read_text()}" if findings else ""
    page = f"""<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><title>Cluster profile</title>
<meta name="viewport" content="width=device-width, initial-scale=1"><style>{CSS} pre {{ font-size: 12px; overflow-x: auto; }}</style></head>
<body><div class="viz-root"><main><h1>Cluster profile</h1>
<div class="sub">Three nodes and a paced client, one order per message. Target {html.escape(meta['target'])}, commit {meta['commit']}{' (uncommitted changes)' if meta['dirty'] else ''}, {meta['date']}.{' Quick pass, so one repeat of each.' if meta['quick'] else ''}</div>
<div class="stats">{stats}</div>{notes}
<h2>Method</h2><p>The cluster starts empty for every run. Latency runs from each order's scheduled send to its first report on the leader, leaving out the first seconds while the JVM compiles. Sustained means every repeat achieved {SUSTAINABLE_ACHIEVED:.0%} of the offered rate with p99 under {SUSTAINABLE_P99_US / 1000:g}ms, at that rate and all lower ones. {len(rows)} runs, {len(flagged)} flagged.</p>
{''.join(sections)}
</main></div><div id="tip"></div><script>{SCRIPT}</script></body></html>"""
    (directory / "report.html").write_text(page)
    print(directory / "report.html")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("plan", "run"):
        command = commands.add_parser(name)
        command.add_argument("--target", choices=TARGETS, default="local")
        command.add_argument("--quick", action="store_true", help="one short pass, to test the harness")
        if name == "run":
            command.add_argument("--seed", type=int, default=SEED, help="shuffles the run order")
            command.add_argument("--stop-after", type=float, help="minutes, then stop and report")
            command.add_argument("--resume", help="directory under .run/profile-cluster")
    report = commands.add_parser("report")
    report.add_argument("run", help="directory under .run/profile-cluster")
    report.add_argument("--findings", type=Path, help="HTML to place at the top")
    args = parser.parse_args()

    if args.command == "plan":
        show_plan(args)
    elif args.command == "run":
        run(args)
    else:
        directory = Path(args.run)
        write_report(directory if directory.is_absolute() else RESULTS / directory, args.findings)


if __name__ == "__main__":
    main()
