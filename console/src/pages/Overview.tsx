import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { ConditionsTable, MembersTable, ServicesTable, verdictWords } from "../components/Health";
import { TimeChart } from "../components/TimeChart";
import type { Series, TimePoint } from "../components/TimeChart";
import { Metric, Panel, Segmented, Skeleton, Tiles, Word, cn } from "../components/ui";
import { api } from "../lib/api";
import type {
  ActivityLog,
  ActivitySummary,
  LogEntry,
  MarketList,
  Tally,
  WebhookPosition,
} from "../lib/api";
import { useExchange } from "../lib/exchange";
import type { Tone } from "../lib/exchange";
import { bytes, count, millis, rate, utc, utcClock } from "../lib/format";
import { usePolled, useTitle } from "../lib/hooks";

type Range = "15m" | "1h" | "6h" | "24h";
const RANGE_MS: Record<Range, number> = {
  "15m": 900000,
  "1h": 3600000,
  "6h": 21600000,
  "24h": 86400000,
};

const SERIES: Series[] = [
  { key: "orders", label: "Orders", stroke: "stroke-s-orders", fill: "bg-s-orders fill-s-orders" },
  { key: "trades", label: "Trades", stroke: "stroke-s-trades", fill: "bg-s-trades fill-s-trades" },
  {
    key: "cancels",
    label: "Cancels",
    stroke: "stroke-s-cancels",
    fill: "bg-s-cancels fill-s-cancels",
  },
  {
    key: "rejects",
    label: "Rejects",
    stroke: "stroke-s-rejects",
    fill: "bg-s-rejects fill-s-rejects",
  },
];

/** Quiet stretches have no rows, so they're filled in, or a line would skip over them. */
function perSecond(summary: ActivitySummary, from: number, to: number): TimePoint[] {
  const size = summary.bucket_secs * 1000;
  const found = new Map(
    summary.series.map((row) => [
      Math.floor(new Date(row.time).getTime() / size) * size,
      row.counts,
    ]),
  );
  const points: TimePoint[] = [];
  const empty: Partial<Tally> = {};
  // The newest bucket is still filling, so it would always read as a fall.
  for (let time = Math.ceil(from / size) * size; time < to - size; time += size) {
    const counts = found.get(time) ?? empty;
    const each = (value = 0) => value / summary.bucket_secs;
    points.push({
      time,
      values: {
        orders: each(counts.orders),
        trades: each(counts.trades),
        cancels: each(counts.cancels),
        rejects: each(counts.rejects),
      },
    });
  }
  return points;
}

const webhookWords: Record<WebhookPosition["state"], { word: string; tone: Tone }> = {
  not_configured: { word: "Not configured", tone: "warn" },
  halted: { word: "Halted", tone: "bad" },
  verifying: { word: "Verifying", tone: "muted" },
  failing: { word: "Failing", tone: "bad" },
  delivering: { word: "Delivering", tone: "muted" },
  idle: { word: "Caught up", tone: "muted" },
};

const logTone: Record<LogEntry["level"], Tone> = { info: "plain", warn: "warn", bad: "bad" };

function EventLog({ log }: { log: ActivityLog | null }) {
  if (!log) return <Skeleton className="m-3 h-20" />;
  if (log.entries.length === 0) {
    return <p className="px-3 py-2 text-muted">Nothing since {utc(log.since)} UTC.</p>;
  }
  return (
    <ol>
      {log.entries.map((entry, index) => (
        <li
          key={`${entry.time}-${index}`}
          className="grid grid-cols-[64px_minmax(0,1fr)] gap-2 border-b border-hairline px-3 py-1"
          title={utc(entry.time)}
        >
          <time className="tabular font-code text-[14px] text-faint">{utcClock(entry.time)}</time>
          <p className="min-w-0 break-words">
            <span className="text-muted">{entry.subject}</span>{" "}
            <Word tone={logTone[entry.level]}>{entry.message}</Word>
            {entry.actor && <span className="text-faint"> by {entry.actor}</span>}
          </p>
        </li>
      ))}
    </ol>
  );
}

function Busiest({ summary }: { summary: ActivitySummary | null }) {
  if (!summary) return <Skeleton className="m-3 h-20" />;
  const top = summary.busiest;
  if (top.length === 0) return <p className="px-3 py-2 text-muted">No orders in this window.</p>;
  const most = Math.max(...top.map((market) => market.counts.orders), 1);
  return (
    <table className="tabular w-full table-fixed text-right text-[14px]">
      <thead className="text-[12px] text-faint">
        <tr>
          <th className="w-[76px] px-3 py-1 text-left font-medium">Market</th>
          <th className="px-1 font-medium">Orders</th>
          <th className="w-[20%] px-1 font-medium" aria-label="Share" />
          <th className="px-1 font-medium">Trades</th>
          <th className="px-1 font-medium">Cancels</th>
          <th className="px-3 font-medium">Rejects</th>
        </tr>
      </thead>
      <tbody>
        {top.map(({ symbol, counts }) => (
          <tr key={symbol} className="border-t border-hairline hover:bg-raised">
            <td className="px-3 py-1 text-left">
              <Link to={`/markets/${symbol}`} className="font-code font-medium text-accent">
                {symbol}
              </Link>
            </td>
            <td className="px-1">{count(counts.orders)}</td>
            <td className="px-1">
              <span
                className="block h-1 bg-faint/60"
                style={{ width: `${(counts.orders / most) * 100}%` }}
              />
            </td>
            <td className="px-1">{count(counts.trades)}</td>
            <td className="px-1">{count(counts.cancels)}</td>
            <td className="px-3">
              <Word tone={counts.rejects > 0 ? "bad" : "plain"}>{count(counts.rejects)}</Word>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

export function Overview() {
  useTitle("Overview");
  const { status, canTrade } = useExchange();
  const [range, setRange] = useState<Range>("1h");
  const summary = usePolled(
    () => api.get<ActivitySummary>(`/activity/summary?range=${range}`),
    5000,
    range,
  );
  const markets = usePolled(() => api.get<MarketList>("/markets?limit=1"), 5000);
  const webhooks = usePolled(() => api.get<WebhookPosition>("/webhooks"), 5000);
  const log = usePolled(() => api.get<ActivityLog>("/activity/log?limit=60"), 5000);

  const data = summary.data?.range === range ? summary.data : null;
  const to = Date.now();
  const from = to - RANGE_MS[range];
  const points = useMemo(() => (data ? perSecond(data, from, to) : []), [data]); // eslint-disable-line react-hooks/exhaustive-deps

  const minute = summary.data?.last_minute;
  const counts = markets.data?.counts;
  const cluster = status?.cluster;
  const latency = status?.latency;
  const verdict = status ? verdictWords[status.verdict] : null;
  const delivery = webhooks.data ? webhookWords[webhooks.data.state] : null;
  const failing = status?.conditions ?? [];
  const asOf = summary.data?.newest
    ? `Newest ${utcClock(summary.data.newest)} UTC`
    : "No orders yet";

  return (
    <div className="flex flex-1 flex-col xl:min-h-0">
      <Tiles className="flex-none grid-cols-2 border-b border-line md:grid-cols-4 xl:grid-cols-8">
        <Metric
          label="Exchange"
          value={verdict?.word}
          tone={verdict?.tone}
          note={
            status
              ? failing.length
                ? `${failing.length} failed ${failing.length === 1 ? "check" : "checks"}`
                : "All checks pass"
              : undefined
          }
        />
        <Metric
          label="Cluster"
          value={
            !cluster
              ? null
              : cluster.mode === "cluster"
                ? `${cluster.members_up} of ${cluster.members_total}`
                : cluster.mode === "standalone"
                  ? "Single"
                  : "Unknown"
          }
          tone={
            cluster && cluster.mode !== "standalone"
              ? !cluster.quorum
                ? "bad"
                : cluster.members_up < cluster.members_total
                  ? "warn"
                  : "plain"
              : "plain"
          }
          note={
            cluster?.mode === "cluster"
              ? `${cluster.quorum ? "Quorum" : "No quorum"}, leader ${cluster.leader ?? "none"}, term ${cluster.term}`
              : undefined
          }
        />
        <Metric
          label="Orders/s, last minute"
          value={minute ? rate(minute.orders / 60) : summary.error ? "-" : null}
          note={minute ? `${count(minute.orders)} orders` : undefined}
        />
        <Metric
          label="Trades/s"
          value={minute ? rate(minute.trades / 60) : summary.error ? "-" : null}
          note={minute ? `${count(minute.trades)} trades` : undefined}
        />
        <Metric
          label="Cancels and rejects/min"
          value={
            minute
              ? `${count(minute.cancels)} / ${count(minute.rejects)}`
              : summary.error
                ? "-"
                : null
          }
          tone={minute && minute.rejects > 0 ? "bad" : "plain"}
          note={
            latency && latency.refused > 0
              ? `${count(latency.refused)} refused by gateways`
              : minute
                ? "0 refused by gateways"
                : undefined
          }
        />
        <Metric
          label="Order latency p99"
          value={status ? (latency?.p99_ms ? millis(latency.p99_ms) : "-") : null}
          note={
            latency?.p50_ms
              ? `p50 ${millis(latency.p50_ms)}, gateway to engine`
              : "No orders in 60s"
          }
        />
        <Metric
          label="Markets open"
          value={counts ? count(counts.open) : markets.error ? "-" : null}
          tone={counts && counts.halted > 0 ? "warn" : "plain"}
          note={
            counts
              ? `of ${count(counts.all)}, ${count(counts.halted)} halted${counts.out_of_sync ? `, ${count(counts.out_of_sync)} out of sync` : ""}`
              : undefined
          }
        />
        <Metric
          label="Webhook backlog"
          value={webhooks.data ? bytes(webhooks.data.backlog) : webhooks.error ? "-" : null}
          tone={delivery?.tone === "bad" ? "bad" : "plain"}
          note={
            delivery && (
              <Word tone={delivery.tone}>
                {canTrade || delivery.tone === "bad" ? delivery.word : "Tape stopped"}
              </Word>
            )
          }
        />
      </Tiles>

      <div
        className={cn(
          "grid flex-1 gap-px bg-line xl:min-h-0",
          "xl:grid-cols-[minmax(0,5fr)_minmax(0,3fr)] xl:grid-rows-[minmax(240px,1fr)_minmax(0,1fr)]",
          "min-[1800px]:grid-cols-[minmax(0,5fr)_minmax(0,3fr)_minmax(0,3fr)]",
        )}
      >
        <Panel
          title="Throughput"
          aside={
            <>
              <span className="tabular hidden sm:inline">{asOf}</span>
              <Segmented
                label="Range"
                value={range}
                onChange={setRange}
                options={(["15m", "1h", "6h", "24h"] as Range[]).map((value) => ({
                  value,
                  label: value,
                }))}
              />
            </>
          }
          className="h-[300px] xl:h-auto"
          bodyClassName="flex flex-col"
        >
          {summary.error && !summary.data ? (
            <p className="p-3 text-bad">{summary.error}</p>
          ) : !data ? (
            <Skeleton className="m-3 flex-1" />
          ) : (
            <TimeChart
              className="flex-1"
              series={SERIES}
              points={points}
              from={from}
              to={to}
              unit="/s"
              zero
              format={(value) =>
                value >= 1000 ? `${value / 1000}k` : String(Number(value.toFixed(2)))
              }
              ariaLabel={`Orders, trades, cancels and rejects per second, past ${range}`}
            />
          )}
        </Panel>

        <Panel
          title="Recent events"
          aside={log.data && <span>Since {utcClock(log.data.since)} UTC</span>}
          className="h-[260px] xl:h-auto min-[1800px]:row-span-2"
          bodyClassName="overflow-y-auto"
        >
          <EventLog log={log.data} />
        </Panel>

        <Panel
          title="Busiest markets"
          aside={<span>Past {range}</span>}
          className="min-[1800px]:col-start-3 min-[1800px]:row-start-1"
          bodyClassName="overflow-y-auto"
        >
          <Busiest summary={data} />
        </Panel>

        <ServicesTable
          id="overview-services"
          status={status}
          className="h-[320px] xl:row-start-2 xl:h-auto"
        />

        {failing.length > 0 ? (
          <ConditionsTable
            id="overview-conditions"
            conditions={failing}
            className="h-[220px] xl:h-auto min-[1800px]:col-start-3 min-[1800px]:row-start-2"
          />
        ) : (
          <MembersTable
            id="overview-members"
            status={status}
            className="h-[180px] xl:hidden min-[1800px]:col-start-3 min-[1800px]:row-start-2 min-[1800px]:block"
          />
        )}
      </div>
    </div>
  );
}
