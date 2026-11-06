import { Link } from "react-router-dom";
import { Card, Notice, PageHeader, SectionTitle, Status, TableFrame, cn } from "../components/ui";
import type { State } from "../components/ui";
import { api } from "../lib/api";
import type {
  MarketList,
  ServiceStatus,
  Status as ExchangeStatus,
  WebhookPosition,
} from "../lib/api";
import { usePolled } from "../lib/hooks";

const verdicts: Record<ExchangeStatus["verdict"], { state: State; title: string; detail: string }> =
  {
    ok: { state: "good", title: "Everything is running", detail: "Every service is ready." },
    degraded: {
      state: "warn",
      title: "Trading works, with something missing",
      detail: "A gateway can take orders, but at least one other service isn't ready.",
    },
    down: {
      state: "bad",
      title: "No gateway can take orders",
      detail: "Trading has stopped until a gateway is ready.",
    },
  };

function serviceState(service: ServiceStatus): { state: State; text: string } {
  if (service.ready) return { state: "good", text: "Ready" };
  if (service.reachable) return { state: "warn", text: "Not ready" };
  return { state: service.required ? "bad" : "warn", text: "Unreachable" };
}

const webhookWords: Record<WebhookPosition["state"], string> = {
  not_configured: "Not configured",
  halted: "Halted",
  verifying: "Verifying",
  failing: "Failing",
  delivering: "Delivering",
  idle: "Caught up",
};

function Stat({ label, value, to }: { label: string; value: string; to: string }) {
  return (
    <Link to={to} className="block rounded-card border border-line bg-surface p-5 hover:bg-raised">
      <p className="text-sm text-muted">{label}</p>
      <p className="tabular mt-1 text-3xl font-semibold tracking-heading">{value}</p>
    </Link>
  );
}

export function Overview() {
  const status = usePolled(() => api.get<ExchangeStatus>("/status"), 5000);
  const markets = usePolled(() => api.get<MarketList>("/markets"), 5000);
  const webhooks = usePolled(() => api.get<WebhookPosition>("/webhooks"), 5000);

  const verdict = status.data ? verdicts[status.data.verdict] : null;
  const list = markets.data?.markets ?? [];
  const halted = list.filter((market) => market.halted).length;

  return (
    <>
      <PageHeader title="Overview" />
      {status.error && (
        <div className="mb-4">
          <Notice state="bad" title="Can't read status">
            {status.error}
            {status.data ? " What's below is the last good reading." : ""}
          </Notice>
        </div>
      )}

      {verdict && (
        <Card className="mb-4 p-6">
          <Status state={verdict.state}>
            {status.data?.verdict === "ok"
              ? "Healthy"
              : status.data?.verdict === "down"
                ? "Down"
                : "Degraded"}
          </Status>
          <p className="mt-2 text-2xl font-semibold tracking-heading md:text-3xl">
            {verdict.title}
          </p>
          <p className="mt-1 text-muted">{verdict.detail}</p>
        </Card>
      )}

      <div className="mb-4 grid gap-4 sm:grid-cols-3">
        <Stat label="Markets" value={markets.data ? String(list.length) : "-"} to="/markets" />
        <Stat label="Halted" value={markets.data ? String(halted) : "-"} to="/markets" />
        <Stat
          label="Webhooks"
          value={
            webhooks.data ? webhookWords[webhooks.data.state] : webhooks.error ? "Unknown" : "-"
          }
          to="/webhooks"
        />
      </div>

      <Card>
        <div className="p-5 pb-3">
          <SectionTitle>Services</SectionTitle>
        </div>
        <TableFrame>
          <table className="w-full min-w-[620px] text-left text-sm">
            <thead className="text-muted">
              <tr className="border-y border-line">
                <th className="px-5 py-2 font-medium">Service</th>
                <th className="px-5 py-2 font-medium">State</th>
                <th className="px-5 py-2 font-medium">Version</th>
                <th className="px-5 py-2 font-medium">Address</th>
              </tr>
            </thead>
            <tbody>
              {status.data?.services.map((service) => {
                const { state, text } = serviceState(service);
                return (
                  <tr
                    key={`${service.name}-${service.target}`}
                    className={cn("border-b border-line last:border-b-0")}
                  >
                    <td className="px-5 py-3 font-medium">{service.name}</td>
                    <td className="px-5 py-3">
                      <Status state={state}>{text}</Status>
                      {service.error && (
                        <p className="mt-0.5 max-w-sm break-words text-xs text-muted">
                          {service.error}
                        </p>
                      )}
                    </td>
                    <td className="tabular px-5 py-3 text-muted">{service.version ?? "-"}</td>
                    <td className="px-5 py-3 font-mono text-xs text-muted">{service.target}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </TableFrame>
      </Card>
    </>
  );
}
