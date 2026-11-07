#!/usr/bin/env python3
"""Reproducible matching engine benchmark.

Plays a deterministic tape of SBE orders into a freshly started `dist` build of the engine, on
bare metal.

    tools/scripts/bench.py                         run everything, print a summary
    tools/scripts/bench.py --save-baseline NAME    also save the results as tools/bench/baselines/NAME.json
    tools/scripts/bench.py --compare NAME          fail on a regression against that baseline
    tools/scripts/bench.py --profile ipc --modes throughput --repeat 3

Tapes are generated from tools/bench/tapes.json and checked against their SHA-256. They stay out of git:
the seed and the hash are enough to rebuild identical bytes.
"""

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "tools" / "bench"
TAPES = ROOT / ".run" / "tapes"
RESULTS = ROOT / ".run" / "bench"
TOOLS = ROOT / "target" / "release"
ENGINE = ROOT / "target" / "dist" / "matching-engine"
IS_MAC = platform.system() == "Darwin"
AERON_DIR = "/Volumes/DevShm/aeron-bench" if IS_MAC else "/dev/shm/aeron-bench"
AERON_BIN = Path(os.environ.get("AERON_BIN", Path.home() / "Projects/aeron/cppbuild/Release/binaries"))

PROFILES = {
    # Shared memory on one machine: the closest thing to the engine's raw speed.
    "ipc": ("aeron:ipc", "aeron:ipc", "aeron:ipc"),
    # The default deployment: UDP for orders and MDC for execution reports.
    "udp": (
        "aeron:udp?endpoint=localhost:40123",
        "aeron:udp?control=localhost:40456|control-mode=dynamic",
        "aeron:udp?endpoint=localhost:0|control=localhost:40456",
    ),
}

LATENCY_ORDERS = "2000000"
MAX_THROUGHPUT_DROP = 0.10
MAX_LATENCY_RISE = 0.25


def run(command, **kwargs):
    result = subprocess.run([str(part) for part in command], capture_output=True, text=True,
                            cwd=ROOT, **kwargs)
    if result.returncode != 0:
        sys.exit(f"{Path(str(command[0])).name} failed:\n{result.stderr.strip()}")
    return result.stdout.strip()


def build():
    run(["cargo", "build", "--release", "-p", "tools"])
    run(["cargo", "build", "-p", "matching-engine", "--profile", "dist", "--features", "silent"])


def ensure_tape(name):
    spec = json.loads((BENCH / "tapes.json").read_text())[name]
    path = TAPES / f"{name}.tape"
    TAPES.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        print(f"generating {path}")
        digest = run([TOOLS / "mmx-tape", "--out", path, *spec["args"]])
    else:
        digest = run(["shasum", "-a", "256", path]).split()[0]
    if digest != spec["sha256"]:
        sys.exit(f"{path} has SHA-256 {digest}, expected {spec['sha256']}")
    return path


def ensure_shared_memory():
    if not IS_MAC or Path("/Volumes/DevShm").exists():
        return
    device = run(["hdiutil", "attach", "-nomount", f"ram://{2048 * 2048}"])
    run(["diskutil", "erasevolume", "HFS+", "DevShm", device])


def measure(engine, tape, mode, profile, rate, max_orders):
    orders_channel, exec_pub, exec_sub = PROFILES[profile]
    env = {**os.environ, "AERON_DIR": AERON_DIR}
    driver = subprocess.Popen(
        [AERON_BIN / "aeronmd", f"-Daeron.dir={AERON_DIR}", "-Daeron.threading.mode=SHARED",
         "-Daeron.dir.delete.on.start=true"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    matching = subprocess.Popen(
        [engine], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        env={**env, "SUB_CHANNEL": orders_channel, "SUB_STREAM_ID": "1001",
             "PUB_CHANNEL": exec_pub, "PUB_STREAM_ID": "1002", "MAX_ORDERS": max_orders})
    try:
        time.sleep(1)
        limit = ["--limit", LATENCY_ORDERS] if mode == "latency" else []
        output = run([TOOLS / "mmx-bench", "--tape", tape, "--mode", mode, "--rate", rate, *limit,
                      "--orders-channel", orders_channel, "--exec-channel", exec_sub],
                     env=env, timeout=900)
        if matching.poll() is not None:
            sys.exit(f"the engine exited with status {matching.returncode}")
        result = json.loads(output)
        # Rejects skip matching, so they would inflate the score.
        if mode != "digest" and result["rejected"]:
            sys.exit(f"{result['rejected']} orders were rejected because the book filled, which "
                     "invalidates this run. Use a shorter tape or a larger --max-orders.")
        return result
    finally:
        matching.kill()
        driver.terminate()
        matching.wait()
        driver.wait()


def environment(engine):
    def shell(*command):
        try:
            return subprocess.run(command, capture_output=True, text=True, cwd=ROOT).stdout.strip()
        except FileNotFoundError:
            return "unknown"

    cpu = shell("sysctl", "-n", "machdep.cpu.brand_string") if IS_MAC else shell("uname", "-m")
    return {
        "commit": shell("git", "rev-parse", "--short", "HEAD"),
        "dirty": bool(shell("git", "status", "--porcelain", "--", "matching-engine", "transport",
                            "sbe", "Cargo.lock")),
        "rustc": shell("rustc", "--version"),
        "cpu": cpu,
        "os": platform.platform(),
        "engine": str(engine),
        "load_before": round(os.getloadavg()[0], 2),
        "date": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }


def summarize_throughput(runs):
    rates = [item["orders_per_second"] for item in runs]
    reports = statistics.median(item["reports_per_second"] for item in runs)
    return {
        "orders_per_second": statistics.median(rates),
        "best": max(rates),
        "worst": min(rates),
        "reports_per_second": reports,
        "reports": runs[0]["reports"],
    }


def summarize_latency(runs):
    keys = runs[0]["latency_ns"].keys()
    return {key: statistics.median(item["latency_ns"][key] for item in runs) for key in keys}


def collect(args, engine, tape, name):
    results = {"tape": name, "max_orders": args.max_orders, "environment": environment(engine),
               "profiles": {}}

    for profile in args.profile:
        outcome = {}
        if "digest" in args.modes:
            print(f"{profile}: digest")
            first = measure(engine, tape, "digest", profile, args.rate, args.max_orders)
            second = measure(engine, tape, "digest", profile, args.rate, args.max_orders)
            if first["digest"] != second["digest"]:
                sys.exit(f"the engine isn't deterministic: {first['digest']} vs {second['digest']}")
            outcome["digest"] = first["digest"]
            outcome["reports"] = first["reports"]
            outcome["rejected"] = first["rejected"]

        if "throughput" in args.modes:
            runs = []
            for attempt in range(args.repeat):
                runs.append(measure(engine, tape, "throughput", profile, args.rate, args.max_orders))
                rate = runs[-1]["orders_per_second"] / 1e6
                print(f"{profile}: throughput {attempt + 1}/{args.repeat}: {rate:.3f}M orders/s")
            outcome["throughput"] = summarize_throughput(runs)

        if "latency" in args.modes:
            runs = []
            for attempt in range(args.repeat):
                runs.append(measure(engine, tape, "latency", profile, args.rate, args.max_orders))
                print(f"{profile}: latency {attempt + 1}/{args.repeat}: "
                      f"p99 {runs[-1]['latency_ns']['p99']}ns")
            outcome["latency_ns"] = summarize_latency(runs)
            outcome["latency_rate"] = int(args.rate)

        results["profiles"][profile] = outcome
    return results


def compare(results, baseline):
    for setting in ("tape", "max_orders"):
        if results.get(setting) != baseline.get(setting):
            sys.exit(f"baseline used {setting} {baseline.get(setting)}, this run used "
                     f"{results.get(setting)}, so they can't be compared")

    failures = []
    for profile, current in results["profiles"].items():
        previous = baseline["profiles"].get(profile)
        if previous is None:
            continue

        if "digest" in current and "digest" in previous and current["digest"] != previous["digest"]:
            failures.append(f"{profile}: engine behaviour changed "
                            f"({previous['digest']} -> {current['digest']})")

        if "throughput" in current and "throughput" in previous:
            new, old = (item["throughput"]["orders_per_second"] for item in (current, previous))
            change = new / old - 1
            print(f"{profile}: throughput {old / 1e6:.3f}M -> {new / 1e6:.3f}M ({change:+.1%})")
            if change < -MAX_THROUGHPUT_DROP:
                failures.append(f"{profile}: throughput fell {-change:.1%}")

        if "latency_ns" in current and "latency_ns" in previous:
            new, old = current["latency_ns"]["p99"], previous["latency_ns"]["p99"]
            change = new / old - 1
            print(f"{profile}: p99 latency {old:.0f}ns -> {new:.0f}ns ({change:+.1%})")
            if change > MAX_LATENCY_RISE:
                failures.append(f"{profile}: p99 latency rose {change:.1%}")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tape", default="5m")
    parser.add_argument("--profile", nargs="+", choices=PROFILES, default=list(PROFILES))
    parser.add_argument("--modes", nargs="+", default=["digest", "throughput", "latency"],
                        choices=["digest", "throughput", "latency"])
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--max-orders", default="1000000", help="the engine's resting order limit")
    parser.add_argument("--rate", default="100000", help="orders per second for latency runs")
    parser.add_argument("--engine", type=Path, help="benchmark this binary instead of building one")
    parser.add_argument("--save-baseline", metavar="NAME")
    parser.add_argument("--compare", metavar="NAME")
    args = parser.parse_args()

    build()
    tape = ensure_tape(args.tape)
    ensure_shared_memory()

    results = collect(args, args.engine or ENGINE, tape, args.tape)

    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    (RESULTS / f"{stamp}.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))

    if args.save_baseline:
        path = BENCH / "baselines" / f"{args.save_baseline}.json"
        path.write_text(json.dumps(results, indent=2) + "\n")
        print(f"saved {path}")

    if args.compare:
        baseline = json.loads((BENCH / "baselines" / f"{args.compare}.json").read_text())
        failures = compare(results, baseline)
        for failure in failures:
            print(f"REGRESSION {failure}")
        return 1 if failures else 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
