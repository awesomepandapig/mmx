#!/usr/bin/env python3
"""A tiny game-server client for an MMX gateway, using only the standard library.

    tools/demo/mmx_client.py --symbol IRON trade           rest a sell, cross it, and cancel a third
    tools/demo/mmx_client.py --symbol IRON place buy 5 --price 10
    tools/demo/mmx_client.py --symbol IRON place sell 5    a market order
    tools/demo/mmx_client.py cancel ORDER_ID
    tools/demo/mmx_client.py get ORDER_ID
    tools/demo/mmx_client.py list

A real game server does what this does: it names the player in `x-player-id`, gives every order a
new UUIDv7 `client_order_id`, and sends it once. It never retries a place: if the answer is lost, the
order may still have landed, so it asks for the order by ID instead.

The gateway trusts `x-player-id`, so only game servers may reach it. Set GATEWAY_URL, or pass
--gateway, to use something other than http://localhost:8000.
"""

import argparse
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request
import uuid

# Fixed, so that a rerun acts as the same two players.
SELLER = "11111111-1111-4111-8111-111111111111"
BUYER = "22222222-2222-4222-8222-222222222222"
TIMEOUT_SECS = 10


def uuid7():
    """A UUIDv7: a millisecond timestamp, then random bits, as RFC 9562 lays out."""
    value = (time.time_ns() // 1_000_000) << 80 | secrets.randbits(80)
    value = value & ~(0xF << 76) | 0x7 << 76
    value = value & ~(0x3 << 62) | 0x2 << 62
    return uuid.UUID(int=value)


class Unknown(Exception):
    """The gateway gave no clear answer, so the order may or may not have landed."""


def call(gateway, player, method, path, body=None):
    """Sends once, and returns the status and parsed reply."""
    request = urllib.request.Request(
        f"{gateway}{path}",
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"x-player-id": str(player), "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECS) as response:
            return response.status, json.loads(response.read() or b"null")
    except urllib.error.HTTPError as error:
        text = error.read().decode(errors="replace")
        try:
            return error.code, json.loads(text)
        except ValueError:
            return error.code, text
    except (urllib.error.URLError, TimeoutError) as error:
        raise Unknown(str(error)) from error


def show(label, status, reply):
    print(f"{label}: HTTP {status}")
    print(json.dumps(reply, indent=2))


def place(gateway, player, symbol, side, size, price=None):
    """Places one order and returns its reply, or None if it was refused or its outcome is unknown."""
    order_id = str(uuid7())
    order = {
        "client_order_id": order_id,
        "symbol": symbol,
        "side": side,
        "type": "market" if price is None else "limit",
        "size": size,
    }
    if price is not None:
        order["price"] = price
    what = f"{side} {size} {symbol} @ {price or 'market'}"
    try:
        status, reply = call(gateway, player, "POST", "/api/v1/orders", order)
    except Unknown as error:
        print(f"{what}: outcome unknown ({error}). Don't resend. Ask for {order_id}.")
        return None
    show(what, status, reply)
    if status == 503:
        print(f"The outcome is unknown. Don't resend. Ask for {order_id}.")
    return reply if 200 <= status < 300 else None


def trade(gateway, symbol, price, size):
    """A resting sell, a buy that crosses it, and a resting order that's canceled."""
    resting = place(gateway, SELLER, symbol, "sell", size, price)
    if resting is None:
        return 1
    crossing = place(gateway, BUYER, symbol, "buy", size, price)
    if crossing is None:
        return 1
    below = format(float(price) / 2, "f").rstrip("0").rstrip(".")
    leftover = place(gateway, BUYER, symbol, "buy", size, below)
    if leftover is None:
        return 1
    status, reply = call(gateway, BUYER, "DELETE", f"/api/v1/orders/{leftover['id']}")
    show(f"cancel {leftover['id']}", status, reply)
    return 0 if 200 <= status < 300 else 1


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--gateway", default=os.environ.get("GATEWAY_URL", "http://localhost:8000")
    )
    parser.add_argument("--player", default=SELLER, help="a UUID, for place, cancel, get and list")
    parser.add_argument("--symbol", default="ITEM")
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("trade", help="make a trade between two players")
    run.add_argument("--price", default="10")
    run.add_argument("--size", default="5")

    order = commands.add_parser("place", help="place one order")
    order.add_argument("side", choices=["buy", "sell"])
    order.add_argument("size")
    order.add_argument("--price", help="leave out for a market order")

    commands.add_parser("cancel", help="cancel an order").add_argument("id")
    commands.add_parser("get", help="show an order").add_argument("id")
    commands.add_parser("list", help="list the player's orders")

    args = parser.parse_args()
    gateway = args.gateway.rstrip("/")
    try:
        if args.command == "trade":
            return trade(gateway, args.symbol, args.price, args.size)
        if args.command == "place":
            placed = place(gateway, args.player, args.symbol, args.side, args.size, args.price)
            return 0 if placed else 1
        if args.command == "cancel":
            status, reply = call(gateway, args.player, "DELETE", f"/api/v1/orders/{args.id}")
        elif args.command == "get":
            status, reply = call(gateway, args.player, "GET", f"/api/v1/orders/{args.id}")
        else:
            status, reply = call(gateway, args.player, "GET", "/api/v1/orders")
    except Unknown as error:
        sys.exit(f"can't reach the gateway at {gateway}: {error}")
    show(args.command, status, reply)
    return 0 if 200 <= status < 300 else 1


if __name__ == "__main__":
    sys.exit(main())
