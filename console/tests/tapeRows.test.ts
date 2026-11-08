import assert from "node:assert/strict";
import test from "node:test";
import type { TapeRow } from "../src/lib/api.ts";
import { mergeTapeRows, rowKey } from "../src/lib/tapeRows.ts";

const row = (
  id: number,
  kind: "order" | "trade" = "order",
  time = "2026-11-07T00:00:00Z",
): TapeRow => ({
  kind,
  exec_id: id,
  time,
  symbol: "IRON",
  side: "buy",
  order_id: 1,
});

test("replayed connection tails and repeated rows within a batch stay unique", () => {
  const first = mergeTapeRows([row(1), row(2), row(2)], [], 2000);
  const reconnected = mergeTapeRows([row(2), row(1), row(3)], first, 2000);
  assert.deepEqual(reconnected.map(rowKey).sort(), ["order-1", "order-2", "order-3"]);
  assert.deepEqual(mergeTapeRows([row(1), row(2), row(3)], reconnected, 2000), reconnected);
});

test("distinct reports on the same order and order/trade views survive", () => {
  const rows = mergeTapeRows([row(1), row(2), row(2, "trade")], [], 2000);
  assert.equal(rows.length, 3);
  assert.equal(new Set(rows.map(rowKey)).size, 3);
});

test("newest unique rows are retained across out-of-order batches and eviction", () => {
  const rows = mergeTapeRows(
    [row(1, "order", "2026-11-07T00:00:01Z"), row(3, "order", "2026-11-07T00:00:03Z")],
    [row(2, "order", "2026-11-07T00:00:02Z"), row(1, "order", "2026-11-07T00:00:01Z")],
    2,
  );
  assert.deepEqual(rows.map(rowKey), ["order-3", "order-2"]);
});
