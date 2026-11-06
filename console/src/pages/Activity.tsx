import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { MarketSearch } from "../components/MarketSearch";
import { SampleNote } from "../components/SampleNote";
import { RowDrawer, TAPE_GRID, TapeHeader, TapeLine } from "../components/Tape";
import { VirtualRows } from "../components/VirtualRows";
import {
  Button,
  Card,
  EmptyState,
  ErrorState,
  Notice,
  PageHeader,
  SearchField,
  Segmented,
  Select,
  SkeletonRows,
  cn,
} from "../components/ui";
import { api, queryString } from "../lib/api";
import type { OrderRow, TapeRow, TradeRow } from "../lib/api";
import { useDebounced, usePolled, useTitle } from "../lib/hooks";
import { rowKey, useTape } from "../lib/tape";
import type { TapeFilter } from "../lib/tape";

const ROW_HEIGHT = 32;
const PAGE = 50;

type Mode = "live" | "history";
type Table = "orders" | "trades";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function History({
  table,
  filter,
  onOpen,
}: {
  table: Table;
  filter: TapeFilter;
  onOpen: (row: TapeRow) => void;
}) {
  // Each page starts just past the last row of the one before, so paging stays cheap at any depth.
  const [cursors, setCursors] = useState<string[]>([]);
  const before = cursors[cursors.length - 1];
  const query = queryString({ ...filter, before, limit: PAGE });
  const page = usePolled<TapeRow[]>(
    async () => {
      if (table === "orders") {
        const body = await api.get<{ orders: OrderRow[] }>(`/orders${query}`);
        return body.orders.map((row) => ({ ...row, kind: "order" as const }));
      }
      const body = await api.get<{ trades: TradeRow[] }>(`/trades${query}`);
      return body.trades.map((row) => ({ ...row, kind: "trade" as const }));
    },
    60000,
    table + query,
  );

  const rows = page.data ?? [];
  if (page.error && !page.data) {
    return (
      <ErrorState
        title="Can't read the tape"
        detail={page.error}
        onRetry={() => void page.refresh()}
      />
    );
  }
  return (
    <>
      <div className="min-h-0 flex-1 overflow-auto">
        <TapeHeader />
        {page.loading ? (
          <SkeletonRows rows={10} />
        ) : rows.length === 0 ? (
          <EmptyState title={cursors.length ? "No older rows" : `No ${table} match`} />
        ) : (
          rows.map((row) => (
            <div key={rowKey(row)} style={{ height: ROW_HEIGHT }}>
              <TapeLine row={row} onOpen={() => onOpen(row)} />
            </div>
          ))
        )}
      </div>
      <div className="flex flex-none items-center justify-between border-t border-line px-4 py-2 text-[13px] text-muted">
        <span>Page {cursors.length + 1}</span>
        <div className="flex gap-2">
          <Button
            className="h-8 px-2.5 text-[13px]"
            disabled={cursors.length === 0}
            onClick={() => setCursors(cursors.slice(0, -1))}
          >
            Newer
          </Button>
          <Button
            className="h-8 px-2.5 text-[13px]"
            disabled={rows.length < PAGE}
            onClick={() => setCursors([...cursors, rows[rows.length - 1].time])}
          >
            Older
          </Button>
        </div>
      </div>
    </>
  );
}

export function Activity() {
  useTitle("Activity");
  const [params, setParams] = useSearchParams();
  const [mode, setMode] = useState<Mode>("live");
  const [table, setTable] = useState<Table>("orders");
  const [paused, setPaused] = useState(false);
  const [open, setOpen] = useState<TapeRow | null>(null);

  const symbol = params.get("symbol") ?? "";
  const side = params.get("side") ?? "";
  const event = params.get("event") ?? "";
  const [playerText, setPlayerText] = useState(params.get("player") ?? "");
  const [orderText, setOrderText] = useState(params.get("order") ?? "");
  const player = useDebounced(playerText.trim(), 300);
  const order = useDebounced(orderText.trim(), 300);

  function set(name: string, value: string) {
    const next = new URLSearchParams(params);
    if (value) next.set(name, value);
    else next.delete(name);
    setParams(next, { replace: true });
  }

  const badPlayer = !!player && !UUID.test(player);
  const badOrder = !!order && !UUID.test(order) && !/^\d+$/.test(order);
  const filter: TapeFilter = {
    symbol: symbol || undefined,
    side: side || undefined,
    event: event || undefined,
    player: player && !badPlayer ? player : undefined,
    order: order && !badOrder ? order : undefined,
  };
  const filtered = !!(symbol || side || event || player || order);

  const live = useTape(filter, paused || mode !== "live");
  const shown = live.rows.filter((row) => (table === "orders") === (row.kind === "order"));
  const problems = Object.entries(live.sources);

  return (
    <div className="flex min-h-[520px] flex-1 flex-col p-4 lg:p-6">
      <PageHeader
        title="Activity"
        detail={mode === "live" ? <SampleNote link={live.link} sample={live.sample} /> : undefined}
        actions={
          <>
            <Segmented
              label="Mode"
              value={mode}
              options={[
                { value: "live", label: "Live" },
                { value: "history", label: "History" },
              ]}
              onChange={setMode}
            />
            {mode === "live" && (
              <Button onClick={() => setPaused(!paused)}>{paused ? "Resume" : "Pause"}</Button>
            )}
          </>
        }
      />

      {problems.length > 0 && (
        <Notice state="warn" title="Part of the tape can't be read" className="mb-2">
          {problems.map(([source, error]) => (
            <p key={source} className="break-words">
              {source}: {error}
            </p>
          ))}
        </Notice>
      )}

      <Card className="flex min-h-0 flex-1 flex-col overflow-hidden">
        <div className="flex flex-none flex-wrap items-center gap-2 border-b border-line p-3">
          <Segmented
            label="Table"
            value={table}
            options={[
              { value: "orders", label: "Orders" },
              { value: "trades", label: "Trades" },
            ]}
            onChange={setTable}
          />
          <MarketSearch
            className="w-44"
            placeholder="Market"
            value={symbol}
            onPick={(next) => set("symbol", next)}
          />
          <Select label="Side" value={side} onChange={(e) => set("side", e.target.value)}>
            <option value="">Any side</option>
            <option value="buy">Buy</option>
            <option value="sell">Sell</option>
          </Select>
          <Select
            label="Event"
            value={event}
            disabled={table === "trades"}
            onChange={(e) => set("event", e.target.value)}
          >
            <option value="">Any event</option>
            {["new", "trade", "canceled", "rejected", "expired"].map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </Select>
          <SearchField
            label="Player"
            className="w-full sm:w-64"
            placeholder="Player UUID"
            aria-invalid={badPlayer}
            value={playerText}
            onChange={(e) => {
              setPlayerText(e.target.value);
              set("player", e.target.value.trim());
            }}
          />
          <SearchField
            label="Order"
            className="w-full sm:w-64"
            placeholder="Order UUID or number"
            aria-invalid={badOrder}
            value={orderText}
            onChange={(e) => {
              setOrderText(e.target.value);
              set("order", e.target.value.trim());
            }}
          />
          {filtered && (
            <Button
              tone="quiet"
              onClick={() => {
                setPlayerText("");
                setOrderText("");
                setParams({}, { replace: true });
              }}
            >
              Clear
            </Button>
          )}
          {(badPlayer || badOrder) && (
            <p className="w-full text-[13px] text-bad">
              {badPlayer ? "A player is a full UUID." : "An order is a UUID or a number."}
            </p>
          )}
        </div>

        {mode === "history" ? (
          <History
            key={table + queryString({ ...filter })}
            table={table}
            filter={filter}
            onOpen={setOpen}
          />
        ) : shown.length === 0 ? (
          <div className="flex-1">
            <div className={cn("hidden", TAPE_GRID)} />
            <EmptyState title={live.link === "live" ? `No ${table} yet` : "Waiting for the tape"}>
              {filtered ? "Nothing matches these filters so far." : undefined}
            </EmptyState>
          </div>
        ) : (
          <div className="flex min-h-0 flex-1 flex-col overflow-x-auto">
            <TapeHeader />
            <VirtualRows
              label={`Live ${table}`}
              rows={shown}
              height={ROW_HEIGHT}
              rowKey={rowKey}
              render={(row) => <TapeLine row={row} onOpen={() => setOpen(row)} />}
            />
          </div>
        )}
      </Card>

      <RowDrawer
        row={open}
        onClose={() => setOpen(null)}
        onFilter={(name, value) => {
          setOpen(null);
          if (name === "player") setPlayerText(value);
          else setOrderText(value);
          set(name, value);
        }}
      />
    </div>
  );
}
