import { useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Button, Card, Notice, PageHeader, Status, TableFrame, cn } from "../components/ui";
import type { State } from "../components/ui";
import { BASE, api, messageOf } from "../lib/api";
import { useSession } from "../lib/session";
import type { OrderRow, TradeRow, WebhookPosition } from "../lib/api";

const KEEP = 500;
const FIRST_LOAD = 100;

type Tab = "orders" | "trades" | "webhooks";
type Link = "connecting" | "live" | "lost";

/** Newest first, with each execution report once, because a first load and the stream overlap. */
function merge<T extends { exec_id: number }>(rows: T[], incoming: T[]): T[] {
  const seen = new Set(rows.map((row) => row.exec_id));
  const fresh = incoming.filter((row) => !seen.has(row.exec_id));
  return [...fresh.reverse(), ...rows].slice(0, KEEP);
}

function clock(time: string) {
  const parsed = new Date(time);
  return Number.isNaN(parsed.getTime()) ? time : parsed.toLocaleTimeString();
}

/** The end of an ID, because a UUIDv7 starts with its timestamp, so the starts all look alike. */
function short(id?: string) {
  return id ? id.slice(-8) : "-";
}

function Side({ side }: { side: string }) {
  return <span className={side === "buy" ? "text-good" : "text-bad"}>{side}</span>;
}

function Empty({ what }: { what: string }) {
  return (
    <div className="p-10 text-center">
      <p className="text-lg font-semibold tracking-heading">No {what} yet</p>
      <p className="mt-1 text-muted">They appear here as they're recorded.</p>
    </div>
  );
}

function Orders({ rows }: { rows: OrderRow[] }) {
  if (rows.length === 0) return <Empty what="orders" />;
  return (
    <TableFrame>
      <table className="w-full min-w-[760px] text-left text-sm">
        <thead className="text-muted">
          <tr className="border-b border-line">
            {[
              "Time",
              "Market",
              "Side",
              "Event",
              "Status",
              "Size",
              "Price",
              "Filled",
              "Player",
              "Order",
            ].map((name) => (
              <th key={name} className="px-4 py-3 font-medium">
                {name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="tabular">
          {rows.map((row) => (
            <tr key={row.exec_id} className="border-b border-line last:border-b-0">
              <td className="whitespace-nowrap px-4 py-2 text-muted">{clock(row.time)}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.symbol}</td>
              <td className="px-4 py-2">
                <Side side={row.side} />
              </td>
              <td className="px-4 py-2">{row.exec_type ?? "-"}</td>
              <td className="px-4 py-2">
                {row.status ?? "-"}
                {row.reason && row.reason !== "none" && (
                  <span className="block text-xs text-muted">{row.reason}</span>
                )}
              </td>
              <td className="px-4 py-2 font-mono text-xs">{row.size ?? "-"}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.price ?? "market"}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.filled_size ?? "-"}</td>
              <td className="px-4 py-2 font-mono text-xs text-muted" title={row.player}>
                {short(row.player)}
              </td>
              <td className="px-4 py-2 font-mono text-xs text-muted" title={row.client_order_id}>
                {short(row.client_order_id)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </TableFrame>
  );
}

function Trades({ rows }: { rows: TradeRow[] }) {
  if (rows.length === 0) return <Empty what="trades" />;
  return (
    <TableFrame>
      <table className="w-full min-w-[620px] text-left text-sm">
        <thead className="text-muted">
          <tr className="border-b border-line">
            {["Time", "Market", "Side", "Size", "Price", "Match", "Player", "Order"].map((name) => (
              <th key={name} className="px-4 py-3 font-medium">
                {name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="tabular">
          {rows.map((row) => (
            <tr key={row.exec_id} className="border-b border-line last:border-b-0">
              <td className="whitespace-nowrap px-4 py-2 text-muted">{clock(row.time)}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.symbol}</td>
              <td className="px-4 py-2">
                <Side side={row.side} />
              </td>
              <td className="px-4 py-2 font-mono text-xs">{row.size ?? "-"}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.price ?? "-"}</td>
              <td className="px-4 py-2 font-mono text-xs">{row.match_id ?? "-"}</td>
              <td className="px-4 py-2 font-mono text-xs text-muted" title={row.player}>
                {short(row.player)}
              </td>
              <td className="px-4 py-2 font-mono text-xs text-muted" title={row.client_order_id}>
                {short(row.client_order_id)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </TableFrame>
  );
}

const deliveryWords: Record<WebhookPosition["state"], { state: State; text: string }> = {
  not_configured: { state: "warn", text: "Not configured" },
  halted: { state: "bad", text: "Halted" },
  verifying: { state: "warn", text: "Verifying" },
  failing: { state: "bad", text: "Failing" },
  delivering: { state: "warn", text: "Delivering" },
  idle: { state: "good", text: "Caught up" },
};

export function WebhookFacts({ position }: { position: WebhookPosition }) {
  const { state, text } = deliveryWords[position.state];
  const facts: [string, string][] = [
    ["Waiting to deliver", `${position.backlog.toLocaleString()} bytes`],
    ["Failures in a row", position.consecutive_failures.toLocaleString()],
    ["Delivered up to", position.delivered_position.toLocaleString()],
  ];
  return (
    <div>
      <Status state={state}>{text}</Status>
      {position.halted && <p className="mt-1 text-sm text-bad">{position.halted}</p>}
      <dl className="mt-3 grid gap-3 sm:grid-cols-3">
        {facts.map(([label, value]) => (
          <div key={label}>
            <dt className="text-sm text-muted">{label}</dt>
            <dd className="tabular font-mono text-sm">{value}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

function Webhooks({ position }: { position: WebhookPosition | null }) {
  return (
    <div className="p-5">
      {position ? (
        <WebhookFacts position={position} />
      ) : (
        <p className="text-muted">Waiting for the outbox.</p>
      )}
      <p className="mt-4 text-sm text-muted">
        The outbox reports how far it has delivered, and doesn't keep a record of each delivery.
      </p>
    </div>
  );
}

export function Activity() {
  const { load } = useSession();
  const [params, setParams] = useSearchParams();
  const symbol = params.get("symbol") ?? "";
  const [tab, setTab] = useState<Tab>("orders");
  const [orders, setOrders] = useState<OrderRow[]>([]);
  const [trades, setTrades] = useState<TradeRow[]>([]);
  const [position, setPosition] = useState<WebhookPosition | null>(null);
  const [link, setLink] = useState<Link>("connecting");
  const [sources, setSources] = useState<Record<string, string>>({});
  const [paused, setPaused] = useState(false);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  useEffect(() => {
    void api
      .get<{ orders: OrderRow[] }>(`/orders?limit=${FIRST_LOAD}`)
      .then((body) => setOrders((rows) => merge(rows, [...body.orders].reverse())))
      .catch((error) => setSources((now) => ({ ...now, orders: messageOf(error) })));
    void api
      .get<{ trades: TradeRow[] }>(`/trades?limit=${FIRST_LOAD}`)
      .then((body) => setTrades((rows) => merge(rows, [...body.trades].reverse())))
      .catch((error) => setSources((now) => ({ ...now, trades: messageOf(error) })));

    const stream = new EventSource(`${BASE}/events`);
    stream.onopen = () => setLink("live");
    // A stream can't show a 401, so this finds out whether the session is what ended.
    stream.onerror = () => {
      setLink("lost");
      void load();
    };
    const on = <T,>(kind: string, handle: (data: T) => void) =>
      stream.addEventListener(kind, (event) => handle(JSON.parse(event.data) as T));

    on<OrderRow>("order", (row) => {
      if (!pausedRef.current) setOrders((rows) => merge(rows, [row]));
    });
    on<TradeRow>("trade", (row) => {
      if (!pausedRef.current) setTrades((rows) => merge(rows, [row]));
    });
    on<WebhookPosition>("webhook", setPosition);
    on<{ source: string; error: string }>("source_failed", ({ source, error }) =>
      setSources((now) => ({ ...now, [source]: error })),
    );
    on<{ source: string }>("source_recovered", ({ source }) =>
      setSources(({ [source]: _gone, ...rest }) => rest),
    );
    return () => stream.close();
  }, [load]);

  const symbols = useMemo(
    () => [...new Set([...orders, ...trades].map((row) => row.symbol))].sort(),
    [orders, trades],
  );
  const shownOrders = symbol ? orders.filter((row) => row.symbol === symbol) : orders;
  const shownTrades = symbol ? trades.filter((row) => row.symbol === symbol) : trades;

  const tabs: [Tab, string][] = [
    ["orders", "Orders"],
    ["trades", "Trades"],
    ["webhooks", "Webhooks"],
  ];
  const linkState: Record<Link, { state: State; text: string }> = {
    connecting: { state: "idle", text: "Connecting" },
    live: paused ? { state: "warn", text: "Paused" } : { state: "good", text: "Live" },
    lost: { state: "bad", text: "Reconnecting" },
  };

  return (
    <>
      <PageHeader
        title="Activity"
        detail="Orders, trades and webhook delivery across every player, as recorded from the tape."
        actions={<Status state={linkState[link].state}>{linkState[link].text}</Status>}
      />

      {Object.keys(sources).length > 0 && (
        <div className="mb-4">
          <Notice state="warn" title="Some activity can't be read right now">
            {Object.entries(sources).map(([source, error]) => (
              <p key={source} className="break-words">
                {source}: {error}
              </p>
            ))}
          </Notice>
        </div>
      )}

      <div className="mb-4 flex flex-wrap items-center gap-2">
        <div role="tablist" aria-label="Activity" className="flex rounded-full bg-raised p-1">
          {tabs.map(([value, label]) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={tab === value}
              onClick={() => setTab(value)}
              className={cn(
                "h-7 rounded-full px-4 text-sm font-medium",
                tab === value ? "bg-surface text-ink" : "text-muted hover:text-ink",
              )}
            >
              {label}
            </button>
          ))}
        </div>
        <label className="sr-only" htmlFor="market-filter">
          Market
        </label>
        <select
          id="market-filter"
          value={symbol}
          onChange={(event) => setParams(event.target.value ? { symbol: event.target.value } : {})}
          className="h-9 rounded-full border border-line bg-surface px-3 text-sm"
        >
          <option value="">All markets</option>
          {[...new Set([...symbols, ...(symbol ? [symbol] : [])])].map((name) => (
            <option key={name}>{name}</option>
          ))}
        </select>
        <Button onClick={() => setPaused(!paused)}>{paused ? "Resume" : "Pause"}</Button>
      </div>

      <Card>
        {tab === "orders" && <Orders rows={shownOrders} />}
        {tab === "trades" && <Trades rows={shownTrades} />}
        {tab === "webhooks" && <Webhooks position={position} />}
      </Card>
    </>
  );
}
