import { useMemo } from "react";
import type { ColumnDef } from "@tanstack/react-table";
import type { Condition, Consumer, Member, ServiceStatus, Status } from "../lib/api";
import { ago, bytes, count, utc } from "../lib/format";
import { DataTable } from "./DataTable";
import { Word } from "./ui";
import type { Tone } from "../lib/exchange";

export const verdictWords: Record<Status["verdict"], { word: string; tone: Tone }> = {
  ok: { word: "Operational", tone: "plain" },
  degraded: { word: "Degraded", tone: "warn" },
  down: { word: "Cannot trade", tone: "bad" },
};

export function serviceWord(service: ServiceStatus): { word: string; tone: Tone } {
  if (service.ready) return { word: "Ready", tone: "plain" };
  const tone = service.required ? "bad" : "warn";
  return { word: service.reachable ? "Not ready" : "Unreachable", tone };
}

/** How far behind, in tape events. With no engine, nobody knows where the tape ends. */
export function behindWord(consumer: Consumer | undefined, canTrade: boolean) {
  if (!consumer) return { word: "", tone: "muted" as Tone };
  if (consumer.state === "unknown") return { word: "Unknown", tone: "muted" as Tone };
  if (consumer.state === "stalled") {
    const behind =
      consumer.behind === undefined ? "Stopped" : `${count(consumer.behind)} events, stopped`;
    return { word: behind, tone: "bad" as Tone };
  }
  if (!canTrade) return { word: "Tape end unknown", tone: "muted" as Tone };
  if (consumer.behind === undefined) return { word: "", tone: "muted" as Tone };
  return {
    word: `${count(consumer.behind)} events`,
    tone: (consumer.behind > 0 ? "warn" : "plain") as Tone,
  };
}

const checkNames: Record<string, string> = {
  engine: "Engine link",
  halts_known: "Halt list",
  order_claims: "Order claims",
  valkey: "Valkey",
  reachable: "Reachable",
  ready: "Ready",
  quorum: "Quorum",
  delivery: "Delivery",
};

export const checkName = (check: string) => checkNames[check] ?? check;

export function ServicesTable({
  id,
  status,
  className,
}: {
  id: string;
  status: Status | null;
  className?: string;
}) {
  const canTrade = !!status?.can_trade;
  const columns = useMemo(() => {
    const consumer = (name: string) => status?.consumers.find((row) => row.name === name);
    const list: ColumnDef<ServiceStatus, never>[] = [
      {
        id: "name",
        header: "Service",
        size: 136,
        accessorFn: (row) => row.name as never,
        meta: { required: true },
      },
      {
        id: "target",
        header: "Instance",
        size: 190,
        accessorFn: (row) => row.target as never,
        meta: { mono: true },
      },
      {
        id: "state",
        header: "State",
        size: 92,
        accessorFn: (row) => serviceWord(row).word as never,
        cell: ({ row }) => {
          const { word, tone } = serviceWord(row.original);
          return <Word tone={tone}>{word}</Word>;
        },
      },
      {
        id: "behind",
        header: "Behind tape",
        size: 120,
        accessorFn: (row) => behindWord(consumer(row.name), canTrade).word as never,
        cell: ({ row }) => {
          const { word, tone } = behindWord(consumer(row.original.name), canTrade);
          return <Word tone={tone}>{word}</Word>;
        },
        meta: { align: "right" },
      },
      {
        id: "seen",
        header: "Last output",
        size: 104,
        accessorFn: (row) => (row.last_seen ?? "") as never,
        cell: ({ row }) => (row.original.last_seen ? ago(row.original.last_seen) : ""),
        meta: { align: "right" },
      },
      {
        id: "version",
        header: "Version",
        size: 76,
        accessorFn: (row) => (row.version ?? "") as never,
        meta: { mono: true },
      },
      {
        id: "error",
        header: "Problem",
        size: 340,
        accessorFn: (row) => (row.error ?? "") as never,
      },
    ];
    return list;
  }, [status, canTrade]);
  return (
    <DataTable
      id={id}
      title="Services"
      columns={columns}
      data={status?.services ?? []}
      rowId={(row) => `${row.name}-${row.target}`}
      loading={!status}
      className={className}
    />
  );
}

const roleWords: Record<Member["role"], { word: string; tone: Tone }> = {
  leader: { word: "Leader", tone: "plain" },
  follower: { word: "Follower", tone: "muted" },
  unreachable: { word: "Unreachable", tone: "bad" },
};

export function MembersTable({
  id,
  status,
  className,
}: {
  id: string;
  status: Status | null;
  className?: string;
}) {
  const cluster = status?.cluster;
  const columns = useMemo(() => {
    const list: ColumnDef<Member, never>[] = [
      {
        id: "member",
        header: "Member",
        size: 70,
        accessorFn: (row) => row.member as never,
        meta: { mono: true, required: true },
      },
      {
        id: "role",
        header: "Role",
        size: 100,
        accessorFn: (row) => row.role as never,
        cell: ({ row }) => {
          const { word, tone } = roleWords[row.original.role];
          return <Word tone={tone}>{word}</Word>;
        },
      },
      {
        id: "position",
        header: "Tape position",
        size: 116,
        accessorFn: (row) => (row.position ?? -1) as never,
        cell: ({ row }) =>
          row.original.position === undefined ? "" : bytes(row.original.position),
        meta: { align: "right", mono: true },
      },
      {
        id: "lag",
        header: "Lag",
        size: 84,
        accessorFn: (row) => (row.lag_bytes ?? -1) as never,
        cell: ({ row }) => {
          const lag = row.original.lag_bytes;
          if (lag === undefined) return "";
          return <Word tone={lag > 0 ? "warn" : "plain"}>{bytes(lag)}</Word>;
        },
        meta: { align: "right", mono: true },
      },
    ];
    return list;
  }, []);
  return (
    <DataTable
      id={id}
      title="Members"
      columns={columns}
      data={cluster?.members ?? []}
      rowId={(row) => String(row.member)}
      loading={!status}
      empty={
        cluster?.mode === "standalone"
          ? "Single engine, no cluster"
          : "No gateway can say who the members are"
      }
      className={className}
    />
  );
}

export function ConditionsTable({
  id,
  conditions,
  className,
}: {
  id: string;
  conditions: Condition[];
  className?: string;
}) {
  const columns = useMemo(() => {
    const list: ColumnDef<Condition, never>[] = [
      {
        id: "subject",
        header: "Subject",
        size: 210,
        accessorFn: (row) => row.subject as never,
        meta: { required: true },
      },
      {
        id: "check",
        header: "Check",
        size: 110,
        accessorFn: (row) => checkName(row.check) as never,
        cell: ({ row }) => (
          <Word tone={row.original.severity}>{checkName(row.original.check)}</Word>
        ),
      },
      {
        id: "since",
        header: "Failing since (UTC)",
        size: 182,
        accessorFn: (row) => row.since as never,
        cell: ({ row }) => utc(row.original.since),
        meta: { mono: true },
      },
      {
        id: "for",
        header: "For",
        size: 76,
        accessorFn: (row) => row.since as never,
        cell: ({ row }) => ago(row.original.since).replace(" ago", ""),
        meta: { align: "right" },
      },
      {
        id: "detail",
        header: "Detail",
        size: 360,
        accessorFn: (row) => row.detail as never,
      },
    ];
    return list;
  }, []);
  return (
    <DataTable
      id={id}
      title="Failed checks"
      columns={columns}
      data={conditions}
      rowId={(row) => `${row.subject}-${row.check}`}
      empty="All checks pass"
      className={className}
    />
  );
}
