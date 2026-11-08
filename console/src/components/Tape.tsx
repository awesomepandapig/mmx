import { useMemo } from "react";
import { Link as RouterLink } from "react-router-dom";
import type { ColumnDef } from "@tanstack/react-table";
import type { TapeRow } from "../lib/api";
import { utc } from "../lib/format";
import { Button, Code, CopyButton, Word, buttonClass } from "./ui";

export function eventOf(row: TapeRow) {
  return row.kind === "trade" ? "trade" : (row.exec_type ?? "-");
}

export function SideText({ side }: { side: string }) {
  return <span className={side === "buy" ? "text-bid-text" : "text-ask-text"}>{side}</span>;
}

/** The width of a value in the table's 12px mono, plus padding, so it shows in full by default. */
const chars = (length: number) => Math.ceil(length * 7.25) + 18;

const reasonOf = (row: TapeRow) =>
  row.kind === "order" && row.reason && row.reason !== "none" ? row.reason : undefined;

const orderOnly = (row: TapeRow) => (row.kind === "order" ? row : null);

export function useTapeColumns(symbol: boolean) {
  return useMemo(() => {
    const columns: ColumnDef<TapeRow, never>[] = [
      {
        id: "time",
        header: "Time (UTC)",
        size: chars(23),
        accessorFn: (row) => row.time as never,
        cell: ({ row }) => utc(row.original.time),
        meta: { mono: true, required: true },
      },
      ...(symbol
        ? [
            {
              id: "symbol",
              header: "Market",
              size: chars(7),
              accessorFn: (row: TapeRow) => row.symbol as never,
              meta: { mono: true },
            } as ColumnDef<TapeRow, never>,
          ]
        : []),
      {
        id: "event",
        header: "Event",
        size: chars(9),
        accessorFn: (row) => eventOf(row) as never,
        cell: ({ row }) => {
          const event = eventOf(row.original);
          return <Word tone={event === "rejected" ? "bad" : "plain"}>{event}</Word>;
        },
        meta: { mono: true },
      },
      {
        id: "side",
        header: "Side",
        size: chars(5),
        accessorFn: (row) => row.side as never,
        cell: ({ row }) => <SideText side={row.original.side} />,
        meta: { mono: true },
      },
      {
        id: "status",
        header: "Status",
        size: chars(17),
        accessorFn: (row) => (reasonOf(row) ?? orderOnly(row)?.status ?? "") as never,
        meta: { mono: true },
      },
      {
        id: "size",
        header: "Size",
        size: chars(12),
        accessorFn: (row) => (row.size ?? "") as never,
        sortingFn: "alphanumeric",
        meta: { mono: true, align: "right" },
      },
      {
        id: "price",
        header: "Price",
        size: chars(12),
        accessorFn: (row) => (row.price ?? "market") as never,
        sortingFn: "alphanumeric",
        meta: { mono: true, align: "right" },
      },
      {
        id: "filled",
        header: "Filled",
        size: chars(12),
        accessorFn: (row) => (orderOnly(row)?.filled_size ?? "") as never,
        meta: { mono: true, align: "right" },
      },
      {
        id: "leaves",
        header: "Remaining",
        size: chars(12),
        accessorFn: (row) => (orderOnly(row)?.leaves_size ?? "") as never,
        meta: { mono: true, align: "right", hidden: true },
      },
      {
        id: "avg",
        header: "Avg price",
        size: chars(12),
        accessorFn: (row) => (orderOnly(row)?.avg_price ?? "") as never,
        meta: { mono: true, align: "right", hidden: true },
      },
      {
        id: "player",
        header: "Player",
        size: chars(36),
        accessorFn: (row) => (row.player ?? "") as never,
        meta: { mono: true },
      },
      {
        id: "client",
        header: "Client order ID",
        size: chars(36),
        accessorFn: (row) => (row.client_order_id ?? "") as never,
        meta: { mono: true },
      },
      {
        id: "order_id",
        header: "Order number",
        size: chars(13),
        accessorFn: (row) => (row.order_id ?? "") as never,
        meta: { mono: true, align: "right" },
      },
      {
        id: "exec_id",
        header: "Exec ID",
        size: chars(13),
        accessorFn: (row) => row.exec_id as never,
        meta: { mono: true, align: "right" },
      },
    ];
    return columns;
  }, [symbol]);
}

/** The whole of one row, beside the table so the table stays in view. */
export function RowDetail({
  row,
  onClose,
  onFilter,
}: {
  row: TapeRow;
  onClose: () => void;
  onFilter?: (name: "player" | "order", value: string) => void;
}) {
  const { kind: _kind, ...fields } = row;
  const json = JSON.stringify(fields, null, 2);
  const line = (label: string, value?: string | number | null) =>
    value === undefined || value === null || value === "" ? null : (
      <div key={label} className="py-0.5">
        <dt className="label">{label}</dt>
        <dd className="tabular">
          <Code>{String(value)}</Code>
        </dd>
      </div>
    );
  return (
    <aside
      aria-label="Event detail"
      className="flex min-h-0 w-full flex-none flex-col overflow-hidden border-l border-line bg-surface lg:w-[380px]"
    >
      <div className="flex h-9 flex-none items-center justify-between gap-2 border-b border-line px-3">
        <h2 className="truncate text-[15px] font-semibold">
          <span className="font-code">{row.symbol}</span> {eventOf(row)}
        </h2>
        <Button tone="quiet" onClick={onClose}>
          Close
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-3">
        <dl>
          {line("Time (UTC)", utc(row.time))}
          {line("Player", row.player)}
          {line("Client order ID", row.client_order_id)}
          {line("Reason", reasonOf(row))}
        </dl>
        <div className="mt-2 flex flex-wrap gap-1.5">
          {onFilter && row.player && (
            <Button onClick={() => onFilter("player", row.player ?? "")}>Filter by player</Button>
          )}
          {onFilter && row.client_order_id && (
            <Button onClick={() => onFilter("order", row.client_order_id ?? "")}>
              Filter by order
            </Button>
          )}
          <RouterLink to={`/markets/${row.symbol}`} className={buttonClass("plain")}>
            Open {row.symbol}
          </RouterLink>
        </div>
        <pre className="mt-3 overflow-x-auto bg-deep p-2.5 font-code text-[14px] leading-[18px]">
          {json}
        </pre>
        <div className="mt-1.5 flex justify-end">
          <CopyButton text={json} label="Copy JSON" />
        </div>
      </div>
    </aside>
  );
}
