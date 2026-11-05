#!/usr/bin/env python3
"""Run the three-node cluster on EC2 hosts created by Terraform, with threads pinned to cores.

    bench.py wait              wait for every host to finish setup
    bench.py push DIR          copy binaries, jar and tapes from DIR to every host
    bench.py up | down         start or stop the cluster
    bench.py leader            print the leader's member ID
    bench.py kill N            stop node N's processes
    bench.py client ARGS...    run a tool on the client host
    bench.py timing            print each node's engine timing

Each host has 4 cores and 8 threads. Thread N and thread N+4 share a core, so the latency-critical
threads get a core each, with their siblings left idle.
"""
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
NODES = 3
SSH = ["ssh", "-o", "StrictHostKeyChecking=accept-new", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
USER = "admin"
HEARTBEAT = os.environ.get("LEADER_HEARTBEAT_TIMEOUT", "500ms")
STORE = os.environ.get("STORE", "/data")  # /data is local NVMe, /var/bench is the gp3 root volume
ENGINE = os.environ.get("ENGINE", "matching-engine")
MAX_ORDERS = os.environ.get("MAX_ORDERS", "1000000")
PINNED = os.environ.get("PINNED", "1") == "1"

# Thread name in the JVM, and the CPU it's pinned to. The engine takes CPU 1.
PINS = {"sender": 2, "receiver": 3, "consensus-module": 0}
SHARED_CPUS = "4-7"


def outputs():
    data = json.loads((HERE / "hosts.json").read_text())
    return data["public"], data["private"]


def ssh(public, script, check=True, stdin=None):
    return subprocess.run([*SSH, f"{USER}@{public}", script], capture_output=True, text=True, check=check, input=stdin)


def properties(i, private):
    ips = [private[f"node{n}"] for n in range(NODES)]
    members = "|".join(
        f"{n},{ips[n]}:{20000+n},{ips[n]}:{20010+n},{ips[n]}:{20020+n},{ips[n]}:{20030+n},{ips[n]}:{8010+n}"
        for n in range(NODES))
    me = ips[i]
    extra = os.environ.get("EXTRA_PROPS", "").replace(";", "\n")
    idle = os.environ.get("IDLE", "org.agrona.concurrent.BusySpinIdleStrategy")
    return f"""aeron.dir=/dev/shm/aeron
aeron.dir.delete.on.start=true
aeron.threading.mode={os.environ.get("THREADING", "DEDICATED")}
aeron.sender.idle.strategy={idle}
aeron.receiver.idle.strategy={idle}
aeron.cluster.idle.strategy={idle}
aeron.term.buffer.sparse.file=false
aeron.spies.simulate.connection=true
aeron.socket.so_sndbuf=4m
aeron.socket.so_rcvbuf=4m
aeron.rcv.initial.window.length=4m
aeron.archive.dir={STORE}/archive
aeron.archive.control.channel=aeron:udp?endpoint={me}:{8010+i}
aeron.archive.control.response.channel=aeron:udp?endpoint={me}:0
aeron.archive.replication.channel=aeron:udp?endpoint={me}:0
aeron.archive.threading.mode=SHARED
aeron.archive.recording.events.enabled=false
aeron.archive.file.sync.level={os.environ.get("SYNC", "1")}
aeron.archive.catalog.file.sync.level={os.environ.get("SYNC", "1")}
aeron.cluster.dir={STORE}/cluster
aeron.cluster.id=0
aeron.cluster.member.id={i}
aeron.cluster.members={members}
aeron.cluster.ingress.channel=aeron:udp
aeron.cluster.log.channel=aeron:udp?term-length={os.environ.get("LOG_TERM_LENGTH", "16m")}|control-mode=manual|control={me}:{20040+i}
aeron.cluster.replication.channel=aeron:udp?endpoint={me}:0
aeron.cluster.clock=io.aeron.cluster.NanosecondClusterClock
aeron.cluster.leader.heartbeat.timeout={HEARTBEAT}
aeron.cluster.leader.heartbeat.interval=100ms
aeron.cluster.session.timeout=30s
aeron.cluster.startup.canvass.timeout=5s
aeron.cluster.election.timeout=500ms
aeron.cluster.service.count=1
{extra}
"""


def start_node(i, public, private, wipe=True):
    me = private[f"node{i}"]
    clean = (f"sudo rm -rf /data/archive /data/cluster /var/bench /data/digest /data/timing; "
             f"sudo mkdir -p {STORE}; sudo chmod 777 {STORE}; ") if wipe else ""
    ssh(public[f"node{i}"], f"pkill -9 -f [C]lusteredMediaDriver; pkill -9 matching-engine; {clean}"
                            "sudo rm -rf /dev/shm/aeron /dev/shm/aeron-admin; "
                            "cat > /data/node.properties", stdin=properties(i, private), check=False)
    java = ("nohup taskset -c 0-7 java --add-opens java.base/jdk.internal.misc=ALL-UNNAMED "
            "--add-opens java.base/java.util.zip=ALL-UNNAMED -Xms512m -Xmx2g -XX:+UseZGC "
            "-cp /data/aeron-all.jar io.aeron.cluster.ClusteredMediaDriver /data/node.properties "
            "> /data/java.log 2>&1 < /dev/null &")
    ssh(public[f"node{i}"], java)
    pins = "true" if not PINNED else "; ".join(
        f"for t in $(ps -T -p $J -o tid=,comm= | grep -i {name[:12]} | awk '{{print $1}}'); do taskset -cp {cpu} $t; done"
        for name, cpu in PINS.items())
    engine_env = (f"CLUSTER_ID=0 AERON_DIR=/dev/shm/aeron MAX_ORDERS={MAX_ORDERS} PUB_STREAM_ID=1002 "
                  f"PUB_CHANNEL='aeron:udp?control={me}:{40450+i}|control-mode=dynamic|term-length=64m' "
                  f"ARCHIVE_CONTROL='aeron:udp?endpoint={me}:{8010+i}' DIGEST_FILE=/data/digest "
                  f"TIMING_FILE=/data/timing RUST_BACKTRACE=1 "
                  + ("" if os.environ.get("NO_DIGEST") else "DIGEST=1 "))
    pin_java = f"taskset -acp {SHARED_CPUS} $J > /dev/null; {pins} > /dev/null; " if PINNED else ""
    run_engine = f"taskset -c {SHARED_CPUS} /data/{ENGINE}" if PINNED else f"/data/{ENGINE}"
    pin_engine = "sleep 1; E=$(pgrep -x matching-engine); taskset -cp 1 $E > /dev/null" if PINNED else "true"
    ssh(public[f"node{i}"],
        "until [ -f /dev/shm/aeron/cnc.dat ]; do sleep 0.1; done; sleep 2; J=$(pgrep -f [C]lusteredMediaDriver); "
        f"{pin_java}{engine_env} nohup {run_engine} > /data/engine.log 2>&1 < /dev/null & {pin_engine}")


def main():
    command = sys.argv[1]
    public, private = outputs()
    hosts = list(public.values())

    if command == "wait":
        deadline = time.time() + 400
        for host in hosts:
            while ssh(host, "test -f /data/ready", check=False).returncode != 0:
                if time.time() > deadline:
                    sys.exit(f"{host} didn't finish setup")
                time.sleep(3)
        print("all hosts ready")
    elif command == "push":
        source = sys.argv[2]
        for host in hosts:
            # Names the children, because the hosts' /data belongs to root and rsync can't update it.
            children = [str(path) for path in sorted(Path(source).iterdir())]
            subprocess.run(["rsync", "-az", "-e", " ".join(SSH), *children, f"{USER}@{host}:/data/"], check=True)
        print("pushed")
    elif command == "up":
        # Stop every node before wiping any, or a wiped node rejoins and copies the old log back.
        for i in range(NODES):
            ssh(public[f"node{i}"], "pkill -9 -f [C]lusteredMediaDriver; pkill -9 matching-engine; true", check=False)
        time.sleep(1)
        for i in range(NODES):
            start_node(i, public, private)
        ssh(public["client"], "pkill -9 -f '[i]o.aeron.driver'; pkill -9 sbx-cluster; sudo rm -rf /dev/shm/aeron; true", check=False)
        ssh(public["client"], "nohup java --add-opens java.base/jdk.internal.misc=ALL-UNNAMED -XX:+UseZGC -Xmx1g "
            "-Daeron.dir=/dev/shm/aeron -Daeron.dir.delete.on.start=true -Daeron.threading.mode=DEDICATED "
            "-Daeron.socket.so_rcvbuf=4m -Daeron.socket.so_sndbuf=4m -Daeron.rcv.initial.window.length=4m "
            "-cp /data/aeron-all.jar io.aeron.driver.MediaDriver > /data/driver.log 2>&1 < /dev/null &", check=False)
        print("started")
    elif command == "down":
        for host in hosts:
            ssh(host, "pkill -9 -f [M]ediaDriver; pkill -9 [m]atching-engine", check=False)
    elif command == "leader":
        for i in range(NODES):
            out = ssh(public[f"node{i}"], "grep '^role' /data/engine.log | tail -1", check=False).stdout.strip()
            if out == "role Leader":
                print(i)
    elif command == "kill":
        ssh(public[f"node{sys.argv[2]}"], "pkill -9 -f [M]ediaDriver; pkill -9 [m]atching-engine", check=False)
    elif command == "start":
        start_node(int(sys.argv[2]), public, private, wipe=False)
    elif command == "snapshot":
        i = sys.argv[2]
        print(ssh(public[f"node{i}"], "java --add-opens java.base/jdk.internal.misc=ALL-UNNAMED -cp /data/aeron-all.jar "
                  "io.aeron.cluster.ClusterTool /data/cluster snapshot 2>&1 | tail -1", check=False).stdout.strip())
    elif command == "shell":
        print(ssh(public[f"node{sys.argv[2]}"], sys.argv[3], check=False).stdout.strip())
    elif command == "timing":
        for i in range(NODES):
            print(f"node{i}: " + ssh(public[f"node{i}"], "cat /data/timing", check=False).stdout)
    elif command == "client":
        nodes = ",".join(private[f"node{n}"] for n in range(NODES))
        env = f"AERON_DIR=/dev/shm/aeron CLUSTER_HOSTS={nodes} CLIENT_HOST={private['client']}"
        tool = shlex.join(sys.argv[2:])
        result = ssh(public["client"], f"cd /data && {env} ./{tool}", check=False)
        print(result.stdout[-1500:] + result.stderr[-800:])


main()
