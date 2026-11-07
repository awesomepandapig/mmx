import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { DataTable } from "../components/DataTable";
import { SampleNote } from "../components/SampleNote";
import { RowDetail, useTapeColumns } from "../components/Tape";
import { Button, Notice, SearchField, Segmented, Select } from "../components/ui";
import { api, queryString } from "../lib/api";
import type { OrderRow, TapeRow, TradeRow } from "../lib/api";
import { useDebounced, usePolled, useTitle } from "../lib/hooks";
import { rowKey, useTape } from "../lib/tape";
import type { TapeFilter } from "../lib/tape";

const PAGE = 100;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

type Mode = "live" | "history";
type View = "all" | "new" | "trade" | "canceled" | "rejected";

const VIEWS: { value: View; label: string }[] = [
  { value: "all", label: "All events" },
  { value: "new", label: "New orders" },
  { value: "trade", label: "Trades" },
  { value: "canceled", label: "Cancels" },
  { value: "rejected", label: "Rejects" },
];

function useHistory(view: View, filter: TapeFilter, before: string | undefined, enabled: boolean) {
  const table = view === "trade" ? "trades" : "orders";
  const query = queryString({
    ...filter,
    event: table === "orders" && view !== "all" ? view : undefined,
    before,
    limit: PAGE,
  });
  return usePolled<TapeRow[]>(
    async () => {
      if (!enabled) return [];
      if (table === "orders") {
        const body = await api.get<{ orders: OrderRow[] }>(`/orders${query}`);
        return body.orders.map((row) => ({ ...row, kind: "order" as const }));
      }
      const body = await api.get<{ trades: TradeRow[] }>(`/trades${query}`);
      return body.trades.map((row) => ({ ...row, kind: "trade" as const }));
    },
    60000,
    table + query + String(enabled),
  );
}

export function Activity() {
  useTitle("Activity");
  const [params, setParams] = useSearchParams();
  const [mode, setMode] = useState<Mode>("live");
  const [paused, setPaused] = useState(false);
  const [open, setOpen] = useState<TapeRow | null>(null);
  const [cursors, setCursors] = useState<string[]>([]);

  const symbol = params.get("symbol") ?? "";
  const side = params.get("side") ?? "";
  const view = (params.get("view") ?? "all") as View;
  const [symbolText, setSymbolText] = useState(symbol);
  const [playerText, setPlayerText] = useState(params.get("player") ?? "");
  const [orderText, setOrderText] = useState(params.get("order") ?? "");
  const player = useDebounced(playerText.trim(), 300);
  const order = useDebounced(orderText.trim(), 300);
  const market = useDebounced(symbolText.trim().toUpperCase(), 300);

  function set(name: string, value: string) {
    const next = new URLSearchParams(params);
    if (value) next.set(name, value);
    else next.delete(name);
    setParams(next, { replace: true });
    setCursors([]);
  }

  const badPlayer = !!player && !UUID.test(player);
  const badOrder = !!order && !UUID.test(order) && !/^\d+$/.test(order);
  const filter: TapeFilter = {
    symbol: market || undefined,
    side: side || undefined,
    player: player && !badPlayer ? player : undefined,
    order: order && !badOrder ? order : undefined,
  };
  const filtered = !!(market || side || player || order);

  const live = useTape(
    { ...filter, event: view === "all" ? undefined : view },
    paused || mode !== "live",
  );
  const history = useHistory(view, filter, cursors[cursors.length - 1], mode === "history");
  const columns = useTapeColumns(true);

  const liveRows = live.rows.filter((row) =>
    view === "trade" ? row.kind === "trade" : row.kind === "order",
  );
  const rows = mode === "live" ? liveRows : (history.data ?? []);
  const problems = Object.entries(live.sources);

  return (
    <div className="flex min-h-[480px] flex-1 flex-col lg:min-h-0">
      {problems.length > 0 && (
        <Notice
          state="warn"
          title="Part of the tape can't be read"
          className="border-x-0 border-t-0"
        >
          {problems.map(([source, error]) => (
            <p key={source} className="break-words">
              {source}: {error}
            </p>
          ))}
        </Notice>
      )}
      <div className="flex flex-none flex-wrap items-center gap-2 border-b border-line bg-surface px-3 py-1.5">
        <h1 className="text-[17px] font-semibold">Activity</h1>
        <Segmented
          label="Show"
          value={view}
          options={VIEWS}
          onChange={(next) => {
            set("view", next === "all" ? "" : next);
          }}
        />
        <SearchField
          label="Market"
          className="w-24"
          placeholder="Market"
          value={symbolText}
          onChange={(event) => {
            setSymbolText(event.target.value);
            set("symbol", event.target.value.trim().toUpperCase());
          }}
        />
        <Select label="Side" value={side} onChange={(event) => set("side", event.target.value)}>
          <option value="">Any side</option>
          <option value="buy">Buy</option>
          <option value="sell">Sell</option>
        </Select>
        <SearchField
          label="Player"
          className="w-full sm:w-72"
          placeholder="Player UUID"
          aria-invalid={badPlayer}
          value={playerText}
          onChange={(event) => {
            setPlayerText(event.target.value);
            set("player", event.target.value.trim());
          }}
        />
        <SearchField
          label="Order"
          className="w-full sm:w-72"
          placeholder="Order UUID or number"
          aria-invalid={badOrder}
          value={orderText}
          onChange={(event) => {
            setOrderText(event.target.value);
            set("order", event.target.value.trim());
          }}
        />
        {filtered && (
          <Button
            tone="quiet"
            onClick={() => {
              setPlayerText("");
              setOrderText("");
              setSymbolText("");
              setParams({}, { replace: true });
              setCursors([]);
            }}
          >
            Clear
          </Button>
        )}
        {(badPlayer || badOrder) && (
          <p className="text-[14px] text-bad">
            {badPlayer ? "A player is a full UUID." : "An order is a UUID or a number."}
          </p>
        )}
      </div>
      <div className="flex min-h-0 flex-1 flex-col lg:flex-row">
        <DataTable
          id="activity"
          className="min-h-[320px] flex-1"
          columns={columns}
          data={rows}
          rowId={rowKey}
          virtual={mode === "live"}
          selected={open ? rowKey(open) : null}
          onRowClick={setOpen}
          loading={mode === "history" && history.loading}
          error={mode === "history" ? history.error : null}
          onRetry={() => void history.refresh()}
          defaultSorting={[{ id: "time", desc: true }]}
          empty={
            mode === "live" && live.link !== "live" ? "Waiting for the tape" : "No events match"
          }
          title=""
          toolbar={
            <div className="flex items-center gap-2">
              <Segmented
                label="Mode"
                value={mode}
                options={[
                  { value: "live", label: "Following" },
                  { value: "history", label: "History" },
                ]}
                onChange={(next) => {
                  setMode(next);
                  setCursors([]);
                }}
              />
              {mode === "live" ? (
                <>
                  <Button className="h-[26px]" onClick={() => setPaused(!paused)}>
                    {paused ? "Follow" : "Pause"}
                  </Button>
                  <SampleNote link={live.link} sample={live.sample} />
                </>
              ) : (
                <>
                  <Button
                    className="h-[26px]"
                    disabled={cursors.length === 0}
                    onClick={() => setCursors(cursors.slice(0, -1))}
                  >
                    Newer
                  </Button>
                  <Button
                    className="h-[26px]"
                    disabled={rows.length < PAGE}
                    onClick={() => setCursors([...cursors, rows[rows.length - 1].time])}
                  >
                    Older
                  </Button>
                  <span className="text-[14px] text-faint">Page {cursors.length + 1}</span>
                </>
              )}
            </div>
          }
        />
        {open && (
          <RowDetail
            row={open}
            onClose={() => setOpen(null)}
            onFilter={(name, value) => {
              setOpen(null);
              if (name === "player") setPlayerText(value);
              else setOrderText(value);
              set(name, value);
            }}
          />
        )}
      </div>
    </div>
  );
}
