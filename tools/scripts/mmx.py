#!/usr/bin/env python3
"""Run the local exchange: start services in order, inspect them, and verify invariants.

    tools/scripts/mmx.py install     build the console/services and fetch the pinned Aeron jar
    tools/scripts/mmx.py up          start three nodes, infrastructure, services and console
    tools/scripts/mmx.py up --standalone   developer single-engine mode (no console)
    tools/scripts/mmx.py credentials show the console URL and first-run password
    tools/scripts/mmx.py down        stop the Rust services (add --all for infrastructure too)
    tools/scripts/mmx.py status      show what's running
    tools/scripts/mmx.py logs matching-engine     follow a service's log
    tools/scripts/mmx.py restart outbox   on a cluster, restart one service
    tools/scripts/mmx.py verify      restart the engine, run seeded load and check invariants
    tools/scripts/mmx.py observe     start Grafana, Prometheus and an OTLP collector on localhost
    tools/scripts/mmx.py e2e         on a running cluster, follow a real trade to a game webhook

Run `install` first; prerequisites are Rust, Node/npm, JDK 17+, CMake and Clang.
Local state defaults to .run/. Set MMX_RUN_DIR, MMX_SHM_DIR and MMX_PORT_OFFSET
for an isolated instance. This launcher is for development; Helm is the deployment path.
"""

import argparse
import importlib.util
import urllib.request
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
RUN = Path(os.environ.get("MMX_RUN_DIR", ROOT / ".run")).resolve()
PORT_OFFSET = int(os.environ.get("MMX_PORT_OFFSET", "0"))

def port(value):
    result = value + PORT_OFFSET
    if not 1 <= result <= 65535:
        sys.exit(f"MMX_PORT_OFFSET gives an invalid port: {result}")
    return result

CONTAINER_PREFIX = os.environ.get("MMX_CONTAINER_PREFIX", f"mmx-{PORT_OFFSET}" if PORT_OFFSET else "mmx")
BIN = ROOT / "target" / "release"
IS_MAC = platform.system() == "Darwin"
AERON_DIR = os.environ.get("AERON_DIR", "/Volumes/DevShm/aeron" if IS_MAC else "/dev/shm/aeron")
AERON_BIN = Path(os.environ.get("AERON_BIN", Path.home() / "Projects/aeron/cppbuild/Release/binaries"))
WS_ADDR = os.environ.get("WS_ADDR", f"127.0.0.1:{port(8090)}")

ENGINE_ENV = {
    "SUB_CHANNEL": os.environ.get("ORDERS_SUB_CHANNEL", "aeron:udp?endpoint=localhost:40123"),
    "SUB_STREAM_ID": "1001",
    "PUB_CHANNEL": os.environ.get(
        "EXECUTIONS_PUB_CHANNEL", "aeron:udp?control=localhost:40456|control-mode=dynamic"
    ),
    "PUB_STREAM_ID": "1002",
    "MAX_ORDERS": os.environ.get("MAX_ORDERS", "1000000"),
}

NATS = (["-p", f"127.0.0.1:{port(4222)}:4222"], "docker.io/library/nats:2-alpine")

INFRASTRUCTURE = {
    "mmx-nats": NATS,
    "mmx-questdb": (
        ["-p", "127.0.0.1:9000:9000", "-p", "127.0.0.1:9009:9009"],
        "docker.io/questdb/questdb:8.2.3",
    ),
    "mmx-postgres": (
        ["-p", "127.0.0.1:5432:5432", "-e", "POSTGRES_USER=mmx", "-e", "POSTGRES_PASSWORD=mmx",
         "-e", "POSTGRES_DB=mmx"],
        "docker.io/library/postgres:16-alpine",
    ),
}

NODE_LOGS = [*(f"engine{i}" for i in range(3)), *(f"java{i}" for i in range(3)), "clientdriver"]

# Name, command, extra environment, and the log line that means it's ready.
SERVICES = [
    ("aeronmd", [AERON_BIN / "aeronmd", f"-Daeron.dir={AERON_DIR}", "-Daeron.threading.mode=SHARED",
                 "-Daeron.dir.delete.on.start=true"], {}, None),
    ("matching-engine", [BIN / "matching-engine"], ENGINE_ENV, "Aeron subscription created"),
    ("market-data", [BIN / "market-data"], {}, "Starting Aeron polling loop"),
    ("trade-reporter", [BIN / "trade-reporter"], {}, "Startup complete"),
    ("observer", [BIN / "mmx-observer"], {"OTEL_METRIC_EXPORT_INTERVAL": "5000"}, "observing"),
    ("api", [BIN / "api"], {}, "Startup complete"),
    ("websocket", [BIN / "websocket"], {"WS_ADDR": WS_ADDR}, "Listening on"),
]


# tools/scripts/cluster.py owns the nodes, and these are the services around them. Tables written
# by older builds can't take this schema, so QuestDB gets its own container here.
CLUSTER_SCRIPT = ROOT / "tools" / "scripts" / "cluster.py"
CLUSTER_RUN = RUN / "cluster"
CLUSTER_NODES = 3
SHARED_MEMORY = Path(os.environ.get("MMX_SHM_DIR", "/Volumes/DevShm" if IS_MAC else "/dev/shm/mmx")).resolve()
CLUSTER_AERON_DIR = str(SHARED_MEMORY / "cluster-client")
# Consumers only leave a dead node once its image times out, which is 10 seconds by default.
CLIENT_PROPS = "-Daeron.image.liveness.timeout=2s"
CLUSTER_ENV = {"AERON_DIR": CLUSTER_AERON_DIR, "LEADER_HEARTBEAT_TIMEOUT": "500ms",
               "EXTRA_PROPS": "aeron.cluster.election.timeout=500ms", "CLIENT_PROPS": CLIENT_PROPS}
CLUSTER_QUESTDB_PORT = port(9100)
# A killed node's archive counts as alive for this long, and refuses to start until then.
NODE_RESTART_DELAY = 11
# Each subscriber to the report stream holds a 192 MB image, and more during a failover, when each
# also needs a replay on its next node. A 6 GB disk runs out then, though consumers do recover.
SHARED_MEMORY_GB = 8
CLUSTER_SHARED_MEMORY_GB = 5
OUTBOX_DIR = RUN / "outbox"
CHECKPOINT_DIR = RUN / "checkpoints"
OUTBOX_ENV_FILE = ROOT / "tools" / "local" / "outbox.env"
# What `mmx serve` writes. Edits stay out of git, because tools/local only holds the starting point.
MMX_STATE = RUN / "mmx"
SAMPLE_MARKETS = ROOT / "tools" / "local" / "markets.yaml"
MARKETS_FILE = MMX_STATE / "markets.yaml"
MMX_ENV_FILE = MMX_STATE / "outbox.env"
MMX_BIND = os.environ.get("MMX_BIND", f"127.0.0.1:{port(8093)}")
MMX_PORT = int(MMX_BIND.rsplit(":")[-1])
# Local only. The gateway and `mmx serve` must agree on it.
ADMIN_TOKEN = os.environ.get("MMX_ADMIN_TOKEN", "dev")

CLUSTER_INFRASTRUCTURE = {
    f"{CONTAINER_PREFIX}-nats": NATS,
    f"{CONTAINER_PREFIX}-cluster-questdb": (["-p", f"127.0.0.1:{CLUSTER_QUESTDB_PORT}:9000"],
                            "docker.io/questdb/questdb:8.2.3"),
    f"{CONTAINER_PREFIX}-valkey": (["-p", f"127.0.0.1:{port(6379)}:6379"], "docker.io/valkey/valkey:8-alpine"),
}


def read_env_file(path):
    lines = (line.strip() for line in path.read_text().splitlines())
    return dict(line.split("=", 1) for line in lines if line and not line.startswith("#"))


def outbox_settings():
    """The sample settings, overridden by what's been saved in the console."""
    saved = read_env_file(MMX_ENV_FILE) if MMX_ENV_FILE.exists() else {}
    settings = read_env_file(OUTBOX_ENV_FILE) if OUTBOX_ENV_FILE.exists() else {}
    if PORT_OFFSET:
        settings["OUTBOX_HTTP"] = f"127.0.0.1:{port(8092)}"
        settings["WEBHOOK_URL"] = f"http://127.0.0.1:{port(8095)}/webhook"
    return {**settings, **saved}


def seed_markets():
    MMX_STATE.mkdir(parents=True, exist_ok=True)
    if not MARKETS_FILE.exists():
        shutil.copy(SAMPLE_MARKETS, MARKETS_FILE)


def cluster_services():
    nodes = range(CLUSTER_NODES)
    tape = {"AERON_DIR": CLUSTER_AERON_DIR,
            "TAPE_ARCHIVES": ",".join(f"localhost:{port(8010) + i}" for i in nodes),
            "TAPE_REPORTS": ",".join(f"localhost:{port(40450) + i}" for i in nodes)}
    questdb = f"localhost:{CLUSTER_QUESTDB_PORT}"
    return [
        ("market-data", [BIN / "market-data"], {**tape, "CHECKPOINT_DIR": str(CHECKPOINT_DIR)},
         "Starting Aeron polling loop"),
        ("trade-reporter", [BIN / "trade-reporter"],
         {**tape, "QUESTDB_CONF": f"http::addr={questdb};"}, "Startup complete"),
        ("outbox", [BIN / "outbox"],
         {**outbox_settings(), **tape,
          "OUTBOX_DIR": str(OUTBOX_DIR)}, "outbox open"),
        ("websocket", [BIN / "websocket"], {"WS_ADDR": WS_ADDR}, "Listening on"),
        ("api", [BIN / "api"], {
            "AERON_DIR": CLUSTER_AERON_DIR, "MARKETS_FILE": str(MARKETS_FILE),
            "MMX_ADMIN_TOKEN": ADMIN_TOKEN, "BIND_ADDR": f"127.0.0.1:{port(8000)}",
            "QUESTDB_URL": f"http://{questdb}", "VALKEY_URL": f"redis://localhost:{port(6379)}",
            # The same value `cluster.py` gives the nodes.
            "CLUSTER_SESSION_TIMEOUT": "30s",
            "CLUSTER_INGRESS_ENDPOINTS": ",".join(f"localhost:{port(20000) + i}" for i in nodes),
            "CLUSTER_RESPONSE_CHANNEL": f"aeron:udp?endpoint=localhost:{port(20301)}",
            "CLUSTER_REPORT_CHANNELS": ",".join(
                f"aeron:udp?endpoint=localhost:0|control=localhost:{port(40450) + i}|session-id=1002"
                for i in nodes)}, "Startup complete"),
        ("mmx", [BIN / "mmx", "serve"], {
            "MMX_ADMIN_TOKEN": ADMIN_TOKEN, "MARKETS_FILE": str(MARKETS_FILE),
            "MMX_STATE_DIR": str(MMX_STATE), "QUESTDB_URL": f"http://{questdb}",
            "VALKEY_URL": f"redis://localhost:{port(6379)}", "MMX_GATEWAY_URLS": f"http://localhost:{port(8000)}",
            "MMX_BIND": MMX_BIND,
            "MMX_OUTBOX_URL": f"http://{outbox_settings()['OUTBOX_HTTP']}",
            "MMX_WEBSOCKET_URL": f"ws://{WS_ADDR}",
            "MMX_OUTBOX_RESTART": "mmx local restart outbox"}, "Admin API listening"),
    ]


def base_env():
    return {**os.environ, "AERON_DIR": AERON_DIR,
            "NATS_URL": f"nats://127.0.0.1:{port(4222)}",
            "RUST_LOG": os.environ.get("RUST_LOG", "info")}


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


def start_infrastructure(infrastructure=INFRASTRUCTURE):
    runtime = container_runtime()
    for name, (flags, image) in infrastructure.items():
        port = int(flags[1].split(":")[1])
        if port_open(port):
            continue
        subprocess.run([runtime, "rm", "-f", name], capture_output=True)
        subprocess.run([runtime, "run", "-d", "--name", name, *flags, image], check=True,
                       capture_output=True)
        print(f"started {name}")

    for name, (flags, _) in infrastructure.items():
        port = int(flags[1].split(":")[1])
        deadline = time.time() + 30
        while not port_open(port):
            if time.time() > deadline:
                sys.exit(f"{name} didn't open port {port}")
            time.sleep(0.2)


def ensure_shared_memory():
    if SHARED_MEMORY.exists():
        return
    if not IS_MAC or os.environ.get("MMX_SHM_DIR"):
        SHARED_MEMORY.mkdir(parents=True, exist_ok=True)
        return
    device = subprocess.run(["hdiutil", "attach", "-nomount", f"ram://{SHARED_MEMORY_GB * 1024 * 2048}"], check=True,
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


def cluster_script(*args):
    return subprocess.run([sys.executable, "-I", str(CLUSTER_SCRIPT), *args], check=True, cwd=ROOT,
                          env={**os.environ, **CLUSTER_ENV}, capture_output=True, text=True).stdout


def cluster_running():
    return any(CLUSTER_RUN.glob("*.pid"))


def cluster_leader():
    """The leading node, if there is one."""
    nodes = cluster_script("leader").split()
    return int(nodes[-1]) if nodes else None


def down_services():
    names = [name for name, *_ in [*SERVICES, *cluster_services()]]
    for name in reversed(dict.fromkeys(names)):
        stop(name)
    if cluster_running():
        cluster_script("down")
        (RUN / "cluster-stopped").write_text(str(time.time()))
        # Killed nodes leave these behind, and the next stack needs the room.
        for name in [*(f"cluster-{i}" for i in range(CLUSTER_NODES)), "cluster-client"]:
            shutil.rmtree(SHARED_MEMORY / name, ignore_errors=True)


def up_services(without=()):
    RUN.mkdir(parents=True, exist_ok=True)
    ensure_shared_memory()
    down_services()
    for service in SERVICES:
        if service[0] not in without:
            start(*service)


def start_cluster_nodes(fresh):
    """Starts the nodes and the client driver. Without `fresh`, they carry on from their tape."""
    if fresh:
        cluster_script("up")
        return
    stopped = RUN / "cluster-stopped"
    if stopped.exists():
        time.sleep(max(0.0, float(stopped.read_text()) + NODE_RESTART_DELAY - time.time()))
    for node in range(CLUSTER_NODES):
        cluster_script("start", str(node))
    # `cluster.py` only starts this while wiping the tape, but it stops it from this PID file.
    log = (CLUSTER_RUN / "clientdriver.log").open("a")
    specification = importlib.util.spec_from_file_location("mmx_cluster", CLUSTER_SCRIPT)
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    driver = subprocess.Popen([str(part) for part in module.driver_command()],
                              stdout=log, stderr=subprocess.STDOUT, cwd=ROOT,
                              env={**os.environ, **CLUSTER_ENV}, start_new_session=True)
    (CLUSTER_RUN / "clientdriver.pid").write_text(str(driver.pid))


def up_cluster(fresh):
    for path in (BIN / "api", BIN / "mmx", BIN / "market-data", BIN / "trade-reporter",
                 BIN / "outbox", BIN / "websocket", ROOT / "console/dist/index.html",
                 Path(os.environ.get("ENGINE", BIN / "matching-engine")),
                 Path(os.environ.get("AERON_JAR", RUN / "aeron-all-1.54.0.jar"))):
        if not path.exists():
            sys.exit(f"missing {path}, see STARTUP.md")

    start_infrastructure(CLUSTER_INFRASTRUCTURE)
    RUN.mkdir(parents=True, exist_ok=True)
    ensure_shared_memory()
    down_services()

    # The space comes back a moment after the processes that had it mapped have gone.
    deadline = time.time() + 10
    while (free_gb := shutil.disk_usage(SHARED_MEMORY).free / 2**30) < CLUSTER_SHARED_MEMORY_GB:
        if time.time() > deadline:
            sys.exit(f"{SHARED_MEMORY} has {free_gb:.1f} GB free and a cluster needs "
                     f"{CLUSTER_SHARED_MEMORY_GB} GB, see STARTUP.md")
        time.sleep(0.2)

    outbox = outbox_settings()
    ports = [port(8000), MMX_PORT, int(WS_ADDR.rsplit(":")[-1]), int(outbox["OUTBOX_HTTP"].rsplit(":")[-1])]
    taken = [port for port in ports if port_open(port)]
    if taken:
        sys.exit(f"something else is listening on {taken}")

    # Saved positions only mean something on the tape they came from.
    fresh = fresh or not (CLUSTER_RUN / "0" / "archive").exists()
    if fresh:
        shutil.rmtree(OUTBOX_DIR, ignore_errors=True)
        shutil.rmtree(CHECKPOINT_DIR, ignore_errors=True)
    OUTBOX_DIR.mkdir(exist_ok=True)
    seed_markets()

    start_cluster_nodes(fresh)
    deadline = time.time() + 60
    while cluster_leader() is None:
        if time.time() > deadline:
            sys.exit(f"no node became leader, see {CLUSTER_RUN}/engine0.log")
        time.sleep(0.2)
    print(f"cluster up, leader is node {cluster_leader()}")

    for name, command, extra_env, ready in cluster_services():
        # The tape is new, so anything QuestDB holds came from an older one.
        rebuild = ["--rebuild"] if fresh and name == "trade-reporter" else []
        start(name, [*command, *rebuild], extra_env, ready)
    # Claims establish their initial safe admission window before accepting new UUIDv7 IDs.
    # Wait here so the next documented curl works instead of racing that window.
    import urllib.error
    deadline = time.monotonic() + 120
    print("waiting for gateway admission readiness (the first start takes about 70 seconds)")
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port(8000)}/readyz", timeout=2) as response:
                if response.status == 200:
                    break
        except (OSError, urllib.error.URLError):
            time.sleep(0.5)
    else:
        sys.exit(f"gateway did not become ready; see {RUN / 'api.log'}")
    announce_console()


def announce_console():
    """The first password is only ever logged once, so it's easy to lose in a log."""
    print(f"console on http://127.0.0.1:{MMX_PORT}")
    for line in (RUN / "mmx.log").read_text(errors="replace").splitlines():
        if "first-run admin password" in line:
            print(f"first-run password: {line.rsplit(' ', 1)[-1]}")


def cmd_restart(args):
    """Restarts one service in place, for settings it only reads at start."""
    service = next(item for item in cluster_services() if item[0] == args.service)
    stop(args.service)
    start(*service)


def cmd_install(_):
    for executable in ("cargo", "npm", "java", "cmake", "clang"):
        if not shutil.which(executable):
            sys.exit(f"missing {executable}; install the prerequisites in STARTUP.md first")
    RUN.mkdir(parents=True, exist_ok=True)
    jar = Path(os.environ.get("AERON_JAR", RUN / "aeron-all-1.54.0.jar"))
    if not jar.exists():
        print("downloading Aeron 1.54.0")
        jar.parent.mkdir(parents=True, exist_ok=True)
        temporary = jar.with_suffix(".download")
        try:
            urllib.request.urlretrieve("https://repo.maven.apache.org/maven2/io/aeron/aeron-all/1.54.0/aeron-all-1.54.0.jar", temporary)
            temporary.replace(jar)
        finally:
            temporary.unlink(missing_ok=True)
    subprocess.run(["npm", "--prefix", "console", "ci"], cwd=ROOT, check=True)
    subprocess.run(["npm", "--prefix", "console", "run", "build"], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--workspace", "--release", "--locked"], cwd=ROOT, check=True)
    print("installed source build; run tools/scripts/mmx.py up")


def cmd_up(args):
    if not args.standalone:
        up_cluster(args.fresh)
        return
    start_infrastructure()
    up_services()


def cmd_down(args):
    down_services()
    if args.all:
        runtime = container_runtime()
        for name in [*INFRASTRUCTURE, *CLUSTER_INFRASTRUCTURE, "mmx-otel"]:
            subprocess.run([runtime, "rm", "-f", name], capture_output=True)


def cmd_status(_):
    clustered = cluster_running()
    if clustered:
        print(cluster_script("status"), end="")
        print(f"{'leader':16} {cluster_leader()}")
    for name, *_ in cluster_services() if clustered else SERVICES:
        pid = pid_of(name)
        print(f"{name:16} {'running, pid ' + str(pid) if pid else 'stopped'}")
    for name, (flags, _) in (CLUSTER_INFRASTRUCTURE if clustered else INFRASTRUCTURE).items():
        port = int(flags[1].split(":")[1])
        print(f"{name:16} {'listening on ' + str(port) if port_open(port) else 'down'}")


def cmd_observe(_):
    runtime = container_runtime()
    if not port_open(4318):
        subprocess.run([runtime, "rm", "-f", "mmx-otel"], capture_output=True)
        subprocess.run([runtime, "run", "-d", "--name", "mmx-otel", "-p", "127.0.0.1:3000:3000",
                        "-p", "127.0.0.1:4318:4318", "-p", "127.0.0.1:9090:9090",
                        "docker.io/grafana/otel-lgtm:latest"], check=True, capture_output=True)
    print("Grafana on http://localhost:3000, Prometheus on http://localhost:9090")


def cmd_logs(args):
    directory = CLUSTER_RUN if args.service in NODE_LOGS else RUN
    os.execvp("tail", ["tail", "-f", str(directory / f"{args.service}.log")])


def cmd_e2e(args):
    kill = [] if args.kill is None else ["--kill", args.kill]
    return subprocess.run([sys.executable, str(ROOT / "tools/scripts/e2e.py"), *kill],
                          cwd=ROOT).returncode


def cmd_verify(args):
    without = args.without.split(",") if args.without else []
    start_infrastructure()
    up_services(without)
    env = base_env()

    check = subprocess.Popen([BIN / "mmx-check", "--idle", "3"], env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
    capture = None
    if args.capture:
        capture = subprocess.Popen([BIN / "mmx-tap", "--json"], env=env,
                                   stdout=open(args.capture, "w"), stderr=subprocess.DEVNULL)
    time.sleep(1)

    load = [BIN / "mmx-load", "--orders", args.orders, "--seed", args.seed, "--rate", args.rate,
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

    up = commands.add_parser("up")
    mode = up.add_mutually_exclusive_group()
    mode.add_argument("--cluster", action="store_true", help="three-node cluster (the default)")
    mode.add_argument("--standalone", action="store_true", help="developer single-engine mode, no console")
    up.add_argument("--fresh", action="store_true", help="with --cluster, wipe the tape first")
    up.set_defaults(run=cmd_up)
    commands.add_parser("install", help="build source and fetch the pinned Aeron jar").set_defaults(run=cmd_install)
    commands.add_parser("credentials", help="show console URL and first-run password").set_defaults(run=lambda _: announce_console())
    commands.add_parser("status").set_defaults(run=cmd_status)
    commands.add_parser("observe").set_defaults(run=cmd_observe)
    e2e = commands.add_parser("e2e")
    e2e.add_argument("--kill", help="the node to kill, by default the leader")
    e2e.set_defaults(run=cmd_e2e)

    down = commands.add_parser("down")
    down.add_argument("--all", action="store_true", help="also remove infrastructure containers")
    down.set_defaults(run=cmd_down)

    logs = commands.add_parser("logs")
    names = dict.fromkeys(name for name, *_ in [*SERVICES, *cluster_services()])
    logs.add_argument("service", choices=[*names, *NODE_LOGS])
    logs.set_defaults(run=cmd_logs)

    restart = commands.add_parser("restart")
    restart.add_argument("service", choices=[name for name, *_ in cluster_services()])
    restart.set_defaults(run=cmd_restart)

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
