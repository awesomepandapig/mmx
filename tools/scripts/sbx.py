#!/usr/bin/env python3
"""Run the local exchange: start services in order, inspect them, and verify invariants.

    tools/scripts/sbx.py up          start infrastructure and every Rust service
    tools/scripts/sbx.py down        stop the Rust services (add --all for infrastructure too)
    tools/scripts/sbx.py status      show what's running
    tools/scripts/sbx.py logs matching-engine     follow a service's log
    tools/scripts/sbx.py verify      restart the engine, run seeded load and check invariants
    tools/scripts/sbx.py observe     start Grafana, Prometheus and an OTLP collector on localhost

Build first with `cargo build --workspace --release`.
"""

import argparse
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUN = ROOT / ".run"
BIN = ROOT / "target" / "release"
IS_MAC = platform.system() == "Darwin"
AERON_DIR = os.environ.get("AERON_DIR", "/Volumes/DevShm/aeron" if IS_MAC else "/dev/shm/aeron")
AERON_BIN = Path(os.environ.get("AERON_BIN", Path.home() / "Projects/aeron/cppbuild/Release/binaries"))
WS_ADDR = os.environ.get("WS_ADDR", "127.0.0.1:8090" if IS_MAC else "127.0.0.1:8080")

ENGINE_ENV = {
    "SUB_CHANNEL": os.environ.get("ORDERS_SUB_CHANNEL", "aeron:udp?endpoint=localhost:40123"),
    "SUB_STREAM_ID": "1001",
    "PUB_CHANNEL": os.environ.get(
        "EXECUTIONS_PUB_CHANNEL", "aeron:udp?control=localhost:40456|control-mode=dynamic"
    ),
    "PUB_STREAM_ID": "1002",
    "MAX_ORDERS": os.environ.get("MAX_ORDERS", "1000000"),
}

INFRASTRUCTURE = {
    "sbx-nats": (["-p", "127.0.0.1:4222:4222"], "docker.io/library/nats:2-alpine"),
    "sbx-questdb": (
        ["-p", "127.0.0.1:9000:9000", "-p", "127.0.0.1:9009:9009"],
        "docker.io/questdb/questdb:8.2.3",
    ),
    "sbx-postgres": (
        ["-p", "127.0.0.1:5432:5432", "-e", "POSTGRES_USER=sbx", "-e", "POSTGRES_PASSWORD=sbx",
         "-e", "POSTGRES_DB=sbx"],
        "docker.io/library/postgres:16-alpine",
    ),
}

# Name, command, extra environment, and the log line that means it's ready.
SERVICES = [
    ("aeronmd", [AERON_BIN / "aeronmd", f"-Daeron.dir={AERON_DIR}", "-Daeron.threading.mode=SHARED",
                 "-Daeron.dir.delete.on.start=true"], {}, None),
    ("matching-engine", [BIN / "matching-engine"], ENGINE_ENV, "Aeron subscription created"),
    ("market-data", [BIN / "market-data"], {}, "Starting Aeron polling loop"),
    ("trade-reporter", [BIN / "trade-reporter"], {}, "Startup complete"),
    ("observer", [BIN / "sbx-observer"], {"OTEL_METRIC_EXPORT_INTERVAL": "5000"}, "observing"),
    ("api", [BIN / "api"], {}, "Startup complete"),
    ("websocket", [BIN / "websocket"], {"WS_ADDR": WS_ADDR}, "Listening on"),
]


def base_env():
    return {**os.environ, "AERON_DIR": AERON_DIR, "RUST_LOG": os.environ.get("RUST_LOG", "info")}


def pid_of(name):
    path = RUN / f"{name}.pid"
    if not path.exists():
        return None
    pid = int(path.read_text())
    try:
        # A dead child stays a zombie until it's reaped, and zombies still answer kill(0).
        reaped, _ = os.waitpid(pid, os.WNOHANG)
        if reaped:
            return None
    except ChildProcessError:
        pass
    try:
        os.kill(pid, 0)
        return pid
    except ProcessLookupError:
        return None


def container_runtime():
    runtime = next((tool for tool in ("container", "docker") if shutil.which(tool)), None)
    if runtime is None:
        sys.exit("need Apple `container` or `docker` for NATS, QuestDB and Postgres")
    return runtime


def port_open(port):
    with socket.socket() as sock:
        sock.settimeout(0.2)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def start_infrastructure():
    runtime = container_runtime()
    for name, (flags, image) in INFRASTRUCTURE.items():
        port = int(flags[1].split(":")[1])
        if port_open(port):
            continue
        subprocess.run([runtime, "rm", "-f", name], capture_output=True)
        subprocess.run([runtime, "run", "-d", "--name", name, *flags, image], check=True,
                       capture_output=True)
        print(f"started {name}")

    for name, (flags, _) in INFRASTRUCTURE.items():
        port = int(flags[1].split(":")[1])
        deadline = time.time() + 30
        while not port_open(port):
            if time.time() > deadline:
                sys.exit(f"{name} didn't open port {port}")
            time.sleep(0.2)


def ensure_shared_memory():
    if not IS_MAC or Path("/Volumes/DevShm").exists():
        return
    device = subprocess.run(["hdiutil", "attach", "-nomount", f"ram://{2048 * 2048}"], check=True,
                            capture_output=True, text=True).stdout.strip()
    subprocess.run(["diskutil", "erasevolume", "HFS+", "DevShm", device], check=True,
                   capture_output=True)
    print("created the DevShm RAM disk")


def start(name, command, extra_env, ready):
    log = RUN / f"{name}.log"
    log.write_text("")
    process = subprocess.Popen([str(part) for part in command], stdout=log.open("a"),
                               stderr=subprocess.STDOUT, env={**base_env(), **extra_env},
                               cwd=ROOT, start_new_session=True)
    (RUN / f"{name}.pid").write_text(str(process.pid))

    deadline = time.time() + 30
    while time.time() < deadline:
        if process.poll() is not None:
            sys.exit(f"{name} exited on startup, see {log}")
        if ready is None:
            time.sleep(1)
            break
        if ready in log.read_text(errors="replace"):
            break
        time.sleep(0.05)
    else:
        sys.exit(f"{name} didn't become ready, see {log}")
    print(f"started {name}")


def stop(name):
    pid = pid_of(name)
    if pid is None:
        return
    os.kill(pid, signal.SIGTERM)
    deadline = time.time() + 5
    while pid_of(name) and time.time() < deadline:
        time.sleep(0.05)
    if pid_of(name):
        os.kill(pid, signal.SIGKILL)
    (RUN / f"{name}.pid").unlink(missing_ok=True)


def down_services():
    for name, *_ in reversed(SERVICES):
        stop(name)


def up_services(without=()):
    RUN.mkdir(exist_ok=True)
    ensure_shared_memory()
    down_services()
    for service in SERVICES:
        if service[0] not in without:
            start(*service)


def cmd_up(_):
    start_infrastructure()
    up_services()


def cmd_down(args):
    down_services()
    if args.all:
        runtime = container_runtime()
        for name in [*INFRASTRUCTURE, "sbx-otel"]:
            subprocess.run([runtime, "rm", "-f", name], capture_output=True)


def cmd_status(_):
    for name, *_ in SERVICES:
        pid = pid_of(name)
        print(f"{name:16} {'running, pid ' + str(pid) if pid else 'stopped'}")
    for name, (flags, _) in INFRASTRUCTURE.items():
        port = int(flags[1].split(":")[1])
        print(f"{name:16} {'listening on ' + str(port) if port_open(port) else 'down'}")


def cmd_observe(_):
    runtime = container_runtime()
    if not port_open(4318):
        subprocess.run([runtime, "rm", "-f", "sbx-otel"], capture_output=True)
        subprocess.run([runtime, "run", "-d", "--name", "sbx-otel", "-p", "127.0.0.1:3000:3000",
                        "-p", "127.0.0.1:4318:4318", "-p", "127.0.0.1:9090:9090",
                        "docker.io/grafana/otel-lgtm:latest"], check=True, capture_output=True)
    print("Grafana on http://localhost:3000, Prometheus on http://localhost:9090")


def cmd_logs(args):
    os.execvp("tail", ["tail", "-f", str(RUN / f"{args.service}.log")])


def cmd_verify(args):
    without = args.without.split(",") if args.without else []
    start_infrastructure()
    up_services(without)
    env = base_env()

    check = subprocess.Popen([BIN / "sbx-check", "--idle", "3"], env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
    capture = None
    if args.capture:
        capture = subprocess.Popen([BIN / "sbx-tap", "--json"], env=env,
                                   stdout=open(args.capture, "w"), stderr=subprocess.DEVNULL)
    time.sleep(1)

    load = [BIN / "sbx-load", "--orders", args.orders, "--seed", args.seed, "--rate", args.rate,
            "--accounts", args.accounts, "--symbols", args.symbols, "--market-pct",
            args.market_pct, "--cancel-pct", args.cancel_pct]
    subprocess.run([str(part) for part in load], env=env, check=True)

    output, _ = check.communicate(timeout=300)
    print(output, end="")
    if capture:
        capture.terminate()

    crashed = [name for name, *_ in SERVICES if name not in without and pid_of(name) is None]
    if crashed:
        print(f"CRASHED: {', '.join(crashed)}")
        for name in crashed:
            lines = (RUN / f"{name}.log").read_text(errors="replace").splitlines()
            for line in [line for line in lines if "panicked" in line or "assertion" in line][:2]:
                print(f"  {name}: {line[:160]}")
    return 1 if check.returncode or crashed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)

    commands.add_parser("up").set_defaults(run=cmd_up)
    commands.add_parser("status").set_defaults(run=cmd_status)
    commands.add_parser("observe").set_defaults(run=cmd_observe)

    down = commands.add_parser("down")
    down.add_argument("--all", action="store_true", help="also remove infrastructure containers")
    down.set_defaults(run=cmd_down)

    logs = commands.add_parser("logs")
    logs.add_argument("service", choices=[name for name, *_ in SERVICES])
    logs.set_defaults(run=cmd_logs)

    verify = commands.add_parser("verify")
    for flag, default in (("orders", "100000"), ("seed", "1"), ("rate", "0"), ("accounts", "100"),
                          ("symbols", "JSP"), ("market-pct", "10"), ("cancel-pct", "0")):
        verify.add_argument(f"--{flag}", default=default)
    verify.add_argument("--capture", help="also write every event to this JSONL file")
    verify.add_argument("--without", help="comma-separated services to leave out")
    verify.set_defaults(run=cmd_verify)

    args = parser.parse_args()
    sys.exit(args.run(args) or 0)


if __name__ == "__main__":
    main()
