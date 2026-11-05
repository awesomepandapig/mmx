#!/usr/bin/env python3
"""Sweep the matching engine's settings and workloads, and report them with charts.

    tools/scripts/profile.py run                     run every sweep
    tools/scripts/profile.py run --quick             one short pass, to test the harness
    tools/scripts/profile.py run --sweeps symbols,capacity
    tools/scripts/profile.py report RUN [--findings FILE]

This measures the engine alone: raw SBE bytes in over IPC, reports out, one message per order, as a
gateway sends them. Nothing else in the system is involved, and nothing is batched.

Rounds run every configuration once, in a shuffled order, so that drift in the machine lands on all
of them rather than on whichever ran last. A run counts only if the engine survives it. Unexpected
rejects mark the configuration invalid, because rejected orders skip matching and flatter the score.
"""
import argparse
import html
import json
import os
import platform
import random
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RESULTS = ROOT / ".run" / "profile"
TAPES = ROOT / ".run" / "tapes" / "profile"
TOOLS = ROOT / "target" / "release"
ENGINE = ROOT / "target" / "dist" / "matching-engine"
IS_MAC = platform.system() == "Darwin"
AERON_DIR = "/Volumes/DevShm/aeron-bench" if IS_MAC else "/dev/shm/aeron-bench"
AERON_BIN = Path(os.environ.get("AERON_BIN", Path.home() / "Projects/aeron/cppbuild/Release/binaries"))
# Linux only: the driver when there's no aeronmd, core lists for each process, and where perf writes.
AERON_JAR = os.environ.get("AERON_JAR")
CPUS = {name: os.environ.get(f"{name}_CPUS") for name in ("ENGINE", "BENCH", "DRIVER")}
PERF_DIR = os.environ.get("PERF_DIR")

BASE = {"orders": 5_000_000, "seed": 1, "accounts": 1000, "spread": 5, "market_pct": 10, "symbols": 1,
        "cancel_pct": 0, "unknown_cancel_pct": 0, "duplicate_pct": 0, "max_orders": 1_000_000}

LATENCY_RATE = 500_000
LATENCY_MESSAGES = 1_500_000
RUN_TIMEOUT = 180
THROUGHPUT_ROUNDS = 5
DIGEST_ROUNDS = 2
LATENCY_ROUNDS = 3
NOISY_SPREAD = 0.05

# Each sweep changes one thing from BASE.
SWEEPS = {
    "tape_size": ("Tape size", "orders", [1_000_000, 5_000_000, 20_000_000], {"max_orders": 5_000_000}),
    "book_depth": ("Book depth: price levels either side of mid", "spread", [5, 50, 500, 5000, 50000],
                   {"max_orders": 5_000_000}),
    "symbols": ("Symbols sharing one engine", "symbols", [1, 10, 100, 500, 1000, 2000, 5000, 10000], {}),
    "capacity": ("MAX_ORDERS", "max_orders", [1_000_000, 5_000_000, 10_000_000], {}),
    "market_pct": ("Market orders, percent", "market_pct", [0, 10, 50, 100], {}),
    "cancels": ("Cancels, percent of messages", "cancel_pct", [0, 10, 30, 50], {}),
    "unknown_cancels": ("Cancels of unknown orders, percent", "unknown_cancel_pct", [0, 10, 50], {}),
    "duplicates": ("Repeated order IDs, percent", "duplicate_pct", [0, 1, 10], {}),
}
LOAD_RATES = [250_000, 500_000, 1_000_000, 2_000_000, 3_000_000, 3_500_000, 4_000_000]


def run_quiet(command, **kwargs):
    return subprocess.run([str(c) for c in command], check=True, capture_output=True, text=True,
                          stdin=subprocess.DEVNULL, **kwargs).stdout.strip()


def shell(*command):
    try:
        return subprocess.run(command, capture_output=True, text=True, cwd=ROOT,
                              stdin=subprocess.DEVNULL).stdout.strip()
    except FileNotFoundError:
        return "unknown"


def preflight():
    busy = [name for name in ("aeronmd", "matching-engine", "sbx-bench")
            if shell("pgrep", "-x", name)]
    if shell("pgrep", "-f", "[C]lusteredMediaDriver"):
        busy.append("a cluster node")
    if busy:
        sys.exit(f"{', '.join(busy)} already running, which would skew every result")


def build():
    if os.environ.get("PREBUILT"):
        return
    run_quiet(["cargo", "build", "--profile", "dist", "-p", "matching-engine", "--features", "silent"], cwd=ROOT)
    run_quiet(["cargo", "build", "--release", "-p", "tools"], cwd=ROOT)


def environment():
    cpu = shell("sysctl", "-n", "machdep.cpu.brand_string") if IS_MAC else shell("uname", "-m")
    return {
        "commit": shell("git", "rev-parse", "--short", "HEAD"),
        "dirty": bool(shell("git", "status", "--porcelain", "--", "services", "crates", "tools", "Cargo.lock")),
        "rustc": shell("rustc", "--version"),
        "cpu": cpu,
        "cores": os.cpu_count(),
        "os": platform.platform(),
        "date": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }


def tape_key(config):
    return "-".join(str(config[key]) for key in BASE if key != "max_orders")


def tape_for(config):
    path = TAPES / f"{tape_key(config)}.tape"
    if path.exists():
        return path
    TAPES.mkdir(parents=True, exist_ok=True)
    print(f"  generating {path.name}", flush=True)
    run_quiet([TOOLS / "sbx-tape", "--out", path, "--orders", config["orders"], "--seed", config["seed"],
               "--accounts", config["accounts"], "--symbol", "ITEM", "--spread", config["spread"],
               "--market-pct", config["market_pct"], "--symbols", config["symbols"],
               "--cancel-pct", config["cancel_pct"], "--unknown-cancel-pct", config["unknown_cancel_pct"],
               "--duplicate-pct", config["duplicate_pct"]])
    return path


def memory_kb(pid):
    output = shell("ps", "-o", "rss=", "-p", str(pid))
    return int(output) if output.isdigit() else 0


def pinned(name, command):
    """Holds a process to its cores. macOS can't, which is one reason its tails can't be trusted."""
    return ["taskset", "-c", CPUS[name], *command] if CPUS[name] else command


def driver_command():
    if AERON_JAR:
        return ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED", f"-Daeron.dir={AERON_DIR}",
                "-Daeron.threading.mode=SHARED", "-Daeron.dir.delete.on.start=true", "-cp", AERON_JAR,
                "io.aeron.driver.MediaDriver"]
    return [AERON_BIN / "aeronmd", f"-Daeron.dir={AERON_DIR}", "-Daeron.threading.mode=SHARED",
            "-Daeron.dir.delete.on.start=true"]


def wait_until_booted(pid):
    """The engine connects before it finishes touching its memory, so waits for that to settle."""
    last, steady = -1, 0
    deadline = time.time() + 60
    while steady < 3 and time.time() < deadline:
        time.sleep(0.2)
        current = memory_kb(pid)
        steady = steady + 1 if current == last and current > 0 else 0
        last = current


def measure(config, mode, tape, rate=LATENCY_RATE, limit=None):
    env = {**os.environ, "AERON_DIR": AERON_DIR}
    driver = subprocess.Popen(
        pinned("DRIVER", driver_command()),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    time.sleep(1)
    engine = subprocess.Popen(
        pinned("ENGINE", [ENGINE]), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
        env={**env, "SUB_CHANNEL": "aeron:ipc", "SUB_STREAM_ID": "1001", "PUB_CHANNEL": "aeron:ipc",
             "PUB_STREAM_ID": "1002", "MAX_ORDERS": str(config["max_orders"])})
    try:
        wait_until_booted(engine.pid)
        boot_kb = memory_kb(engine.pid)
        command = [TOOLS / "sbx-bench", "--tape", tape, "--mode", mode, "--rate", rate]
        if limit:
            command += ["--limit", limit]
        load = os.getloadavg()[0]
        perf = None
        if PERF_DIR and mode == "digest":
            perf = subprocess.Popen(["perf", "record", "-q", "-e", "cpu-clock", "-F", "999", "-g", "-p", str(engine.pid),
                                     "-o", f"{PERF_DIR}/{tape.stem}-{config['max_orders']}.data"],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            output = run_quiet(pinned("BENCH", command), env=env, timeout=RUN_TIMEOUT)
        except subprocess.TimeoutExpired:
            return {"failed": f"no answer after {RUN_TIMEOUT}s"}
        except subprocess.CalledProcessError as err:
            return {"failed": (err.stderr or "sbx-bench failed").strip()[-300:]}
        if engine.poll() is not None:
            return {"failed": f"the engine exited with status {engine.returncode}"}
        result = json.loads(output.splitlines()[-1])
        result.update(load=round(load, 2), boot_kb=boot_kb, end_kb=memory_kb(engine.pid))
        return result
    finally:
        if perf:
            perf.terminate()
            perf.wait()
        engine.kill()
        driver.terminate()
        engine.wait()
        driver.wait()


def configurations(names, quick):
    """Returns configurations keyed so that a value shared by two sweeps runs once."""
    configs = {}
    for name in names:
        _, field, values, overrides = SWEEPS[name]
        for value in values:
            config = {**BASE, **overrides, field: value}
            if quick:
                config["orders"] = min(config["orders"], 500_000)
            configs[f"{tape_key(config)}/{config['max_orders']}"] = config
    return configs


def record(path, row):
    with path.open("a") as out:
        out.write(json.dumps(row) + "\n")


def run_sweeps(args):
    preflight()
    build()
    names = args.sweeps.split(",") if args.sweeps else list(SWEEPS)
    unknown = [name for name in names if name not in SWEEPS]
    if unknown:
        sys.exit(f"unknown sweeps: {', '.join(unknown)}")

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    directory = RESULTS / stamp
    directory.mkdir(parents=True)
    rows = directory / "runs.jsonl"
    configs = configurations(names, args.quick)
    (directory / "run.json").write_text(json.dumps(
        {"environment": environment(), "sweeps": names, "base": BASE, "configs": configs,
         "latency_rate": LATENCY_RATE, "quick": args.quick}, indent=2))

    tapes = {key: tape_for(config) for key, config in configs.items()}
    shuffle = random.Random(args.seed)
    rounds = [("digest", DIGEST_ROUNDS), ("throughput", THROUGHPUT_ROUNDS), ("latency", LATENCY_ROUNDS)]
    plan = []
    for mode, count in rounds:
        for attempt in range(1 if args.quick else count):
            order = list(configs)
            shuffle.shuffle(order)
            plan += [(key, mode, attempt) for key in order]

    print(f"{len(configs)} configurations, {len(plan)} runs, results in {directory}", flush=True)
    started = time.time()
    for index, (key, mode, attempt) in enumerate(plan):
        limit = LATENCY_MESSAGES if mode == "latency" else None
        result = measure(configs[key], mode, tapes[key], limit=limit)
        record(rows, {**result, "config": key, "mode": mode, "attempt": attempt})
        if index % 20 == 0:
            print(f"{index + 1}/{len(plan)} after {time.time() - started:.0f}s", flush=True)

    base_key = f"{tape_key(BASE)}/{BASE['max_orders']}"
    base = {**BASE, "orders": min(BASE["orders"], 500_000)} if args.quick else BASE
    tape = tape_for(base)
    for rate in LOAD_RATES:
        for attempt in range(1 if args.quick else LATENCY_ROUNDS):
            limit = min(BASE["orders"], max(1_000_000, rate * 2))
            result = measure(base, "latency", tape, rate=rate, limit=500_000 if args.quick else limit)
            record(rows, {**result, "config": base_key, "mode": "load", "rate": rate, "attempt": attempt})
            print(f"load {rate}: {result.get('latency_ns', result)}", flush=True)

    print(f"done in {time.time() - started:.0f}s: {directory}", flush=True)
    write_report(directory, None)


def load_rows(directory):
    return [json.loads(line) for line in (directory / "runs.jsonl").read_text().splitlines()]


def summarize(directory):
    """Returns per-configuration statistics, marking the ones that can't be trusted."""
    meta = json.loads((directory / "run.json").read_text())
    rows = load_rows(directory)
    summary = {}
    for key, config in meta["configs"].items():
        runs = [row for row in rows if row["config"] == key and row["mode"] != "load"]
        failed = [row for row in runs if "failed" in row]
        passed = [row for row in runs if "failed" not in row]
        speed = [row for row in passed if row["mode"] == "throughput"]
        latency = [row for row in passed if row["mode"] == "latency"]
        digests = {row["digest"] for row in passed if row["mode"] == "digest"}
        rates = [row["messages_per_second"] for row in speed]
        rejected = max((row["rejected"] for row in passed), default=0)
        reports = {row["reports"] for row in passed if row["mode"] != "latency"}
        problems = []
        if failed:
            problems.append(f"{len(failed)} failed: {failed[0]['failed'][:80]}")
        if rejected:
            problems.append(f"{rejected:,} rejected because the book filled")
        if len(digests) > 1:
            problems.append("digests differ, so the engine wasn't deterministic")
        if len(reports) > 1:
            problems.append("report counts differ between runs")
        median = statistics.median(rates) if rates else 0
        spread = (max(rates) - min(rates)) / median if median else 0
        summary[key] = {
            "config": config, "median": median, "best": max(rates, default=0), "worst": min(rates, default=0),
            "spread": spread, "runs": len(rates), "problems": problems, "digest": next(iter(digests), None),
            "reports": speed[0]["reports"] if speed else 0, "messages": speed[0]["messages"] if speed else 0,
            "orders_per_second": statistics.median(row["orders_per_second"] for row in speed) if speed else 0,
            "reports_per_second": statistics.median(row["reports_per_second"] for row in speed) if speed else 0,
            "duplicates": speed[0]["duplicates"] if speed else 0, "canceled": speed[0]["canceled"] if speed else 0,
            "cancel_rejects": speed[0]["cancel_rejects"] if speed else 0,
            "memory_mb": max((row["end_kb"] for row in passed), default=0) / 1024,
            "latency": {name: statistics.median(row["latency_ns"][name] for row in latency)
                        for name in ("p50", "p99", "p99_9", "p99_99")} if latency else None,
        }
    return meta, rows, summary


CSS = """
.viz-root { color-scheme: light; --surface: #fcfcfb; --ink: #0b0b0b; --ink-2: #52514e; --ink-3: #7a7974;
  --grid: #e8e7e3; --series-1: #2a78d6; --series-2: #eb6834; --series-3: #1baf7a; --warn: #b3261e; }
@media (prefers-color-scheme: dark) { :root:where(:not([data-theme="light"])) .viz-root { color-scheme: dark;
  --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --ink-3: #94938a; --grid: #33332f;
  --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --warn: #f2b8b5; } }
:root[data-theme="dark"] .viz-root { color-scheme: dark; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7;
  --ink-3: #94938a; --grid: #33332f; --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --warn: #f2b8b5; }
body { margin: 0; }
.viz-root { background: var(--surface); color: var(--ink); min-height: 100vh; padding: 32px 20px 64px;
  font: 15px/1.55 -apple-system, "Segoe UI", Roboto, sans-serif; }
main { max-width: 860px; margin: 0 auto; }
h1 { font-size: 26px; line-height: 1.2; margin: 0 0 6px; } h2 { font-size: 18px; margin: 44px 0 2px; }
p { margin: 8px 0; color: var(--ink-2); } li { color: var(--ink-2); margin: 4px 0; }
.sub { color: var(--ink-2); margin-bottom: 12px; font-size: 14px; }
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 12px; margin: 22px 0; }
.stat b { display: block; font-size: 26px; font-weight: 600; } .stat span { color: var(--ink-2); font-size: 13px; }
svg { display: block; width: 100%; height: auto; } svg text { fill: var(--ink-2); font-size: 12px; }
.grid { stroke: var(--grid); stroke-width: 1; } .value { fill: var(--ink) !important; font-weight: 600; }
.dot { stroke: var(--surface); stroke-width: 2; } .range { stroke-width: 2; stroke-linecap: round; }
.hit { fill: transparent; }
table { border-collapse: collapse; width: 100%; font-size: 13px; margin-top: 8px; font-variant-numeric: tabular-nums; }
th, td { text-align: right; padding: 4px 8px; border-bottom: 1px solid var(--grid); color: var(--ink-2); }
th:first-child, td:first-child { text-align: left; } th { font-weight: 600; color: var(--ink); }
details { margin-top: 8px; } summary { color: var(--ink-2); font-size: 13px; cursor: pointer; }
.bad { color: var(--warn); } .legend { display: flex; gap: 16px; font-size: 13px; color: var(--ink-2); margin: 6px 0 2px; }
.legend i { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 6px; }
#tip { position: fixed; pointer-events: none; display: none; background: var(--ink); color: var(--surface);
  padding: 4px 8px; border-radius: 4px; font-size: 12px; z-index: 5; }
"""

SCRIPT = """
const tip = document.getElementById('tip');
document.addEventListener('mousemove', event => {
  const text = event.target.getAttribute && event.target.getAttribute('data-tip');
  tip.style.display = text ? 'block' : 'none';
  if (text) { tip.textContent = text; tip.style.left = event.clientX + 12 + 'px'; tip.style.top = event.clientY + 12 + 'px'; }
});
"""


def millions(value):
    return f"{value / 1e6:.2f}M"


def latency_text(nanoseconds):
    return f"{nanoseconds:,.0f}ns" if nanoseconds < 10_000 else f"{nanoseconds / 1000:,.0f}µs"


def axis_time(nanoseconds):
    for limit, unit in ((1e6, "ms"), (1e3, "µs")):
        if nanoseconds >= limit:
            return f"{nanoseconds / (1e6 if unit == 'ms' else 1e3):g}{unit}"
    return f"{nanoseconds:g}ns"


def dot_chart(items):
    """One row per configuration: median dot, with a line from the worst run to the best."""
    width, row, left, right = 820, 34, 150, 110
    height = row * len(items) + 26
    top = max(item["best"] for item in items) * 1.08 or 1
    x = lambda value: left + (width - left - right) * value / top
    parts = [f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="Messages per second by setting">']
    step = 1_000_000 if top <= 8_000_000 else 2_000_000
    for tick in range(0, int(top) + 1, step):
        parts.append(f'<line class="grid" x1="{x(tick):.1f}" x2="{x(tick):.1f}" y1="0" y2="{height - 22}"/>')
        parts.append(f'<text x="{x(tick):.1f}" y="{height - 6}" text-anchor="middle">{tick / 1e6:.0f}M</text>')
    for index, item in enumerate(items):
        y = row * index + row / 2
        tip = (f"{item['label']}: {millions(item['median'])} messages/s, "
               f"{millions(item['worst'])} to {millions(item['best'])} over {item['runs']} runs")
        parts.append(f'<text x="{left - 12}" y="{y + 4}" text-anchor="end">{html.escape(item["label"])}</text>')
        color = "var(--warn)" if item["problems"] else "var(--series-1)"
        parts.append(f'<line class="range" stroke="{color}" x1="{x(item["worst"]):.1f}" x2="{x(item["best"]):.1f}" '
                     f'y1="{y}" y2="{y}"/>')
        parts.append(f'<circle class="dot" fill="{color}" cx="{x(item["median"]):.1f}" cy="{y}" r="6"/>')
        parts.append(f'<text class="value" x="{width - right + 8}" y="{y + 4}">{millions(item["median"])}</text>')
        parts.append(f'<rect class="hit" x="0" y="{y - row / 2}" width="{width}" height="{row}" '
                     f'data-tip="{html.escape(tip, quote=True)}"/>')
    parts.append("</svg>")
    return "".join(parts)


def table(items):
    head = ("<tr><th>Setting</th><th>Median msg/s</th><th>Worst</th><th>Best</th><th>Spread</th><th>Reports/s</th>"
            "<th>p50</th><th>p99</th><th>p99.9</th><th>Memory</th></tr>")
    lines = [head]
    for item in items:
        latency = item["latency"] or {}
        cells = [f"{millions(item[name])}" for name in ("median", "worst", "best")]
        cells.append(f"{item['spread'] * 100:.1f}%")
        cells.append(millions(item["reports_per_second"]))
        cells += [latency_text(latency[name]) if name in latency else "-" for name in ("p50", "p99", "p99_9")]
        cells.append(f"{item['memory_mb']:,.0f} MB")
        lines.append(f"<tr><td>{html.escape(item['label'])}</td>" + "".join(f"<td>{c}</td>" for c in cells) + "</tr>")
    return "<details><summary>Table</summary><table>" + "".join(lines) + "</table></details>"


def load_chart(rows, low=2):
    series = [("p50", "p50", "var(--series-1)"), ("p99", "p99", "var(--series-2)"), ("p99_9", "p99.9", "var(--series-3)")]
    rates = sorted({row["rate"] for row in rows if row["mode"] == "load" and "failed" not in row})
    if not rates:
        return "", []
    points = {}
    for rate in rates:
        runs = [row for row in rows if row["mode"] == "load" and row.get("rate") == rate and "failed" not in row]
        points[rate] = {key: statistics.median(run["latency_ns"][key] for run in runs) for key, _, _ in series}

    import math
    width, height, left, right, bottom = 820, 300, 64, 70, 30
    high = math.ceil(math.log10(max(max(p.values()) for p in points.values())))
    x = lambda index: left + (width - left - right) * index / max(len(rates) - 1, 1)
    y = lambda value: (height - bottom) * (1 - (math.log10(max(value, 10 ** low)) - low) / (high - low)) + 6
    parts = [f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="Latency by offered load, log scale">']
    for power in range(low, high + 1):
        value = 10 ** power
        parts.append(f'<line class="grid" x1="{left}" x2="{width - right}" y1="{y(value):.1f}" y2="{y(value):.1f}"/>')
        parts.append(f'<text x="{left - 8}" y="{y(value) + 4:.1f}" text-anchor="end">{axis_time(value)}</text>')
    for index, rate in enumerate(rates):
        parts.append(f'<text x="{x(index):.1f}" y="{height - 6}" text-anchor="middle">{rate / 1e6:g}M</text>')
    end_labels = []
    for key, label, color in reversed(series):
        path = " ".join(f"{x(i):.1f},{y(points[rate][key]):.1f}" for i, rate in enumerate(rates))
        parts.append(f'<polyline fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round" points="{path}"/>')
        for index, rate in enumerate(rates):
            tip = f"{rate / 1e6:g}M orders/s: {label} {latency_text(points[rate][key])}"
            parts.append(f'<circle class="dot" fill="{color}" cx="{x(index):.1f}" cy="{y(points[rate][key]):.1f}" r="5"/>')
            parts.append(f'<circle class="hit" cx="{x(index):.1f}" cy="{y(points[rate][key]):.1f}" r="14" '
                         f'data-tip="{html.escape(tip, quote=True)}"/>')
        label_y = y(points[rates[-1]][key]) + 4
        while any(abs(label_y - used) < 14 for used in end_labels):
            label_y += 14
        end_labels.append(label_y)
        parts.append(f'<text x="{x(len(rates) - 1) + 10:.1f}" y="{label_y:.1f}">{label}</text>')
    parts.append("</svg>")
    legend = "".join(f'<span><i style="background:{color}"></i>{label}</span>' for _, label, color in series)
    rows_html = "".join(
        f"<tr><td>{rate / 1e6:g}M/s</td>" + "".join(f"<td>{latency_text(points[rate][k])}</td>" for k, _, _ in series) + "</tr>"
        for rate in rates)
    head = "<tr><th>Offered load</th>" + "".join(f"<th>{label}</th>" for _, label, _ in series) + "</tr>"
    return (f'<div class="legend">{legend}</div>' + "".join(parts) +
            f"<details><summary>Table</summary><table>{head}{rows_html}</table></details>"), points


def setting_label(field, value):
    if field in ("orders", "max_orders"):
        return f"{value / 1e6:g}M"
    if field.endswith("_pct"):
        return f"{value}%"
    return f"{value:,}"


def write_report(directory, findings):
    meta, rows, summary = summarize(directory)
    keyed = {key: value for key, value in summary.items()}
    sections = []
    for name in meta["sweeps"]:
        title, field, values, overrides = SWEEPS[name]
        items = []
        for value in values:
            config = {**BASE, **overrides, field: value}
            if meta.get("quick"):
                config["orders"] = min(config["orders"], 500_000)
            item = dict(keyed[f"{tape_key(config)}/{config['max_orders']}"])
            item["label"] = setting_label(field, value)
            items.append(item)
        worst, best = min(i["median"] for i in items), max(i["median"] for i in items)
        change = (best - worst) / best * 100 if best else 0
        problems = "".join(f'<p class="bad">{html.escape(i["label"])}: {html.escape(p)}</p>'
                           for i in items for p in i["problems"])
        sections.append(f"<h2>{html.escape(title)}</h2>"
                        f'<div class="sub">Messages per second: dot is the median, line is worst to best. '
                        f"This setting moves throughput by {change:.0f}%.</div>"
                        f"{dot_chart(items)}{problems}{table(items)}")

    chart, _ = load_chart(rows)
    trusted = [item for item in summary.values() if not item["problems"] and item["median"]]
    base_key = f"{tape_key(BASE)}/{BASE['max_orders']}"
    base = summary.get(base_key) or (max(trusted, key=lambda i: i["median"]) if trusted else None)
    env = meta["environment"]
    stats = ""
    if base:
        latency = base["latency"] or {}
        stats = "".join(f'<div class="stat"><b>{value}</b><span>{label}</span></div>' for value, label in [
            (millions(base["median"]), "orders/s, default workload"),
            (millions(base["reports_per_second"]), "reports/s"),
            (latency_text(latency["p50"]) if latency else "-", f"p50 at {LATENCY_RATE // 1000}K orders/s"),
            (latency_text(latency["p99_9"]) if latency else "-", f"p99.9 at {LATENCY_RATE // 1000}K orders/s"),
        ])
    notes = ""
    if findings:
        notes = "<h2>Findings</h2>" + findings.read_text()
    invalid = sum(1 for item in summary.values() if item["problems"])
    page = f"""<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><title>Matching engine profile</title>
<meta name="viewport" content="width=device-width, initial-scale=1"><style>{CSS}</style></head>
<body><div class="viz-root"><main><h1>Matching engine profile</h1>
<div class="sub">Engine alone over IPC, one message per order. {html.escape(env['cpu'])}, commit {env['commit']}{' (uncommitted changes)' if env['dirty'] else ''}, {env['date']}.</div>
<div class="stats">{stats}</div>{notes}
<h2>Method</h2><p>Each configuration ran {THROUGHPUT_ROUNDS} throughput, {DIGEST_ROUNDS} digest and {LATENCY_ROUNDS} latency runs on a freshly started engine, in shuffled rounds. Each sweep changes one setting from the default: {BASE['orders'] / 1e6:g}M messages, {BASE['spread']} price levels either side of mid, {BASE['market_pct']}% market orders, one symbol, MAX_ORDERS {BASE['max_orders'] / 1e6:g}M. {len(summary)} configurations, {invalid} flagged. Sender, receiver and engine aren't pinned to cores on macOS.</p>
{''.join(sections)}
<h2>Latency by offered load</h2><div class="sub">Scheduled send to first report, so a stall can't hide the queue behind it. Log scale.</div>{chart}
</main></div><div id="tip"></div><script>{SCRIPT}</script></body></html>"""
    (directory / "report.html").write_text(page)
    print(directory / "report.html")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    run.add_argument("--sweeps", help="comma-separated sweeps, default all")
    run.add_argument("--quick", action="store_true")
    run.add_argument("--seed", type=int, default=1, help="shuffles the run order")
    report = commands.add_parser("report")
    report.add_argument("run", help="directory under .run/profile")
    report.add_argument("--findings", type=Path, help="HTML to place at the top")
    args = parser.parse_args()

    if args.command == "run":
        run_sweeps(args)
    else:
        directory = Path(args.run)
        write_report(directory if directory.is_absolute() else RESULTS / directory, args.findings)


if __name__ == "__main__":
    main()
