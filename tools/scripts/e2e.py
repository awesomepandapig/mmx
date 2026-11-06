#!/usr/bin/env python3
"""Follow real trades through a running cluster, from the REST API to a game webhook.

    tools/scripts/sbx.py up --cluster
    tools/scripts/sbx.py e2e
    tools/scripts/sbx.py e2e --kill 0     kill this node instead of the leader

It stands in for the game: it receives and verifies webhooks, places orders over REST, and watches
NATS and the websocket. It checks that a repeated POST places nothing, including across two
gateways, that order status can be trusted, and that losing Valkey refuses new orders but not
cancels. Then it kills the leader while orders are flowing, checks that every order is answered and
every webhook arrives exactly once, and restarts the node. It exits non-zero with a message on the
first failure. Standard library only.
"""

import argparse
import base64
import collections
import hashlib
import hmac
import http.server
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

import sbx

GATEWAY = "http://127.0.0.1:8000"
SECOND_GATEWAY = "http://127.0.0.1:8001"
SECOND_GATEWAY_LOG = sbx.RUN / "api-second.log"
SECOND_GATEWAY_RESPONSES = "aeron:udp?endpoint=localhost:20302"
SYMBOL = "ITEM"
PRICE = "100"
MAKER = "e2e00000-0000-4000-8000-00000000000a"
TAKER = "e2e00000-0000-4000-8000-00000000000b"
PAIRS = 30
KILL_AFTER_PAIRS = 10
PAIR_GAP = 0.05
WEBHOOK_WAIT = 45  # The outbox may be backing off from a time when nothing was listening.
QUERY_WAIT = 15
FAILOVER_WAIT = 30
REJOIN_WAIT = 60
HOLD_WAIT = 90  # A new Valkey refuses new orders for 70 seconds.
ROUNDS = 25
RACERS = 6
ORDER_ID_WINDOW = 60
SETTLED_AFTER = 98
FILL_ID = re.compile(r"^[0-9a-f]{32}$")
JAR = os.environ.get("AERON_JAR", str(sbx.RUN / "aeron-all-1.54.0.jar"))
retries = collections.Counter()


class Failed(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failed(message)


def passed(message):
    print(f"  ok   {message}", flush=True)


def wait_for(condition, timeout, what):
    """Waits for `condition`. `what` can be a function, so a timeout reports the latest state."""
    deadline = time.time() + timeout
    while True:
        value = condition()
        if value:
            return value
        if time.time() > deadline:
            raise Failed(f"timed out after {timeout}s waiting for {what() if callable(what) else what}")
        time.sleep(0.05)


class Game(http.server.ThreadingHTTPServer):
    """The stand-in game. It verifies every delivery before reading it, as a real game must."""

    daemon_threads = True

    def __init__(self, url, secret):
        address = re.match(r"http://([^:/]+):(\d+)", url)
        super().__init__((address[1], int(address[2])), WebhookHandler)
        self.secret = secret.encode()
        self.lock = threading.Lock()
        self.events = []
        self.bad_signatures = 0
        self.stale_timestamps = 0

    def verify(self, timestamp, signature, body):
        message = timestamp.encode() + b"." + body
        expected = "v1=" + hmac.new(self.secret, message, hashlib.sha256).hexdigest()
        return hmac.compare_digest(expected, signature)

    def of(self, order_id, kind=None):
        with self.lock:
            return [event for event in self.events if event["client_order_id"] == order_id
                    and (kind is None or event["type"] == kind)]

    def seen(self):
        with self.lock:
            return list(self.events)


class WebhookHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        timestamp = self.headers.get("X-MMX-Timestamp", "")
        signature = self.headers.get("X-MMX-Signature", "")
        game = self.server
        if not game.verify(timestamp, signature, body):
            with game.lock:
                game.bad_signatures += 1
            self.send_response(401)
            self.end_headers()
            return
        with game.lock:
            game.stale_timestamps += abs(time.time() - int(timestamp)) > 300
            game.events.extend(json.loads(body)["events"])
        self.send_response(200)
        self.end_headers()

    def log_message(self, *_):
        pass


class NatsWatch(threading.Thread):
    """Counts messages per subject, speaking just enough of the NATS protocol."""

    def __init__(self, subjects):
        super().__init__(daemon=True)
        self.counts = collections.Counter()
        self.sock = socket.create_connection(("127.0.0.1", 4222), timeout=5)
        self.sock.settimeout(None)
        self.file = self.sock.makefile("rb")
        self.file.readline()  # INFO
        request = "CONNECT {}\r\n" + "".join(f"SUB {subject} {i}\r\n" for i, subject in
                                             enumerate(subjects, 1))
        self.sock.sendall(request.encode())

    def run(self):
        try:
            while line := self.file.readline():
                parts = line.split()
                if parts[:1] == [b"PING"]:
                    self.sock.sendall(b"PONG\r\n")
                elif parts[:1] == [b"MSG"]:
                    self.file.read(int(parts[-1]) + 2)
                    self.counts[parts[1].decode()] += 1
        except OSError:
            pass


class WebSocketWatch(threading.Thread):
    """Counts frames per channel that mention the symbol."""

    def __init__(self, address, symbol):
        super().__init__(daemon=True)
        self.symbol = symbol
        self.counts = collections.Counter()
        host, port = address.rsplit(":", 1)
        self.sock = socket.create_connection((host, int(port)), timeout=5)
        key = base64.b64encode(uuid.uuid4().bytes).decode()
        self.sock.sendall(f"GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n"
                          f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                          "Sec-WebSocket-Version: 13\r\n\r\n".encode())
        self.file = self.sock.makefile("rb")
        require(b" 101 " in self.file.readline(), "the websocket refused the upgrade")
        while self.file.readline().strip():
            pass
        self.sock.settimeout(None)
        for channel in ("heartbeats", "level2", "ticker", "candles"):
            self.send({"type": "subscribe", "channel": channel, "product_ids": [symbol]})

    def send(self, message):
        text = json.dumps(message).encode()
        require(len(text) < 126, "this client only sends short frames")
        mask = uuid.uuid4().bytes[:4]
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(text))
        self.sock.sendall(bytes([0x81, 0x80 | len(text)]) + mask + masked)

    def run(self):
        try:
            while header := self.file.read(2):
                length = header[1] & 0x7f
                if length >= 126:
                    length = int.from_bytes(self.file.read(2 if length == 126 else 8), "big")
                payload = self.file.read(length)
                if header[0] & 0x0f == 1:
                    frame = json.loads(payload)
                    if self.symbol in payload.decode():
                        self.counts[frame.get("channel", frame.get("type"))] += 1
        except OSError:
            pass


def uuid7(age=0.0):
    """A UUIDv7 (RFC 9562) made `age` seconds ago. Python's standard library may not have one."""
    milliseconds = int((time.time() - age) * 1000)
    value = milliseconds << 80 | 0x7 << 76 | secrets.randbits(12) << 64 | 0b10 << 62 \
        | secrets.randbits(62)
    return str(uuid.UUID(int=value))


def call(method, path, player, body=None, gateway=GATEWAY):
    """Returns the status, the parsed reply, and 0 and `None` if the gateway didn't answer."""
    headers = {"content-type": "application/json", "x-player-id": player}
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(f"{gateway}/api/v1{path}", data, headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"null")
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return 0, None


def order_body(side, size, order_id=None):
    return {"client_order_id": order_id or uuid7(), "product_id": SYMBOL, "side": side,
            "type": "limit", "size": size, "price": PRICE}


def place(player, side, size):
    """Places an order and returns its ID and how it was answered. A refusal that sent nothing can
    be tried again with a new ID, since the claim may have landed even though it wasn't confirmed.
    An unknown outcome is left for `settle`, because a caller must never send it again.
    """
    deadline = time.time() + FAILOVER_WAIT
    while True:
        body = order_body(side, size)
        status, reply = call("POST", "/orders", player, body)
        if status == 503 and reply.get("code") == "claim_unavailable" and time.time() < deadline:
            retries["claim_unavailable"] += 1
            time.sleep(0.1)
            continue
        return body["client_order_id"], status, reply


def recorded(order_id, player, wanted):
    """Reads an order back through QuestDB until `wanted` holds."""
    def attempt():
        status, order = call("GET", f"/orders/{order_id}", player)
        return order if status == 200 and "product_id" in order and wanted(order) else None
    return wait_for(attempt, QUERY_WAIT, f"GET to show order {order_id} as expected")


def clear_book():
    """Cancels anything an earlier failed run left resting."""
    for player in (MAKER, TAKER):
        # This reads the recorder, which is briefly behind if it was following a killed node.
        def attempt():
            status, reply = call("DELETE", "/orders", player)
            if status == 503 and reply["code"] == "recorder_behind":
                retries["recorder_behind"] += 1
                return None
            require(status == 200, f"cancelling old orders answered {status}: {reply}")
            require(not reply["unresolved"], f"old orders are unresolved: {reply}")
            return True
        wait_for(attempt, FAILOVER_WAIT, "the recorder to catch up")


def check_trade(game):
    print("A trade", flush=True)
    maker_id, status, reply = place(MAKER, "sell", "10")
    require(status == 201 and reply["status"] == "open",
            f"the resting order answered {status}: {reply}")
    passed(f"resting sell accepted by the engine, {maker_id}")
    taker_id, status, reply = place(TAKER, "buy", "4")
    require(status == 201, f"the crossing order answered {status}: {reply}")
    passed(f"crossing buy accepted by the engine, {taker_id}")

    fills = wait_for(lambda: len(game.of(maker_id, "fill")) and len(game.of(taker_id, "fill"))
                     and game.of(maker_id, "fill") + game.of(taker_id, "fill"),
                     WEBHOOK_WAIT, lambda: f"the fill webhooks, {game.bad_signatures} bad signatures")
    maker_fill, taker_fill = fills
    for fill in fills:
        require(FILL_ID.match(fill.get("fill_id", "")), f"bad fill_id in {fill}")
        require((fill["last_qty"], fill["last_px"]) == ("4", PRICE), f"wrong fill: {fill}")
        require(fill["symbol"] == SYMBOL, f"wrong symbol: {fill}")
    require(maker_fill["match_id"] == taker_fill["match_id"], f"legs don't match: {fills}")
    require(maker_fill["fill_id"] != taker_fill["fill_id"], "both legs share a fill_id")
    require(taker_fill["terminal"] and not maker_fill["terminal"], f"wrong terminal flags: {fills}")
    require(game.bad_signatures == 0, "a webhook failed its signature check")
    passed(f"signed fill webhooks arrived, fill_ids {maker_fill['fill_id']} and "
           f"{taker_fill['fill_id']}")

    maker = recorded(maker_id, MAKER, lambda order: order["filled_size"] == "4")
    require(maker["status"] == "open" and maker["size"] == "10", f"wrong order: {maker}")
    taker = recorded(taker_id, TAKER, lambda order: order["status"] == "filled")
    require(taker["filled_size"] == "4", f"wrong order: {taker}")
    status, listed = call("GET", f"/orders?product_id={SYMBOL}&status=open", MAKER)
    require(status == 200 and maker_id in [order["id"] for order in listed],
            f"GET orders didn't list the open order: {status} {listed}")
    passed("GET shows both orders through QuestDB")

    status, reply = call("DELETE", f"/orders/{maker_id}", MAKER)
    require(status == 200 and reply["status"] == "canceled", f"cancel answered {status}: {reply}")
    canceled = wait_for(lambda: game.of(maker_id, "canceled"), WEBHOOK_WAIT, "the cancel webhook")
    require(len(canceled) == 1, f"expected one cancel: {canceled}")
    require(canceled[0]["terminal"] and canceled[0]["release_qty"] == "6",
            f"wrong cancel: {canceled[0]}")
    maker = recorded(maker_id, MAKER, lambda order: order["status"] == "canceled")
    require(maker["executed_value"] == "400", f"a cancel lost the fills: {maker}")
    passed("cancel webhook arrived, releasing 6, and GET shows it")


def readiness(gateway):
    try:
        with urllib.request.urlopen(f"{gateway}/readyz", timeout=5) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        return json.loads(error.read())
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return None


def claims_ready(gateway=GATEWAY):
    reply = readiness(gateway)
    return bool(reply and reply["checks"].get("order_claims"))


def wait_for_claims(gateway=GATEWAY):
    wait_for(lambda: claims_ready(gateway), HOLD_WAIT, f"order claims on {gateway}")


class SecondGateway:
    """Another real gateway, sharing only what a real one would. One that was never told about an
    order is also the way to prove that a gateway holds no state that matters."""

    def __init__(self):
        self.process = None

    def start(self):
        settings = next(extra for name, _, extra, _ in sbx.cluster_services() if name == "api")
        env = {**sbx.base_env(), **settings, "BIND_ADDR": SECOND_GATEWAY.rsplit("/", 1)[1],
               "CLUSTER_RESPONSE_CHANNEL": SECOND_GATEWAY_RESPONSES}
        self.process = subprocess.Popen([str(sbx.BIN / "api")], env=env, cwd=sbx.ROOT,
                                        stdout=SECOND_GATEWAY_LOG.open("w"),
                                        stderr=subprocess.STDOUT)
        wait_for(lambda: readiness(SECOND_GATEWAY), QUERY_WAIT, "the second gateway to start")

    def stop(self):
        if self.process:
            self.process.terminate()
            self.process.wait(timeout=10)
            self.process = None


def race(order, player):
    """POSTs one order to both gateways at the same moment."""
    gateways = [GATEWAY, SECOND_GATEWAY] * (RACERS // 2)
    start = threading.Barrier(len(gateways))
    replies = [None] * len(gateways)

    def post(index, gateway):
        start.wait()
        replies[index] = call("POST", "/orders", player, order, gateway)

    threads = [threading.Thread(target=post, args=(index, gateway))
               for index, gateway in enumerate(gateways)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return replies


def check_one_winner(replies, order_id):
    """Returns whether the order was placed. A slow Valkey refuses some requests, and may refuse all
    of them, so the fills in `check_double_post` are what prove nothing else ran."""
    created = [reply for status, reply in replies if status == 201]
    require(len(created) <= 1, f"{order_id} was placed {len(created)} times: {replies}")
    for status, reply in replies:
        if status == 201:
            continue
        require(reply and reply["id"] == order_id, f"bad reply: {status} {reply}")
        if status == 409:
            require(reply["code"] == "duplicate_order" and "placed nothing new" in reply["details"],
                    f"bad reply: {reply}")
        else:
            require(status == 503 and refused(reply, "claim_unavailable"),
                    f"a repeat answered {status}: {reply}")
    return bool(created)


def repeat_until_answered(order, gateway=GATEWAY):
    """A repeat can only be `duplicate_order` once Valkey answers. Safe here, because it's a repeat."""
    deadline = time.time() + QUERY_WAIT
    while True:
        status, reply = call("POST", "/orders", TAKER, order, gateway)
        if status != 503 or reply["code"] != "claim_unavailable" or time.time() > deadline:
            return status, reply
        retries["claim_unavailable"] += 1
        time.sleep(0.1)


def check_double_post(game):
    print("Repeated POSTs", flush=True)
    maker_id, status, reply = place(MAKER, "sell", str(ROUNDS * 2))
    require(status == 201, f"the resting order answered {status}: {reply}")

    # Each of these fills at once, so the engine has forgotten the ID by the time a repeat lands.
    taker_ids, refused_ids = [], []
    for _ in range(ROUNDS):
        order = order_body("buy", "1")
        order_id = order["client_order_id"]
        placed = check_one_winner(race(order, TAKER), order_id)
        (taker_ids if placed else refused_ids).append(order_id)
    require(taker_ids, "every order was refused, so nothing was tested")
    passed(f"{ROUNDS} orders that fill at once, {RACERS} POSTs each across two gateways: "
           f"{len(taker_ids)} placed once, {len(refused_ids)} refused with nothing sent, none twice")

    # After the fill, when only the claim still knows the ID.
    wait_for(lambda: all(game.of(order_id, "fill") for order_id in taker_ids), WEBHOOK_WAIT,
             "the fill webhooks")
    for gateway in (GATEWAY, SECOND_GATEWAY):
        status, reply = repeat_until_answered(order_body("buy", "1", taker_ids[0]), gateway)
        require(status == 409 and reply["code"] == "duplicate_order",
                f"a repeat after the fill answered {status}: {reply}")

    maker = recorded(maker_id, MAKER, lambda order: order["filled_size"] == str(len(taker_ids)))
    fills = sum(len(game.of(order_id, "fill")) for order_id in taker_ids)
    require(fills == len(taker_ids), f"{len(taker_ids)} orders made {fills} fills")
    extra = [order_id for order_id in refused_ids if game.of(order_id)]
    require(not extra, f"refused orders reached the engine: {extra}")
    status, reply = call("DELETE", f"/orders/{maker_id}", MAKER)
    require(status == 200, f"cancel answered {status}: {reply}")
    passed(f"the resting order filled exactly {maker['filled_size']}, with one fill per order, and "
           "a repeat after the fill placed nothing")

    return taker_ids


def check_order_ids():
    print("Order IDs", flush=True)
    for order_id, code in ((uuid7(ORDER_ID_WINDOW + 5), "stale_client_order_id"),
                           (uuid7(-60), "future_client_order_id"),
                           (str(uuid.uuid4()), "invalid_client_order_id")):
        status, reply = call("POST", "/orders", TAKER, order_body("buy", "1", order_id))
        require(status == 400 and reply["code"] == code, f"{code} answered {status}: {reply}")
        require(reply["id"] == order_id, f"wrong ID: {reply}")
        passed(f"refused with {code}")

    body = order_body("buy", "1")
    del body["client_order_id"]
    status, reply = call("POST", "/orders", TAKER, body)
    require(status == 400, f"a missing ID answered {status}: {reply}")
    passed("a missing ID is refused")


def run_valkey(*args):
    runtime = sbx.container_runtime()
    subprocess.run([runtime, *args], check=True, capture_output=True, timeout=60)


def refused(reply, code):
    return "Nothing was sent" in reply.get("details", "") and reply.get("code") == code


def check_valkey_stopped():
    print("Valkey stopped", flush=True)
    resting_id, status, reply = place(MAKER, "sell", "1")
    require(status == 201, f"the resting order answered {status}: {reply}")

    run_valkey("stop", "sbx-valkey")
    try:
        order = order_body("buy", "1")
        wait_for(lambda: not claims_ready(), QUERY_WAIT, "/readyz to report the claim store")
        status, reply = call("POST", "/orders", TAKER, order)
        require(status == 503 and refused(reply, "claim_unavailable"),
                f"an order with Valkey down answered {status}: {reply}")
        passed("a new order was refused with claim_unavailable, and nothing was sent")

        ready = readiness(GATEWAY)
        require(ready["ready"] and not ready["checks"]["order_claims"], f"/readyz said {ready}")
        passed(f"/readyz stays in rotation and shows it: {ready['checks']}")

        status, reply = call("DELETE", f"/orders/{resting_id}", MAKER)
        require(status == 200 and reply["status"] == "canceled",
                f"a cancel with Valkey down answered {status}: {reply}")
        passed("a cancel still worked")
    finally:
        run_valkey("start", "sbx-valkey")

    # What Valkey kept depends on how it stopped, so either way this waits for a safe state.
    wait_for_claims()
    order_id, status, reply = place(TAKER, "buy", "1")
    require(status == 201, f"an order after Valkey returned answered {status}: {reply}")
    passed("new orders work once Valkey is back")
    status, reply = call("DELETE", f"/orders/{order_id}", TAKER)
    require(status == 200, f"cancel answered {status}: {reply}")


def check_valkey_emptied(second):
    print("Valkey lost its data", flush=True)
    claimed = order_body("sell", "1")
    claimed["client_order_id"], status, reply = place(MAKER, "sell", "1")
    require(status == 201, f"the order answered {status}: {reply}")

    run_valkey("exec", "sbx-valkey", "valkey-cli", "FLUSHALL")
    emptied_at = time.time()
    # A replay of the lost claim, at once, before any poll could have noticed.
    status, reply = call("POST", "/orders", MAKER, claimed)
    require(status == 503 and refused(reply, "claims_recovering"),
            f"a replay after the loss answered {status}: {reply}")
    passed("a replay right after the loss was refused with claims_recovering")

    status, reply = call("POST", "/orders", TAKER, order_body("buy", "1"), SECOND_GATEWAY)
    require(status == 503 and reply["code"] == "claims_recovering",
            f"the other gateway answered {status}: {reply}")

    second.stop()
    second.start()
    status, reply = call("POST", "/orders", TAKER, order_body("buy", "1"), SECOND_GATEWAY)
    require(status == 503 and reply["code"] == "claims_recovering",
            f"a gateway started during the hold answered {status}: {reply}")
    passed("every gateway refuses, including one that started during the hold")

    status, reply = call("DELETE", f"/orders/{claimed['client_order_id']}", MAKER)
    require(status == 200, f"a cancel during the hold answered {status}: {reply}")
    passed("a cancel worked during the hold")

    wait_for_claims()
    wait_for_claims(SECOND_GATEWAY)
    held = time.time() - emptied_at
    require(held > HOLD_WAIT - 25, f"the hold lasted only {held:.0f}s")
    status, reply = call("POST", "/orders", MAKER, claimed)
    require(status == 400 and reply["code"] == "stale_client_order_id",
            f"the replay after the hold answered {status}: {reply}")
    passed(f"after {held:.0f}s orders are open again, and by then the replay is stale")
    order_id, status, reply = place(TAKER, "buy", "1")
    require(status == 201, f"a new order after the hold answered {status}: {reply}")
    status, reply = call("DELETE", f"/orders/{order_id}", TAKER)
    require(status == 200, f"cancel answered {status}: {reply}")


def expect_status(order_id, wanted, gateway=GATEWAY):
    status, reply = call("GET", f"/orders/{order_id}", TAKER, gateway=gateway)
    require(status == 200, f"status answered {status}: {reply}")
    require(reply["status"] == wanted, f"expected {wanted}: {reply}")
    return reply


def start_status_checks():
    print("Order status", flush=True)
    unknown_id = uuid7()
    reply = expect_status(unknown_id, "unknown")
    require(reply["settled"] is False and "on hold" in reply["message"], f"bad reply: {reply}")
    expect_status(unknown_id, "unknown", SECOND_GATEWAY)
    passed("an unknown fresh ID answers unknown, not a 404, on both gateways")

    # Past the session timeout, but this could still be posted, so it may yet land.
    expect_status(uuid7(ORDER_ID_WINDOW - 20), "unknown")
    passed("an ID older than the session timeout that could still be posted answers unknown")
    return unknown_id, time.time()


def finish_status_checks(unknown_id, asked_at, placed_id, second):
    print("Order status, later", flush=True)
    time.sleep(max(0.0, asked_at + SETTLED_AFTER + 1 - time.time()))

    def answer():
        status, reply = call("GET", f"/orders/{unknown_id}", TAKER)
        require(status == 200, f"status answered {status}: {reply}")
        return reply["status"] == "never_landed" and reply
    # It stays unknown until the recorder has caught up with a node that was just killed.
    reply = wait_for(answer, FAILOVER_WAIT, "the unknown ID to answer never_landed")
    require(reply["settled"] is True, f"bad reply: {reply}")
    passed("the unknown ID later answers never_landed")

    second.stop()
    second.start()
    status, reply = call("GET", f"/orders/{placed_id}", TAKER, gateway=SECOND_GATEWAY)
    require(status == 200 and reply["status"] == "filled", f"status answered {status}: {reply}")
    passed("a gateway that just started, and never saw the order, reports it filled")


def check_market_data(nats, websocket):
    print("Market data", flush=True)
    wanted = [f"l2.{SYMBOL}", f"ticker.{SYMBOL}", f"candles.{SYMBOL}"]
    wait_for(lambda: all(nats.counts[subject] for subject in wanted), QUERY_WAIT,
             lambda: f"NATS messages on {wanted}, saw {dict(nats.counts)}")
    passed(f"NATS carried {dict(nats.counts)}")
    wait_for(lambda: websocket.counts["l2_data"] and websocket.counts["ticker"]
             and websocket.counts["candles"], QUERY_WAIT,
             lambda: f"websocket frames for {SYMBOL}, saw {dict(websocket.counts)}")
    passed(f"websocket delivered {dict(websocket.counts)}")


def stream_orders(orders, slowest):
    """Places crossing pairs, one after another, for as long as the failover takes."""
    for _ in range(PAIRS):
        for player, side in ((MAKER, "sell"), (TAKER, "buy")):
            started = time.time()
            order_id, status, reply = place(player, side, "1")
            slowest.append(time.time() - started)
            orders.append((order_id, player, status, reply))
        time.sleep(PAIR_GAP)


def settle(order_id, player):
    """How an order ended. A missing order is only final once the recorder has passed it."""
    status, order = call("GET", f"/orders/{order_id}", player)
    return order["status"] if status == 200 and "product_id" in order else None


def check_failover(game, nats, websocket, target):
    print("Failover", flush=True)
    start_position = max([event["tape_position"] for event in game.seen()], default=0)
    leader = sbx.cluster_leader()
    require(leader is not None, "the cluster has no leader")
    killed = leader if target is None else target

    orders, slowest = [], []
    stream = threading.Thread(target=stream_orders, args=(orders, slowest), daemon=True)
    stream.start()
    wait_for(lambda: len(orders) >= KILL_AFTER_PAIRS * 2, QUERY_WAIT, "orders to start flowing")
    sbx.cluster_script("kill", str(killed))
    killed_at = time.time()
    role = "leader" if killed == leader else "follower"
    passed(f"killed {role} node {killed} after {len(orders)} orders")

    try:
        stream.join(FAILOVER_WAIT * 2)
        require(not stream.is_alive(), f"orders stalled after the kill, {len(orders)} answered")
        passed(f"{len(orders)} orders answered, slowest took {max(slowest):.2f}s, new leader is "
               f"node {sbx.cluster_leader()}")

        order_id, status, reply = place(MAKER, "sell", "1")
        require(status == 201, f"a new order after the failover answered {status}: {reply}")
        orders.append((order_id, MAKER, status, reply))
        passed(f"a new order was accepted after the failover, {order_id}")
        check_exactly_once(game, orders, start_position)

        counts = nats.counts[f"l2.{SYMBOL}"], websocket.counts["l2_data"]
        check_trade(game)
        wait_for(lambda: nats.counts[f"l2.{SYMBOL}"] > counts[0]
                 and websocket.counts["l2_data"] > counts[1], QUERY_WAIT,
                 "market data to keep flowing after the failover")
        passed("market data kept flowing on NATS and the websocket")
    finally:
        restart_node(killed, killed_at)


def check_exactly_once(game, orders, start_position):
    unknown = [order for order in orders if order[2] != 201]
    allowed = [order for order in unknown if order[2] == 503 and order[3] and
               order[3].get("code") == "outcome_unknown"]
    require(len(unknown) == len(allowed), f"orders got an unexpected answer: {unknown}")

    clear_book()
    position = wait_for(lambda: outbox_position() if outbox_settled() else None, WEBHOOK_WAIT,
                        "the outbox to deliver everything")

    outcomes = collections.Counter()
    for order_id, player, status, _ in orders:
        # An accepted order always shows up. An unknown one that never landed won't.
        if status == 201:
            ended = wait_for(lambda: settle(order_id, player) not in (None, "open"), QUERY_WAIT,
                             f"order {order_id} to finish") and settle(order_id, player)
        else:
            ended = settle(order_id, player) or "never landed"
        outcomes[ended] += 1
        expected = {"filled": ["fill"], "canceled": ["canceled"]}.get(ended, [])
        wait_for(lambda: len(game.of(order_id)) >= len(expected), WEBHOOK_WAIT,
                 f"webhooks for order {order_id} ({ended})")
        kinds = [event["type"] for event in game.of(order_id)]
        require(kinds == expected, f"order {order_id} ended {ended}, webhooks: {kinds}")

    run = [event for event in game.seen() if event["tape_position"] > start_position]
    positions = [event["tape_position"] for event in run]
    require(positions == sorted(positions), "webhooks arrived out of tape order")
    duplicates = [fill_id for fill_id, count in
                  collections.Counter(event["fill_id"] for event in game.seen()).items() if count > 1]
    require(not duplicates, f"webhooks were delivered twice: {duplicates}")
    require(game.bad_signatures == 0, "a webhook failed its signature check")
    require(position["halted"] is None, f"the outbox halted: {position}")
    passed(f"{len(orders)} orders ended {dict(outcomes)}, {len(run)} webhooks, none missing, "
           "none repeated")


def outbox_position():
    address = sbx.outbox_settings()["OUTBOX_HTTP"]
    with urllib.request.urlopen(f"http://{address}/position", timeout=5) as response:
        return json.loads(response.read())


def outbox_settled():
    position = outbox_position()
    return position["delivered_position"] == position["durable_position"]


def member_positions():
    """Each node's log position as the leader sees it."""
    leader = sbx.cluster_leader()
    if leader is None:
        return {}
    result = subprocess.run(
        ["java", "--add-opens", "java.base/jdk.internal.misc=ALL-UNNAMED",
         f"-Daeron.dir=/Volumes/DevShm/cluster-{leader}", "-cp", JAR,
         "io.aeron.cluster.ClusterTool", str(sbx.CLUSTER_RUN / str(leader) / "cluster"),
         "list-members"], capture_output=True, text=True)
    members = re.findall(r"ClusterMember\{id=(\d+).*?logPosition=(-?\d+)", result.stdout)
    return {int(node): int(position) for node, position in members}


def restart_node(node, killed_at):
    time.sleep(max(0.0, killed_at + sbx.NODE_RESTART_DELAY - time.time()))
    sbx.cluster_script("start", str(node))

    def caught_up():
        positions = member_positions()
        return len(positions) == sbx.CLUSTER_NODES and len(set(positions.values())) == 1 \
            and positions[node] > 0 and positions
    positions = wait_for(caught_up, REJOIN_WAIT, f"node {node} to catch up with the leader")
    passed(f"node {node} restarted and caught up, log positions {positions}")


def preflight():
    missing = [name for name, *_ in sbx.cluster_services() if sbx.pid_of(name) is None]
    require(not missing, f"not running: {missing}. Start with `tools/scripts/sbx.py up --cluster`.")
    down = [node for node in sbx.cluster_script("status").splitlines() if "False" in node]
    require(not down, f"nodes are down: {down}")


def run(target):
    preflight()
    settings = sbx.outbox_settings()
    game = Game(settings["WEBHOOK_URL"], settings["WEBHOOK_SECRET"])
    threading.Thread(target=game.serve_forever, daemon=True).start()
    nats = NatsWatch([f"l2.{SYMBOL}", f"ticker.{SYMBOL}", f"candles.{SYMBOL}"])
    websocket = WebSocketWatch(sbx.WS_ADDR, SYMBOL)
    nats.start()
    websocket.start()

    wait_for_claims()
    clear_book()
    check_trade(game)
    check_market_data(nats, websocket)

    second = SecondGateway()
    second.start()
    try:
        wait_for_claims(SECOND_GATEWAY)
        unknown_id, asked_at = start_status_checks()
        taker_ids = check_double_post(game)
        check_order_ids()
        check_valkey_stopped()
        check_valkey_emptied(second)
        check_failover(game, nats, websocket, target)
        finish_status_checks(unknown_id, asked_at, taker_ids[0], second)
    finally:
        second.stop()

    position = outbox_position()
    require(position["verified"] and position["halted"] is None, f"outbox unhealthy: {position}")
    passed(f"outbox healthy at {position['durable_position']}, "
           f"{len(game.seen())} events received in total, "
           f"{retries['recorder_behind']} recorder_behind and "
           f"{retries['claim_unavailable']} claim_unavailable retries")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--kill", type=int, choices=range(sbx.CLUSTER_NODES),
                        help="the node to kill, by default the leader")
    try:
        run(parser.parse_args().kill)
    except Failed as failure:
        print(f"FAIL: {failure}", flush=True)
        return 1
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"FAIL: {error}", flush=True)
        return 1
    print("PASS: a real trade reached the game webhook, and survived losing a node", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
