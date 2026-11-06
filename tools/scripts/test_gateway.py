#!/usr/bin/env python3
"""End-to-end test of the POST guarantee: kill the leader mid-stream and check no order ran twice,
using real HTTP against a local three-node cluster. It needs Valkey on port 6379, for claims.

Build first: cargo build --release -p api -p tools
             cargo build -p matching-engine --profile dist --features silent
"""
import collections
import concurrent.futures
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUN = ROOT / ".run" / "cluster"
ORDERS = 4000
WORKERS = 16
KILL_AFTER = ORDERS // 3
SHM = "/Volumes/DevShm"
HOLD_WAIT = 90  # A new Valkey refuses new orders for this long, at most.

env = {**os.environ, "AERON_DIR": f"{SHM}/cluster-client", "THREADING": "SHARED",
       "LEADER_HEARTBEAT_TIMEOUT": "500ms", "RUST_LOG": "info",
       "EXTRA_PROPS": "aeron.cluster.election.timeout=500ms"}


def cluster(*args):
    return subprocess.run([sys.executable, "-I", str(ROOT / "tools/scripts/cluster.py"), *args], env=env,
                          capture_output=True, text=True, timeout=60).stdout.strip()


def uuid7():
    return str(uuid.UUID(int=int(time.time() * 1000) << 80 | 0x7 << 76 | secrets.randbits(12) << 64
                         | 0b10 << 62 | secrets.randbits(62)))


def post(body, player=None):
    request = urllib.request.Request("http://127.0.0.1:8000/api/v1/orders", json.dumps(body).encode(),
                                     {"content-type": "application/json",
                                      "x-player-id": player or str(uuid.uuid4())})
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, None
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"null").get("code")
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return 0, None


def place(index, sent):
    # A player of its own for each order, so that no two orders self-trade.
    player = str(uuid.uuid4())
    started = time.time()
    while True:
        body = {"client_order_id": uuid7(), "product_id": "ITEM", "side": "buy" if index % 2 else "sell",
                "type": "limit", "size": "1", "price": str(90 + index % 21)}
        status, code = post(body, player)
        sent.append(1)
        # A claim that wasn't confirmed sent nothing, so a new ID can try again. Anything else is
        # final, and an unknown outcome is never sent a second time.
        if code != "claim_unavailable" or time.time() - started > 20:
            return body["client_order_id"], status, time.time() - started
        time.sleep(0.1)


def main():
    cluster("up")
    time.sleep(13)
    leader = int(cluster("leader").splitlines()[-1])
    follower = (leader + 1) % 3
    tap = subprocess.Popen([ROOT / "target/release/sbx-tap", "--json", "--channel",
                            f"aeron:udp?endpoint=localhost:0|control=localhost:{40450 + follower}|session-id=1002"],
                           env=env, stdout=open(RUN / "tap.jsonl", "w"), stderr=subprocess.DEVNULL)
    api = subprocess.Popen([ROOT / "target/release/api"], stdout=open(RUN / "api.log", "w"), stderr=subprocess.STDOUT, env={
        **env, "CLUSTER_SESSION_TIMEOUT": "30s", "VALKEY_URL": "redis://localhost:6379",
        "MARKETS_FILE": str(ROOT / "services/api/rest/markets.yaml"),
        "CLUSTER_INGRESS_ENDPOINTS": "localhost:20000,localhost:20001,localhost:20002",
        "CLUSTER_RESPONSE_CHANNEL": "aeron:udp?endpoint=localhost:20301",
        "CLUSTER_REPORT_CHANNELS": ",".join(
            f"aeron:udp?endpoint=localhost:0|control=localhost:{40450 + i}|session-id=1002" for i in range(3))})
    try:
        deadline = time.time() + 20
        while post({})[0] == 0:
            assert time.time() < deadline, "api never started"
            time.sleep(0.2)

        assert post({"product_id": "ITEM", "side": "buy", "type": "limit", "size": "1", "price": "1"})[0] == 400, \
            "a missing client_order_id must be refused"

        deadline = time.time() + HOLD_WAIT
        while json.load(urllib.request.urlopen("http://127.0.0.1:8000/readyz"))["checks"]["order_claims"] is False:
            assert time.time() < deadline, "order claims never became ready"
            time.sleep(0.2)

        sent = []
        with concurrent.futures.ThreadPoolExecutor(WORKERS) as pool:
            futures = [pool.submit(place, i, sent) for i in range(ORDERS)]
            while len(sent) < KILL_AFTER:
                time.sleep(0.01)
            print(f"killing leader {leader} after {len(sent)} requests")
            cluster("kill", str(leader))
            results = [future.result() for future in futures]
        time.sleep(2)
    finally:
        api.terminate()
        tap.terminate()

    statuses = collections.Counter(status for _, status, _ in results)
    slowest = max(seconds for _, _, seconds in results)
    news = collections.Counter()
    with open(RUN / "tap.jsonl") as lines:
        for line in lines:
            event = json.loads(line)
            if event.get("event") == "execution" and event.get("exec_type") == "new":
                news[event["cl_ord_id"]] += 1

    # An unknown outcome may or may not have landed, but anything accepted must be there.
    wanted = {uuid.UUID(order_id).hex for order_id, status, _ in results if status == 201}
    missing = len(wanted - set(news))
    doubled = sum(1 for count in news.values() if count > 1)
    print(f"statuses {dict(statuses)}, requests {len(sent)}, slowest {slowest:.2f}s")
    print(f"orders {len(wanted)}, in tape {len(wanted) - missing}, missing {missing}, executed twice {doubled}")
    cluster("down")
    ok = set(statuses) <= {201, 503} and missing == 0 and doubled == 0
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


sys.exit(main())
