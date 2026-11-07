#!/usr/bin/env python3
"""Run a local three-node Aeron Cluster with the Rust engine as its service.

    cluster.py up | down | status
    cluster.py kill N | kill-engine N | start N
"""
import os, shutil, signal, subprocess, sys, time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUN = Path(os.environ.get("MMX_RUN_DIR", ROOT / ".run")) / "cluster"
JAR = Path(os.environ.get("AERON_JAR", Path(os.environ.get("MMX_RUN_DIR", ROOT / ".run")) / "aeron-all-1.54.0.jar"))
ENGINE = Path(os.environ.get("ENGINE", ROOT / "target" / "release" / "matching-engine"))
SHM = Path(os.environ.get("MMX_SHM_DIR", "/Volumes/DevShm" if sys.platform == "darwin" else "/dev/shm/mmx"))
PORT_OFFSET = int(os.environ.get("MMX_PORT_OFFSET", "0"))

def port(value):
    return value + PORT_OFFSET

NODES = 3
MEMBERS = "|".join(
    f"{i},localhost:{port(20000)+i},localhost:{port(20010)+i},localhost:{port(20020)+i},localhost:{port(20030)+i},localhost:{port(8010)+i}"
    for i in range(NODES))
HEARTBEAT = os.environ.get("LEADER_HEARTBEAT_TIMEOUT", "1s")


def props(i):
    return f"""
aeron.dir={SHM}/cluster-{i}
aeron.dir.delete.on.start=true
aeron.dir.delete.on.shutdown=true
aeron.threading.mode={os.environ.get("THREADING", "SHARED")}
aeron.term.buffer.sparse.file=true
aeron.spies.simulate.connection=true
aeron.archive.dir={RUN}/{i}/archive
aeron.archive.control.channel=aeron:udp?endpoint=localhost:{port(8010)+i}
aeron.archive.control.response.channel=aeron:udp?endpoint=localhost:0
aeron.archive.replication.channel=aeron:udp?endpoint=localhost:0
aeron.archive.threading.mode=SHARED
aeron.archive.recording.events.enabled=false
aeron.archive.file.sync.level={os.environ.get("SYNC", "1")}
aeron.archive.catalog.file.sync.level={os.environ.get("SYNC", "1")}
aeron.cluster.dir={RUN}/{i}/cluster
aeron.cluster.id=0
aeron.cluster.member.id={i}
aeron.cluster.members={MEMBERS}
aeron.cluster.ingress.channel=aeron:udp
aeron.cluster.log.channel=aeron:udp?term-length={os.environ.get("LOG_TERM_LENGTH", "16m")}|control-mode=manual|control=localhost:{port(20040)+i}
aeron.cluster.replication.channel=aeron:udp?endpoint=localhost:0
aeron.cluster.clock=io.aeron.cluster.NanosecondClusterClock
aeron.cluster.leader.heartbeat.timeout={HEARTBEAT}
aeron.cluster.leader.heartbeat.interval=100ms
aeron.cluster.session.timeout=30s
aeron.cluster.startup.canvass.timeout=5s
aeron.cluster.election.timeout=1s
aeron.cluster.service.count=1
""" + os.environ.get("EXTRA_PROPS", "").replace(";", "\n") + "\n"


def spawn(name, command, env=None):
    log = open(RUN / f"{name}.log", "a")
    process = subprocess.Popen([str(c) for c in command], stdout=log, stderr=subprocess.STDOUT,
                               env={**os.environ, **(env or {})}, cwd=ROOT, start_new_session=True)
    (RUN / f"{name}.pid").write_text(str(process.pid))


def stop(name, sig=signal.SIGKILL):
    path = RUN / f"{name}.pid"
    if path.exists():
        try:
            os.killpg(int(path.read_text()), sig)
        except ProcessLookupError:
            pass
        path.unlink()


def alive(name):
    path = RUN / f"{name}.pid"
    if not path.exists():
        return False
    try:
        os.kill(int(path.read_text()), 0)
        return True
    except ProcessLookupError:
        return False


def start(i):
    (RUN / str(i)).mkdir(parents=True, exist_ok=True)
    (RUN / f"node{i}.properties").write_text(props(i))
    for name in (f"engine{i}.log", f"digest{i}"):
        (RUN / name).unlink(missing_ok=True)
    shutil.rmtree(SHM / f"cluster-{i}", ignore_errors=True)
    spawn(f"java{i}", ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED",
                       "--add-opens", "java.base/java.util.zip=ALL-UNNAMED", "-Xms256m", "-Xmx1g",
                       "-XX:+UseZGC", "-cp", JAR, "io.aeron.cluster.ClusteredMediaDriver",
                       RUN / f"node{i}.properties"])
    deadline = time.time() + 30
    while not (SHM / f"cluster-{i}" / "cnc.dat").exists():
        if time.time() > deadline:
            sys.exit(f"node {i} driver didn't start, see {RUN}/java{i}.log")
        time.sleep(0.05)
    time.sleep(2)
    if not alive(f"java{i}"):
        sys.exit(f"node {i} exited on startup, see {RUN}/java{i}.log")
    spawn(f"engine{i}", [ENGINE], {
        "CLUSTER_ID": "0", "AERON_DIR": f"{SHM}/cluster-{i}", "MAX_ORDERS": os.environ.get("MAX_ORDERS", "1000000"),
        "PUB_CHANNEL": f"aeron:udp?control=localhost:{port(40450)+i}|control-mode=dynamic|term-length=64m",
        "PUB_STREAM_ID": "1002", "NODE_ID": str(i), "RUST_BACKTRACE": "1",
        "ARCHIVE_CONTROL": f"aeron:udp?endpoint=localhost:{port(8010)+i}",
        "TIMING_FILE": f"{RUN}/timing{i}", "DIGEST_FILE": f"{RUN}/digest{i}", **({} if os.environ.get("NO_DIGEST") else {"DIGEST": "1"})})


def down():
    for i in range(NODES):
        stop(f"engine{i}")
        stop(f"java{i}")
    stop("clientdriver")


def driver_command():
    properties = [f"-Daeron.dir={SHM}/cluster-client",
                  f"-Daeron.threading.mode={os.environ.get('CLIENT_THREADING', 'SHARED')}",
                  "-Daeron.dir.delete.on.start=true", *os.environ.get("CLIENT_PROPS", "").split()]
    native = Path(os.environ.get("AERON_BIN", Path.home() / "Projects/aeron/cppbuild/Release/binaries")) / "aeronmd"
    if native.exists():
        return [native, *properties]
    return ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED",
            *properties, "-cp", JAR, "io.aeron.driver.MediaDriver"]


def main():
    command = sys.argv[1]
    if command == "up":
        down()
        time.sleep(0.5)
        shutil.rmtree(RUN, ignore_errors=True)
        RUN.mkdir(parents=True)
        for i in range(NODES):
            shutil.rmtree(SHM / f"cluster-{i}", ignore_errors=True)
            start(i)
        spawn("clientdriver", driver_command())
    elif command == "down":
        down()
    elif command == "status":
        for i in range(NODES):
            print(i, "java", alive(f"java{i}"), "engine", alive(f"engine{i}"))
    elif command == "leader":
        for i in range(NODES):
            log = RUN / f"engine{i}.log"
            if alive(f"engine{i}") and log.exists():
                roles = [l for l in log.read_text(errors="replace").splitlines() if l.startswith("role")]
                if roles and roles[-1] == "role Leader":
                    print(i)
    elif command == "snapshot":
        i = int(sys.argv[2])
        result = subprocess.run(["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED",
                                 "-cp", str(JAR), "io.aeron.cluster.ClusterTool",
                                 str(RUN / str(i) / "cluster"), "snapshot"],
                                capture_output=True, text=True)
        print((result.stdout + result.stderr).strip()[-200:])
    elif command == "kill":
        i = int(sys.argv[2])
        stop(f"engine{i}")
        stop(f"java{i}")
    elif command == "kill-engine":
        stop(f"engine{sys.argv[2]}")
    elif command == "start":
        start(int(sys.argv[2]))


if __name__ == "__main__":
    main()
