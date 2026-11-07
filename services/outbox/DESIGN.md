# Outbox design

The outbox reads the tape, makes each frame durable on its own disk, and then tells the game. This
note records the guarantees, the contract with the engine, and the replication plan.

## How it works

1. One thread follows the report recording of any node, starting one frame before the journal's end.
2. Every frame, including new-order reports and term markers, goes into `journal.log`. Frames share a
   flush: 64 KiB or 1 ms, whichever comes first.
3. Only after the flush does the journal's durable length advance. That length is the sender's read
   limit, so an unflushed frame can't be sent.
4. One sender POSTs fills, cancels and rejects in tape order. A batch is retried until the game
   answers 2xx.
5. The `delivered` cursor records progress after each accepted batch.

A crash can only leave the cursor behind the truth. That repeats events, and it can't skip them.

## Why this can't cause a double spend

- **The game never hears about something undurable.** See step 3 above.
- **A repeat is harmless.** `fill_id` is a hash of the frame and its tape position, so it's the same on
  every retry, restart, replica and rebuild.
- **A reused ID can't hide an event.** If a total loss ever reuses an `exec_id`, the new frame hashes
  differently, so a game won't drop it as a repeat.
- **A repeat order can't release live escrow.** The engine checks for a full book before it checks for
  duplicates, so a repeat can be rejected with reason `other`. The outbox tracks working orders and marks
  any reject for a working `client_order_id` under another `order_id` as `terminal: false, release_qty:
  "0"`.
- **The outbox won't paper over a broken tape.** Each restart must read the journal's last frame back
  from the tape, byte for byte, before anything is sent. A mismatch, a skipped frame or a missing
  `exec_id` halts delivery, and it stays halted across restarts until the tape agrees.

Halting can delay escrow, but it can't double spend. It trades availability for safety on purpose.

## Webhook contract

```
POST <WEBHOOK_URL>
Content-Type: application/json
X-MMX-Timestamp: 1767225600
X-MMX-Signature: v1=<lowercase hex>

{"events":[{ ... }]}
```

The signature is `HMAC-SHA256(WEBHOOK_SECRET, "<timestamp>.<raw body>")`. Verify it over the raw bytes
before parsing, using a constant-time compare. The timestamp is fresh on every attempt, so a game can
refuse anything older than about five minutes. The body doesn't change between attempts of one batch,
but batch boundaries can change after a restart.

| Field | Present on | Meaning |
| --- | --- | --- |
| `fill_id` | all | Dedupe key, on every event type: 32 hex characters |
| `type` | all | `fill`, `canceled`, `rejected` or `cancel_rejected` |
| `tape_position`, `exec_id` | all | Where the event is on the tape; both increase |
| `terminal` | all | This event closes the escrow held for `client_order_id` |
| `release_qty` | all | Quantity to release from escrow, as a decimal string |
| `client_order_id` | all | UUID the game sent; a cancel request's own ID on `cancel_rejected` |
| `orig_client_order_id` | `cancel_rejected` | The order that the cancel named |
| `order_id` | not `cancel_rejected` | Engine order ID |
| `account`, `symbol`, `side`, `status` | not `cancel_rejected` | The order |
| `order_qty`, `filled_qty`, `leaves_qty` | not `cancel_rejected` | Decimal strings |
| `price` | limit orders | Decimal string, omitted on market orders |
| `match_id`, `last_qty`, `last_px`, `avg_px` | `fill` | Both legs share `match_id`, not `fill_id` |
| `reason` | `rejected`, `cancel_rejected` | `duplicate_order`, `unknown_order`, `stale_order` or `other` |
| `transact_time` | not `cancel_rejected` | Nanoseconds since the epoch, as a string |

What the game must do, in one database transaction per event:

1. Insert `fill_id` into a table with a unique key. If it's already there, skip the event.
2. Apply `last_qty` at `last_px` on a `fill`.
3. Release `release_qty`. Act on `terminal` and `release_qty` only, never on `type` alone.
4. If `terminal` is true, close the escrow for `client_order_id`. Ignore any later terminal event for an
   escrow that's already closed.

Answer 2xx only after the transaction commits. Anything else, including a redirect, is retried forever
with backoff from 100 ms up to 30 s, and later events wait behind it. So a game bug that refuses one
event stalls the queue rather than losing it; watch `consecutive_failures` on `/position`.

Keep `fill_id`s for as long as the tape is kept. A rebuilt outbox sends history again, and only those
IDs make that harmless.

## Durable position

`GET /position` on `OUTBOX_HTTP` (default port 8092) returns:

```json
{
  "durable_position": 192000,
  "durable_exec_id": 812,
  "delivered_position": 190080,
  "verified": true,
  "halted": null,
  "consecutive_failures": 0
}
```

`durable_position` is the end of the last flushed frame, in the report stream's position space. It's
the upper bound on what the game may have heard. The endpoint starts after journal recovery and doesn't
need the tape, so it answers even with every engine node down.

The startup check, for section 5:

- Run it after log replay finishes, and compare against the furthest report recording of any member.
  A single node that's still rebuilding can be briefly behind, which is normal.
- `durable_position` beyond that end, or `durable_exec_id` beyond the engine's counter, is a total
  loss: refuse to resume.
- If the outbox can't be reached, the answer is unknown, so refuse there too. An operator can override.
- A position of 0 proves nothing. It may be a new install or a lost disk.

Comparing positions alone isn't enough to rebuild safely. Frames between the tape's end and
`durable_position` may have reached the game, including cancels that released escrow, so a book rebuilt
without them could fill those orders again. `GET /journal?from=<tape end>&limit=<n>` returns exactly those
frames, in the journal format described in `src/journal.rs`, and the book rebuild has to apply them.

Exports are paginated: `limit` must be between 1 and 10,000 (default 10,000), and each response holds
at most 1 MiB of complete records. Continue with `from` equal to the last returned record's position;
a short page does not prove the durable journal has ended. Invalid/repeated arguments return 400.
Two exports may be assembled concurrently, within a 16-connection server limit; excess exports
return 503, and excess connections close. Socket reads/writes have timeouts. Export scans still start
at the journal's beginning, so large-history seek performance remains an optimization to measure.

Webhook batches default to 100 events and must contain between 1 and 10,000. Malformed explicit
numeric settings refuse startup. Delivery honors valid `Retry-After` seconds or HTTP-date hints on
non-success responses, including 429 and 503; the receiver's wait is not shortened by the local
30-second exponential-backoff cap. Waiting can be interrupted by stop or a safety halt, and no failed
attempt advances the delivery cursor. Event bodies/IDs remain identical across attempts. Sender
panics or unexpected termination stop the process rather than leave a ready process without delivery.
This does not implement poison-batch pausing, rate limiting, replication, or corruption repair.
The supported header forms follow [RFC 9110, section 10.2.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-10.2.3).

Both endpoints are unauthenticated and `/journal` exposes trades, so keep the port inside the cluster.

## Replication

The goal is that one lost disk can't block webhooks, or lose the high-water mark that the total-loss
check depends on. Today, losing the outbox disk means rebuilding from the tape, which works but sends
history again and leaves the check blind until it finishes.

| Option | Survives a disk loss | Blocks webhooks? | Cost |
| --- | --- | --- | --- |
| A. Rebuild from the tape | Yes, slowly | Until rebuilt | None, and it works today |
| B. Replicated volume (network block storage) | Yes | Only during reattach | Depends on the operator's storage; adds fsync latency |
| C. Independent replicas, one sender | Yes | About one lease timeout | One more pod and disk; needs a lease |
| D. Active-active senders | Yes | No | The game sees every event at least twice, and order across senders is lost |
| E. Quorum journal (Raft) | Yes | No | A second consensus system in the outbox |

**Recommendation: C, with A as the fallback.**

Run two or three outboxes, each with its own disk, each following the tape from its own journal. They
need no replication protocol, because the tape is already the shared log and `fill_id` is deterministic.
A Kubernetes `Lease` picks the one sender. Standbys journal and flush as usual, but don't send.

- **Failover:** a standby takes the lease and starts from its own cursor, or from the position published
  by the old sender if it's further on. Repeats carry the same IDs, so a stale cursor is harmless. Two
  senders during a lease overlap are harmless too, for the same reason.
- **Keeping the high-water mark:** a sender may only send what a standby has also flushed, while a
  standby is healthy, so the position survives one disk. It reads the standby's `/position` to learn
  this. If the standby is gone for longer than a timeout, the sender carries on alone and reports that
  it's degraded, because webhooks mustn't block on one failed disk.
- **Total-loss check:** the engine takes the maximum across all replicas, and needs a majority to answer.
- **Rebuilds:** a replacement replica needs `delivered_position` from a peer before it may send, so it
  doesn't send history again.

B is a reasonable choice for operators who already have good replicated storage, and needs no code. It
doesn't help if the storage itself fails, so it shouldn't be the only answer in the Helm chart. D is
ruled out because tape order is what lets a game trust that fills arrive before the event closing an
order. E solves a problem the tape has already solved.

## If the tape becomes disk-synced

A tape synced to disk would make the journal redundant, and this service could shrink to a webhook
sender with a saved position. The file-sync layer is kept separate for that reason:

- **Keep:** `tape.rs`, `frame.rs`, `webhook.rs`, the POST, retry and signing code in `delivery.rs`, and
  the cursor.
- **Remove:** `journal.rs`, the flush batching in `outbox.rs`, and the `/journal` endpoint.
- **Change:** `Deliverer::next_batch` would read frames from the tape up to the tape's durable position,
  in place of the journal up to its synced length. `webhook::event` only needs the position, frame, exec
  ID and flags, which is `journal::Record` without the file format.
- **Decide:** `Ledger` is rebuilt from the journal on startup. Without one, it needs a replay of the tape
  from the start, or a checkpoint. It can't be dropped while the engine checks for a full book before
  duplicates, because it's what stops a repeat order from releasing live escrow.

The durable position would then be the cursor, which is a lower bound on what the game has heard rather
than an upper bound, so the engine's total-loss check would have to change too. A synced tape would also
make the check much less likely to matter, and that's the case for making the change. The replication
plan above would need rewriting, because the tape's own replicas would cover a disk loss.

## Known limits

- The journal is one file with no retention. It costs about 160 bytes per report, and startup reads it
  all to rebuild the working order set. Pruning needs a checkpoint of that set first.
- After a total loss, the tape can grow past the journal with different content, and the outbox then
  stays halted. Clearing that needs the admin recovery path from section 5, plus a way to accept the new
  tape without resending history.
- If tape retention ever drops the start of the recording, a rebuild from position 0 can't work without
  a checkpoint.
