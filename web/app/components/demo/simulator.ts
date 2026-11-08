/**
 * An in-browser stand-in for an MMX deployment: order books, an outbox and
 * webhook retries, with bots trading alongside the player. Nothing here touches the
 * network, so the demo needs no server. Prices are integer hundredths of a gold.
 */

export type Side = "buy" | "sell";
export type OrderType = "limit" | "market";
export type Service = "api" | "auth" | "engine" | "outbox" | "webhook" | "game";
export type PayloadTab = "request" | "response" | "webhook" | "ws";

export interface Market {
  sym: string;
  name: string;
  tick: number;
  last: number;
  bids: RestingOrder[];
  asks: RestingOrder[];
  hist: { t: number; p: number }[];
}

export interface RestingOrder {
  id: string;
  sym: string;
  side: Side;
  price: number;
  rem: number;
  size: number;
  mine: boolean;
}

export interface Level {
  price: number;
  qty: number;
  mine: boolean;
}

export interface LogEntry {
  id: number;
  at: number;
  service: Service;
  message: string;
  result: string;
  tone?: "good" | "bad";
  mine: boolean;
}

export interface Payload {
  method?: string;
  path?: string;
  attempt?: number;
  status?: number;
  headers?: Record<string, string>;
  body?: unknown;
}

export interface OrderInput {
  sym: string;
  side: Side;
  type: OrderType;
  qty: number;
  price: number | null;
}

interface Fill {
  id: string;
  orderId: string;
  sym: string;
  side: Side;
  qty: number;
  price: number;
  holdPrice: number;
}

interface Match {
  maker: RestingOrder;
  qty: number;
  price: number;
}

const ACCOUNT = "acct_7f31";
const SERVICE_TOKEN = "Bearer $SERVICE_TOKEN";
const WEBHOOK_PATH = "/webhooks/mmx";
const HISTORY = 180;
const MAX_LOG = 80;
const SEED_TICKS = 34;
const BOT_MS = 420;
const HOP_MS = 150;
const RETRY_MS = 3000;
const NOTICE_MS = 4000;

const STARTING_MARKETS = [
  { sym: "COAL", name: "Coal", last: 415, tick: 5 },
  { sym: "IRON", name: "Iron ore", last: 950, tick: 5 },
  { sym: "STEEL", name: "Steel bar", last: 3140, tick: 10 },
  { sym: "OAK", name: "Oak log", last: 210, tick: 5 },
  { sym: "LOBSTR", name: "Lobster", last: 695, tick: 5 },
];

export const money = (cents: number) => (cents / 100).toFixed(2);
export const whole = (cents: number) =>
  Math.floor(cents / 100).toLocaleString("en-US");

// Seeded, so the server and the first client render draw the same book.
function createRandom(seed: number) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export class Simulator {
  markets: Market[];
  selected = "IRON";
  gold = 240000;
  heldGold = 0;
  items: Record<string, { free: number; held: number }> = {};
  open: RestingOrder[] = [];
  gameOnline = true;
  outbox = 0;
  log: LogEntry[] = [];
  payloads: Record<PayloadTab, Payload | null> = {
    request: null,
    response: null,
    webhook: null,
    ws: null,
  };
  notice: { text: string; bad: boolean } | null = null;
  version = 0;

  private random = createRandom(88416);
  private listeners = new Set<() => void>();
  private timers = new Set<ReturnType<typeof setTimeout>>();
  private noticeTimer?: ReturnType<typeof setTimeout>;
  private filled: Record<string, number> = {};
  private orderSeq = 88416;
  private fillSeq = 0;
  private logSeq = 0;
  private depth = 13;
  private hop = HOP_MS;
  private seeded = false;
  private clock: number | null = null;

  constructor() {
    this.markets = STARTING_MARKETS.map((m) => ({
      ...m,
      bids: [],
      asks: [],
      hist: [],
    }));
    for (const m of this.markets) {
      this.items[m.sym] = { free: m.sym === "IRON" ? 120 : 0, held: 0 };
      this.seedMarket(m);
    }
  }

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /** Runs the bots until the returned function is called. */
  start({ reducedMotion = false } = {}) {
    this.hop = reducedMotion ? 0 : HOP_MS;
    if (!this.seeded) {
      this.seeded = true;
      const now = Date.now();
      for (let i = 0; i < SEED_TICKS; i++) {
        this.clock = now - (SEED_TICKS - i) * BOT_MS;
        this.botTick();
      }
      this.clock = null;
    }
    const bots = setInterval(
      () => {
        this.botTick();
        this.publish();
      },
      reducedMotion ? BOT_MS * 3 : BOT_MS,
    );
    const tape = setInterval(
      () => this.markets.forEach((m) => this.recordPrice(m)),
      1000,
    );
    this.publish();
    return () => {
      clearInterval(bots);
      clearInterval(tape);
      this.timers.forEach(clearTimeout);
      this.timers.clear();
    };
  }

  market(sym = this.selected) {
    return this.markets.find((m) => m.sym === sym) as Market;
  }

  select(sym: string) {
    this.selected = sym;
    this.publish();
  }

  setDepth(levels: number) {
    if (levels === this.depth) return;
    this.depth = levels;
    this.publish();
  }

  setGameOnline(online: boolean) {
    this.gameOnline = online;
    this.publish();
  }

  levels(sym: string, side: Side, count: number) {
    const m = this.market(sym);
    const out: Level[] = [];
    for (const o of side === "buy" ? m.bids : m.asks) {
      const top = out[out.length - 1];
      if (top?.price === o.price) {
        top.qty += o.rem;
        top.mine = top.mine || o.mine;
      } else {
        if (out.length === count) break;
        out.push({ price: o.price, qty: o.rem, mine: o.mine });
      }
    }
    return out;
  }

  /** Gold (or items' gold value) that crossing the book would use, and what would be left unfilled. */
  quote(sym: string, side: Side, qty: number, limit: number | null) {
    const m = this.market(sym);
    let cost = 0;
    let left = qty;
    for (const o of side === "buy" ? m.asks : m.bids) {
      if (!left || !crosses(side, o.price, limit)) break;
      const q = Math.min(left, o.rem);
      cost += q * o.price;
      left -= q;
    }
    return { cost, left };
  }

  placeOrder(order: OrderInput) {
    const m = this.market(order.sym);
    const { side, type, qty, price } = order;
    const limitPrice = type === "limit" ? price : null;
    const summary = `${side} ${qty} ${m.sym}${type === "limit" ? ` @ ${money(price ?? 0)}` : " market"}`;
    const body = {
      client_order_id: crypto.randomUUID(),
      symbol: m.sym,
      side,
      type,
      size: String(qty),
      ...(type === "limit" ? { price: decimal(price ?? 0) } : {}),
    };

    const problem = this.validate(m, order);
    if (problem) {
      this.capture("POST", "/api/v1/orders", body, problem.status, {
        error: problem.code,
        message: problem.text,
      });
      this.write(
        "api",
        `POST /api/v1/orders ${summary}`,
        String(problem.status),
        { mine: true, tone: "bad" },
      );
      this.showNotice(problem.text, true);
      this.publish();
      return;
    }

    const id = this.nextOrderId();
    const { fills, rem } = this.match(m, side, qty, limitPrice);
    const spent = fills.reduce((total, f) => total + f.qty * f.price, 0);
    const rests = limitPrice !== null && rem > 0;
    const holdGold = spent + (rests ? rem * limitPrice : 0);
    const holdQty = qty - rem + (rests ? rem : 0);
    if (side === "buy") {
      this.gold -= holdGold;
      this.heldGold += holdGold;
    } else {
      this.items[m.sym].free -= holdQty;
      this.items[m.sym].held += holdQty;
    }

    const resting: RestingOrder | null = rests
      ? { id, sym: m.sym, side, price: limitPrice, rem, size: qty, mine: true }
      : null;
    if (resting) {
      this.insert(m, resting);
      this.open.push(resting);
    }

    const status = rests
      ? fills.length
        ? "partially_filled"
        : "open"
      : "filled";
    this.capture("POST", "/api/v1/orders", body, 201, {
      order_id: id,
      status,
      filled_size: String(qty - rem),
      created_at: new Date().toISOString(),
    });
    this.captureBookUpdate(m, fills, resting);

    const steps = [
      () =>
        this.write(
          "game",
          side === "buy"
            ? `hold ${money(holdGold)} gold`
            : `hold ${holdQty} ${m.sym}`,
          "",
          { mine: true },
        ),
      () =>
        this.write("api", `POST /api/v1/orders ${summary}`, "201", {
          mine: true,
          tone: "good",
        }),
      () =>
        this.write("auth", `player ${ACCOUNT} named by game server`, "", {
          mine: true,
        }),
    ];
    if (!fills.length)
      steps.push(() =>
        this.write("engine", `rest ${id} at ${money(price ?? 0)}`, "open", {
          mine: true,
        }),
      );
    for (const f of fills) {
      const fill = this.createFill(m, id, side, f.qty, f.price, f.price);
      steps.push(() =>
        this.write(
          "engine",
          `match ${id}, ${fill.id}`,
          `${f.qty} @ ${money(f.price)}`,
          { mine: true },
        ),
      );
      steps.push(() => this.queueFill(fill));
      // The player crossed their own resting order, so both sides settle.
      if (f.maker.mine) {
        const own = this.makerFill(m, f);
        steps.push(() => this.queueFill(own));
      }
    }
    this.chain(steps);
    if (fills.length) this.recordPrice(m);
    this.publish();
  }

  cancelOrder(id: string) {
    const o = this.open.find((x) => x.id === id);
    if (!o) return;
    const m = this.market(o.sym);
    const book = o.side === "buy" ? m.bids : m.asks;
    book.splice(book.indexOf(o), 1);
    this.open = this.open.filter((x) => x !== o);
    if (o.side === "buy") {
      this.heldGold -= o.rem * o.price;
      this.gold += o.rem * o.price;
    } else {
      this.items[o.sym].held -= o.rem;
      this.items[o.sym].free += o.rem;
    }
    this.capture("DELETE", `/api/v1/orders/${o.id}`, undefined, 200, {
      order_id: o.id,
      status: "cancelled",
    });
    this.chain([
      () =>
        this.write("api", `DELETE /api/v1/orders/${o.id}`, "200", {
          mine: true,
          tone: "good",
        }),
      () => this.write("engine", `cancel ${o.id}`, "removed", { mine: true }),
      () =>
        this.write(
          "game",
          o.side === "buy"
            ? `release ${money(o.rem * o.price)} gold`
            : `release ${o.rem} ${o.sym}`,
          "",
          { mine: true },
        ),
    ]);
    this.publish();
  }

  private validate(m: Market, { side, type, qty, price }: OrderInput) {
    const invalid = (text: string) => ({
      status: 400,
      code: "invalid_order",
      text,
    });
    const refused = (code: string, text: string) => ({
      status: 422,
      code,
      text,
    });
    if (!Number.isInteger(qty) || qty < 1) return invalid("Enter a quantity");
    if (type === "limit") {
      if (!price || price < 0) return invalid("Enter a price");
      if (price % m.tick)
        return invalid(`Price must be in steps of ${money(m.tick)}`);
    }
    const limit = type === "limit" ? price : null;
    if (side === "buy") {
      const { cost, left } = this.quote(m.sym, side, qty, limit);
      if (cost + (limit === null ? 0 : left * limit) > this.gold)
        return refused("insufficient_gold", "Not enough gold");
    } else if (qty > this.items[m.sym].free) {
      return refused(
        "insufficient_items",
        `Not enough ${m.name.toLowerCase()}`,
      );
    }
    return null;
  }

  private seedMarket(m: Market) {
    let p = m.last;
    const now = Date.now();
    for (let i = 0; i < HISTORY; i++) {
      if (i % 5 === 0) p += this.rand(-1, 1) * m.tick;
      m.hist.push({ t: now - (HISTORY - i) * 1000, p });
    }
    m.last = p;
    for (let i = 0; i < 12; i++) {
      this.insert(
        m,
        this.botOrder(m, "buy", p - m.tick * (i + 1), this.rand(20, 160)),
      );
      this.insert(
        m,
        this.botOrder(m, "sell", p + m.tick * (i + 1), this.rand(20, 160)),
      );
    }
    this.fillDepth(m);
  }

  private botOrder(
    m: Market,
    side: Side,
    price: number,
    size: number,
  ): RestingOrder {
    return {
      id: this.nextOrderId(),
      sym: m.sym,
      side,
      price,
      rem: size,
      size,
      mine: false,
    };
  }

  private botTick() {
    const m = this.random() < 0.6 ? this.market() : this.pick(this.markets);
    const bid = m.bids[0].price;
    const ask = m.asks[0].price;
    const roll = this.random();
    if (roll < 0.55) {
      const side = this.pick<Side>(["buy", "sell"]);
      const improve = ask - bid > m.tick ? m.tick : 0;
      const away = m.tick * this.rand(0, 5);
      const price =
        side === "buy" ? bid - away + improve : ask + away - improve;
      const order = this.botOrder(m, side, price, this.rand(5, 90));
      this.insert(m, order);
      this.write(
        "api",
        `POST /api/v1/orders ${side} ${order.size} ${m.sym} @ ${money(price)}`,
        "201",
      );
    } else if (roll < 0.8) {
      const side = this.pick<Side>(["buy", "sell"]);
      const size = this.rand(5, 45);
      const { fills } = this.match(m, side, size, null);
      this.write(
        "api",
        `POST /api/v1/orders ${side} ${size} ${m.sym} market`,
        "201",
      );
      for (const f of fills) {
        if (f.maker.mine) {
          const fill = this.makerFill(m, f);
          this.write(
            "engine",
            `match ${fill.orderId}, ${fill.id}`,
            `${f.qty} @ ${money(f.price)}`,
            { mine: true },
          );
          this.queueFill(fill);
        } else {
          this.write(
            "engine",
            `match ${f.maker.id}`,
            `${f.qty} @ ${money(f.price)}`,
          );
        }
      }
      this.recordPrice(m);
    } else {
      const book = this.pick([m.bids, m.asks]);
      const candidates = book.slice(6).filter((o) => !o.mine);
      if (candidates.length) {
        const o = this.pick(candidates);
        book.splice(book.indexOf(o), 1);
        this.write("api", `DELETE /api/v1/orders/${o.id}`, "200");
      }
    }
    this.fillDepth(m);
  }

  // Keeps enough resting levels that the ladder always fills its space.
  private fillDepth(m: Market) {
    const refill = (side: Side, book: RestingOrder[], direction: number) => {
      while (this.levels(m.sym, side, this.depth).length < this.depth) {
        const worst = book[book.length - 1]?.price ?? m.last;
        const price = worst + direction * m.tick * this.rand(1, 2);
        if (price <= 0) return;
        book.push(this.botOrder(m, side, price, this.rand(20, 160)));
      }
    };
    refill("sell", m.asks, 1);
    refill("buy", m.bids, -1);
  }

  private insert(m: Market, order: RestingOrder) {
    const book = order.side === "buy" ? m.bids : m.asks;
    const direction = order.side === "buy" ? -1 : 1;
    const at = book.findIndex((o) => (o.price - order.price) * direction > 0);
    book.splice(at < 0 ? book.length : at, 0, order);
  }

  private match(m: Market, side: Side, qty: number, limit: number | null) {
    const opposite = side === "buy" ? m.asks : m.bids;
    const fills: Match[] = [];
    let rem = qty;
    while (
      rem > 0 &&
      opposite.length &&
      crosses(side, opposite[0].price, limit)
    ) {
      const maker = opposite[0];
      const q = Math.min(rem, maker.rem);
      maker.rem -= q;
      rem -= q;
      fills.push({ maker, qty: q, price: maker.price });
      m.last = maker.price;
      if (!maker.rem) opposite.shift();
    }
    return { fills, rem };
  }

  private createFill(
    m: Market,
    orderId: string,
    side: Side,
    qty: number,
    price: number,
    holdPrice: number,
  ): Fill {
    const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    const suffix = Array.from({ length: 4 }, () =>
      this.pick([...alphabet]),
    ).join("");
    return {
      id: `fill_01JC${suffix}${String(++this.fillSeq).padStart(2, "0")}`,
      orderId,
      sym: m.sym,
      side,
      qty,
      price,
      holdPrice,
    };
  }

  private makerFill(m: Market, f: Match) {
    if (!f.maker.rem) this.open = this.open.filter((o) => o !== f.maker);
    return this.createFill(
      m,
      f.maker.id,
      f.maker.side,
      f.qty,
      f.price,
      f.maker.price,
    );
  }

  private queueFill(fill: Fill) {
    this.write("outbox", `store ${fill.id}`, "queued", { mine: true });
    this.outbox++;
    this.after(this.hop, () => this.deliver(fill, 1));
  }

  private deliver(fill: Fill, attempt: number) {
    const m = this.market(fill.sym);
    this.payloads.webhook = {
      method: "POST",
      path: WEBHOOK_PATH,
      attempt,
      body: {
        events: [
          {
            fill_id: fill.id,
            type: "fill",
            client_order_id: fill.orderId,
            account: ACCOUNT,
            symbol: fill.sym,
            side: fill.side,
            last_qty: String(fill.qty),
            last_px: decimal(fill.price),
          },
        ],
      },
    };
    if (!this.gameOnline) {
      this.write("webhook", `POST ${WEBHOOK_PATH} try ${attempt}`, "503", {
        mine: true,
        tone: "bad",
      });
      this.write("outbox", `${fill.id} kept, retry scheduled`, "pending", {
        mine: true,
      });
      this.after(RETRY_MS, () => this.deliver(fill, attempt + 1));
      return;
    }
    this.write("webhook", `POST ${WEBHOOK_PATH} try ${attempt}`, "200", {
      mine: true,
      tone: "good",
    });
    this.chain([
      () => this.write("game", "INSERT applied_fills", "1 row", { mine: true }),
      () => {
        this.settle(fill);
        const grant =
          fill.side === "buy"
            ? `grant ${fill.qty} ${m.name.toLowerCase()}`
            : `pay ${money(fill.qty * fill.price)} gold`;
        this.write("game", grant, "done", { mine: true, tone: "good" });
        this.filled[fill.orderId] = (this.filled[fill.orderId] ?? 0) + fill.qty;
        const verb = fill.side === "buy" ? "Bought" : "Sold";
        this.showNotice(
          `${verb} ${this.filled[fill.orderId]} ${m.name.toLowerCase()}`,
          false,
        );
        this.outbox--;
      },
    ]);
  }

  private settle(f: Fill) {
    const item = this.items[f.sym];
    if (f.side === "buy") {
      this.heldGold -= f.qty * f.holdPrice;
      this.gold += f.qty * (f.holdPrice - f.price); // price improvement comes back
      item.free += f.qty;
    } else {
      item.held -= f.qty;
      this.gold += f.qty * f.price;
    }
  }

  private capture(
    method: string,
    path: string,
    body: unknown,
    status: number,
    response: unknown,
  ) {
    const headers = { Authorization: SERVICE_TOKEN, "X-MMX-Account": ACCOUNT };
    this.payloads.request = { method, path, headers, body };
    this.payloads.response = { status, body: response };
  }

  private captureBookUpdate(
    m: Market,
    fills: Match[],
    resting: RestingOrder | null,
  ) {
    const sides = new Map<number, Side>();
    for (const f of fills) sides.set(f.price, f.maker.side);
    if (resting) sides.set(resting.price, resting.side);
    const updates = [...sides].map(([price, side]) => ({
      side,
      price_level: money(price),
      new_quantity: String(
        (side === "buy" ? m.bids : m.asks)
          .filter((o) => o.price === price)
          .reduce((total, o) => total + o.rem, 0),
      ),
    }));
    this.payloads.ws = {
      body: {
        channel: "l2_data",
        sequence_num: this.orderSeq,
        events: [{ type: "update", symbol: m.sym, updates }],
      },
    };
  }

  private recordPrice(m: Market) {
    m.hist.push({ t: Date.now(), p: m.last });
    if (m.hist.length > HISTORY) m.hist.shift();
  }

  private write(
    service: Service,
    message: string,
    result = "",
    options: { mine?: boolean; tone?: "good" | "bad" } = {},
  ) {
    const entry = {
      id: ++this.logSeq,
      at: this.clock ?? Date.now(),
      service,
      message,
      result,
      tone: options.tone,
      mine: !!options.mine,
    };
    this.log = [...this.log.slice(1 - MAX_LOG), entry];
  }

  private showNotice(text: string, bad: boolean) {
    this.notice = { text, bad };
    if (this.noticeTimer) clearTimeout(this.noticeTimer);
    this.noticeTimer = setTimeout(() => {
      this.notice = null;
      this.publish();
    }, NOTICE_MS);
  }

  // Staggered so each hop can be followed by eye.
  private chain(steps: (() => void)[]) {
    steps.forEach((step, i) => this.after(i * this.hop, step));
  }

  private after(ms: number, fn: () => void) {
    const timer = setTimeout(() => {
      this.timers.delete(timer);
      fn();
      this.publish();
    }, ms);
    this.timers.add(timer);
  }

  private publish() {
    this.markets.forEach((m) => this.fillDepth(m));
    this.version++;
    this.listeners.forEach((listener) => listener());
  }

  private nextOrderId() {
    return `order_${++this.orderSeq}`;
  }

  private rand(min: number, max: number) {
    return min + Math.floor(this.random() * (max - min + 1));
  }

  private pick<T>(list: T[]) {
    return list[this.rand(0, list.length - 1)];
  }
}

/** Prices are hundredths of a gold here, and decimal strings on the wire. */
function decimal(price: number) {
  return (price / 100).toFixed(2);
}

function crosses(side: Side, price: number, limit: number | null) {
  return limit === null || (side === "buy" ? price <= limit : price >= limit);
}
