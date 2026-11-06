import { Link as RouterLink } from "react-router-dom";
import type { TapeRow } from "../lib/api";
import { clock, dateTime, short } from "../lib/format";
import { Button, Code, CopyButton, Drawer, cn } from "./ui";

export function eventOf(row: TapeRow) {
  return row.kind === "trade" ? "trade" : (row.exec_type ?? "-");
}

const eventColor: Record<string, string> = {
  new: "text-accent",
  trade: "text-good",
  canceled: "text-faint",
  rejected: "text-bad",
  expired: "text-faint",
};

export function eventClass(event: string) {
  return eventColor[event] ?? "text-muted";
}

export function SideText({ side }: { side: string }) {
  return <span className={side === "buy" ? "text-bid-text" : "text-ask-text"}>{side}</span>;
}

/** The columns of a tape table, kept here so a header and its rows can't drift apart. */
export const TAPE_GRID =
  "grid grid-cols-[104px_72px_48px_72px_128px_96px_96px_96px_80px_80px] items-center gap-x-3 px-4";
export const TAPE_MIN_WIDTH = "min-w-[992px]";

export function TapeHeader() {
  return (
    <div
      className={cn(TAPE_GRID, TAPE_MIN_WIDTH, "h-9 flex-none text-[13px] font-medium text-faint")}
    >
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
        <span key={name}>{name}</span>
      ))}
    </div>
  );
}

export function TapeLine({ row, onOpen }: { row: TapeRow; onOpen: () => void }) {
  const event = eventOf(row);
  const order = row.kind === "order" ? row : null;
  const reason = order?.reason && order.reason !== "none" ? order.reason : null;
  return (
    <button
      type="button"
      onClick={onOpen}
      className={cn(
        TAPE_GRID,
        TAPE_MIN_WIDTH,
        "tabular h-full w-full border-t border-hairline text-left font-code text-[12.5px] hover:bg-raised",
      )}
    >
      <span className="text-faint">{clock(row.time)}</span>
      <span>{row.symbol}</span>
      <SideText side={row.side} />
      <span className={eventClass(event)}>{event}</span>
      <span className={cn("truncate", reason ? "text-bad" : "text-muted")} title={reason ?? ""}>
        {reason ?? order?.status ?? "-"}
      </span>
      <span>{row.size ?? "-"}</span>
      <span>{row.price ?? "market"}</span>
      <span>{order?.filled_size ?? "-"}</span>
      <span className="text-faint">{short(row.player)}</span>
      <span className="text-faint">{short(row.client_order_id)}</span>
    </button>
  );
}

export function RowDrawer({
  row,
  onClose,
  onFilter,
}: {
  row: TapeRow | null;
  onClose: () => void;
  onFilter?: (name: "player" | "order", value: string) => void;
}) {
  if (!row) return null;
  const { kind: _kind, ...fields } = row;
  const json = JSON.stringify(fields, null, 2);
  return (
    <Drawer open onClose={onClose} title={`${eventOf(row)} on ${row.symbol}`}>
      <p className="text-sm text-muted">{dateTime(row.time)}</p>
      <dl className="mt-4 space-y-2 text-sm">
        {row.player && (
          <div>
            <dt className="text-faint">Player</dt>
            <dd>
              <Code>{row.player}</Code>
            </dd>
          </div>
        )}
        {row.client_order_id && (
          <div>
            <dt className="text-faint">Client order ID</dt>
            <dd>
              <Code>{row.client_order_id}</Code>
            </dd>
          </div>
        )}
      </dl>
      <div className="mt-4 flex flex-wrap gap-2">
        {onFilter && row.player && (
          <Button onClick={() => onFilter("player", row.player ?? "")}>Filter by player</Button>
        )}
        {onFilter && row.client_order_id && (
          <Button onClick={() => onFilter("order", row.client_order_id ?? "")}>
            Filter by order
          </Button>
        )}
        <RouterLink
          to={`/markets/${row.symbol}`}
          className="inline-flex h-9 items-center rounded-lg border border-line px-3.5 text-sm font-medium hover:border-accent"
        >
          Open {row.symbol}
        </RouterLink>
      </div>
      <pre className="mt-4 overflow-x-auto rounded-xl bg-deep p-4 font-code text-[12.5px] leading-normal">
        {json}
      </pre>
      <div className="mt-2 flex justify-end">
        <CopyButton text={json} label="Copy JSON" />
      </div>
    </Drawer>
  );
}
