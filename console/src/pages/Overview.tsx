import { Link } from "react-router-dom";
import { LineChart } from "../components/Sparkline";
import {
  Card,
  EmptyState,
  ErrorState,
  Notice,
  Page,
  PageHeader,
  SectionTitle,
  Skeleton,
  SkeletonRows,
  Status,
  TableFrame,
  tdClass,
  thClass,
} from "../components/ui";
import type { State } from "../components/ui";
import { api } from "../lib/api";
import type {
  ActivitySummary,
  MarketList,
  ServiceStatus,
  Status as ExchangeStatus,
  WebhookPosition,
} from "../lib/api";
import { count } from "../lib/format";
import { usePolled, useTitle } from "../lib/hooks";

const verdicts: Record<ExchangeStatus["verdict"], { state: State; title: string }> = {
  ok: { state: "good", title: "All services ready" },
  degraded: { state: "warn", title: "Degraded: a service isn't ready" },
  down: { state: "bad", title: "Down: no gateway can take orders" },
};

function serviceState(service: ServiceStatus): { state: State; text: string } {
  if (service.ready) return { state: "good", text: "Ready" };
  if (service.reachable) return { state: "warn", text: "Not ready" };
  return { state: service.required ? "bad" : "warn", text: "Unreachable" };
}

const webhookWords: Record<WebhookPosition["state"], { state: State; text: string }> = {
  not_configured: { state: "idle", text: "Not configured" },
  halted: { state: "bad", text: "Halted" },
  verifying: { state: "warn", text: "Verifying" },
  failing: { state: "bad", text: "Failing" },
  delivering: { state: "warn", text: "Delivering" },
  idle: { state: "good", text: "Caught up" },
};

function Tile({
  label,
  to,
  value,
  note,
}: {
  label: string;
  to: string;
  value: React.ReactNode;
  note?: React.ReactNode;
}) {
  return (
    <Link
      to={to}
      className="block rounded-2xl border border-line bg-surface p-4 transition-colors hover:border-accent"
    >
      <p className="text-[13px] text-muted">{label}</p>
      <div className="tabular mt-1 font-display text-[28px] font-medium leading-9 tracking-heading">
        {value ?? <Skeleton className="my-1 h-7 w-16" />}
      </div>
      <div className="mt-0.5 h-5 text-[13px]">{note}</div>
    </Link>
  );
}

export function Overview() {
  useTitle("Overview");
  const status = usePolled(() => api.get<ExchangeStatus>("/status"), 5000);
  const markets = usePolled(() => api.get<MarketList>("/markets?limit=1"), 5000);
  const webhooks = usePolled(() => api.get<WebhookPosition>("/webhooks"), 5000);
  const summary = usePolled(() => api.get<ActivitySummary>("/activity/summary"), 15000);

  const verdict = status.data ? verdicts[status.data.verdict] : null;
  const counts = markets.data?.counts;
  const delivery = webhooks.data ? webhookWords[webhooks.data.state] : null;
  const rate = summary.data?.reports_per_minute ?? [];
  const latest = rate.length ? rate[rate.length - 1].reports : null;

  return (
    <Page>
      <PageHeader
        title="Overview"
        detail={verdict ? <Status state={verdict.state}>{verdict.title}</Status> : undefined}
      />

      {status.error && (
        <Notice state="bad" title="Can't read status" className="mb-3">
          {status.error}
        </Notice>
      )}

      <div className="grid grid-cols-2 gap-2 lg:grid-cols-4">
        <Tile
          label="Markets"
          to="/markets"
          value={counts ? count(counts.all) : markets.error ? "-" : null}
          note={counts ? <span className="text-muted">{count(counts.open)} open</span> : null}
        />
        <Tile
          label="Halted"
          to="/markets?state=halted"
          value={counts ? count(counts.halted) : markets.error ? "-" : null}
          note={
            counts && counts.out_of_sync > 0 ? (
              <span className="text-warn">{count(counts.out_of_sync)} out of sync</span>
            ) : null
          }
        />
        <Tile
          label="Reports, last minute"
          to="/activity"
          value={summary.data ? count(latest ?? 0) : summary.error ? "-" : null}
        />
        <Tile
          label="Webhook backlog"
          to="/webhooks"
          value={webhooks.data ? `${count(webhooks.data.backlog)} B` : webhooks.error ? "-" : null}
          note={delivery ? <Status state={delivery.state}>{delivery.text}</Status> : null}
        />
      </div>

      <div className="mt-2 grid gap-2 lg:grid-cols-[2fr_1fr]">
        <Card className="flex h-[260px] flex-col p-4">
          <SectionTitle>Reports per minute, past hour</SectionTitle>
          {summary.error && !summary.data ? (
            <ErrorState title="Can't read the tape" detail={summary.error} />
          ) : summary.data && rate.length === 0 ? (
            <EmptyState title="No reports in the past hour" />
          ) : (
            <LineChart
              className="mt-2 flex-1"
              zero
              ariaLabel="Reports per minute over the past hour"
              format={(value) => count(Math.round(value))}
              points={rate.map((point) => ({
                label: new Date(point.time).toLocaleTimeString("en-GB", {
                  hour: "2-digit",
                  minute: "2-digit",
                }),
                value: point.reports,
                text: `${count(point.reports)} reports`,
              }))}
            />
          )}
        </Card>

        <Card className="flex h-[260px] flex-col overflow-hidden">
          <SectionTitle className="p-4 pb-2">Busiest markets, past hour</SectionTitle>
          {summary.loading ? (
            <SkeletonRows rows={4} />
          ) : summary.data?.busiest.length ? (
            <ul className="min-h-0 flex-1 overflow-y-auto">
              {summary.data.busiest.map((market) => (
                <li key={market.symbol} className="border-t border-hairline">
                  <Link
                    to={`/markets/${market.symbol}`}
                    className="flex items-center justify-between px-4 py-1.5 text-sm hover:bg-raised"
                  >
                    <span className="font-code">{market.symbol}</span>
                    <span className="tabular text-muted">{count(market.reports)}</span>
                  </Link>
                </li>
              ))}
            </ul>
          ) : (
            <EmptyState title="No activity" />
          )}
        </Card>
      </div>

      <Card className="mt-2">
        <SectionTitle className="px-4 pb-1 pt-3">Services</SectionTitle>
        {status.loading ? (
          <SkeletonRows rows={5} />
        ) : (
          <TableFrame>
            <table className="w-full min-w-[620px] text-left text-sm">
              <thead>
                <tr>
                  <th className={thClass}>Service</th>
                  <th className={thClass}>State</th>
                  <th className={thClass}>Version</th>
                  <th className={thClass}>Address</th>
                </tr>
              </thead>
              <tbody>
                {status.data?.services.map((service) => {
                  const { state, text } = serviceState(service);
                  return (
                    <tr
                      key={`${service.name}-${service.target}`}
                      className="border-t border-hairline"
                    >
                      <td className={`${tdClass} font-medium`}>{service.name}</td>
                      <td className={tdClass}>
                        <Status state={state}>{text}</Status>
                        {service.error && (
                          <p className="max-w-md break-words text-xs text-muted">{service.error}</p>
                        )}
                      </td>
                      <td className={`${tdClass} tabular text-muted`}>{service.version ?? "-"}</td>
                      <td className={`${tdClass} font-code text-[12.5px] text-muted`}>
                        {service.target}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </TableFrame>
        )}
      </Card>
    </Page>
  );
}
