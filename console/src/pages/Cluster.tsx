import { Link } from "react-router-dom";
import { ConditionsTable, MembersTable, ServicesTable, behindWord } from "../components/Health";
import { Metric, Tiles, Word, cn } from "../components/ui";
import { useExchange } from "../lib/exchange";
import type { Tone } from "../lib/exchange";
import { ago, bytes, count, utc } from "../lib/format";
import { useTitle } from "../lib/hooks";

function Stage({
  name,
  value,
  note,
  tone = "plain",
}: {
  name: string;
  value: string;
  note?: string;
  tone?: Tone;
}) {
  return (
    <div
      className={cn(
        "min-w-[132px] flex-1 border bg-surface px-2.5 py-1.5",
        tone === "bad" ? "border-bad" : tone === "warn" ? "border-warn" : "border-line",
      )}
    >
      <p className="label">{name}</p>
      <p className="tabular">
        <Word tone={tone}>{value}</Word>
      </p>
      <p className="tabular truncate text-[14px] text-faint">{note ?? " "}</p>
    </div>
  );
}

const Arrow = () => (
  <span aria-hidden className="flex-none self-center text-faint">
    →
  </span>
);

/** The path an order takes, and where the tape goes from there, with the break marked. */
function Pipeline() {
  const { status, canTrade } = useExchange();
  if (!status) return null;
  const gateways = status.services.filter((service) => service.name === "gateway");
  const ready = gateways.filter((gateway) => gateway.ready).length;
  const { cluster, consumers } = status;
  const position = cluster.members.reduce((top, member) => Math.max(top, member.position ?? 0), 0);
  const stage = (name: string) => {
    const { word, tone } = behindWord(
      consumers.find((row) => row.name === name),
      canTrade,
    );
    return { value: word || "No lag to show", tone };
  };
  const websocket = status.services.find((service) => service.name === "websocket");
  const feed = consumers.find((row) => row.name === "market-data");
  return (
    <div className="flex flex-none flex-wrap items-stretch gap-1.5 border-b border-line bg-canvas p-2 lg:flex-nowrap">
      <Stage
        name="Gateways"
        value={`${ready} of ${gateways.length} ready`}
        note={`${cluster.gateways_reporting} report the cluster`}
        tone={ready === 0 ? "bad" : ready < gateways.length ? "warn" : "plain"}
      />
      <Arrow />
      <Stage
        name="Engine"
        value={
          cluster.mode === "cluster"
            ? `${cluster.members_up} of ${cluster.members_total} members`
            : cluster.mode === "standalone"
              ? "Single process"
              : "Unknown"
        }
        note={cluster.leader !== null ? `Leader ${cluster.leader}` : "No leader"}
        tone={canTrade ? "plain" : "bad"}
      />
      <Arrow />
      <Stage
        name="Tape"
        value={position ? bytes(position) : "Unknown"}
        note={`Newest event ${count(cluster.head_exec_id)}`}
      />
      <Arrow />
      <div className="flex min-w-0 flex-[3] flex-wrap gap-1.5 lg:flex-nowrap">
        <Stage name="Outbox" {...stage("outbox")} note="Webhooks to the game" />
        <Stage name="Trade reporter" {...stage("trade-reporter")} note="QuestDB" />
        <Stage
          name="Market data"
          value={feed?.state === "stalled" ? "Stopped" : "Following"}
          tone={feed?.state === "stalled" ? "bad" : "plain"}
          note={feed?.last_seen ? `Ticker ${ago(feed.last_seen)}` : "No trades heard yet"}
        />
      </div>
      {websocket && (
        <>
          <Arrow />
          <Stage
            name="Websocket"
            value={websocket.ready ? "Listening" : "Unreachable"}
            tone={websocket.ready ? "plain" : "warn"}
            note="Browsers"
          />
        </>
      )}
    </div>
  );
}

export function Cluster() {
  useTitle("Cluster");
  const { status } = useExchange();
  const cluster = status?.cluster;
  const clustered = cluster?.mode === "cluster";
  return (
    <div className="flex flex-1 flex-col lg:min-h-0">
      <Tiles className="flex-none grid-cols-2 border-b border-line md:grid-cols-5">
        <Metric
          label="Quorum"
          value={
            !cluster
              ? null
              : clustered
                ? `${cluster.members_up} of ${cluster.members_total} members`
                : cluster.mode === "standalone"
                  ? "Single engine"
                  : "Unknown"
          }
          tone={
            clustered && !cluster?.quorum ? "bad" : cluster?.mode === "unknown" ? "bad" : "plain"
          }
          note={clustered ? `${cluster?.quorum_needed} needed` : undefined}
        />
        <Metric
          label="Leader"
          value={!cluster ? null : cluster.leader === null ? "None" : `Member ${cluster.leader}`}
          tone={clustered && cluster?.leader === null ? "bad" : "plain"}
          note={clustered ? `Term ${cluster?.term}` : undefined}
        />
        <Metric
          label="Last leader change"
          value={
            !cluster
              ? null
              : cluster.last_leader_change
                ? ago(cluster.last_leader_change)
                : "None seen"
          }
          note={
            cluster?.last_leader_change
              ? `${utc(cluster.last_leader_change)} UTC`
              : "Since each gateway started"
          }
        />
        <Metric
          label="Leader changes"
          value={cluster ? count(cluster.leader_changes) : null}
          note="Most any gateway has seen"
        />
        <Metric
          label="Gateways reporting"
          value={cluster ? `${cluster.gateways_reporting} of ${cluster.gateways_total}` : null}
          tone={cluster && cluster.gateways_reporting === 0 ? "bad" : "plain"}
          note={
            <Link to="/activity" className="text-accent">
              Open activity
            </Link>
          }
        />
      </Tiles>
      <Pipeline />
      <div className="grid flex-1 gap-px bg-line lg:min-h-0 lg:grid-cols-[minmax(0,2fr)_minmax(0,5fr)] lg:grid-rows-[minmax(0,1fr)_minmax(0,1fr)]">
        <MembersTable id="cluster-members" status={status} className="h-[200px] lg:h-auto" />
        <ServicesTable
          id="cluster-services"
          status={status}
          className="h-[340px] lg:row-span-2 lg:h-auto"
        />
        <ConditionsTable
          id="cluster-conditions"
          conditions={status?.conditions ?? []}
          className="h-[200px] lg:h-auto"
        />
      </div>
    </div>
  );
}
