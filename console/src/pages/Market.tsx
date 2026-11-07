import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { DataTable } from "../components/DataTable";
import { MarketPicker, rememberMarket } from "../components/MarketPicker";
import { MarketsNav } from "../components/MarketsNav";
import { MarketDialogs, OutcomeNotice } from "../components/marketActions";
import type { MarketDialog, Outcome } from "../components/marketActions";
import { SampleNote } from "../components/SampleNote";
import { RowDetail, useTapeColumns } from "../components/Tape";
import { TimeChart } from "../components/TimeChart";
import type { TimePoint } from "../components/TimeChart";
import {
  Button,
  EmptyState,
  ErrorState,
  Facts,
  Notice,
  Panel,
  SearchField,
  Segmented,
  Skeleton,
  Word,
  buttonClass,
  cn,
} from "../components/ui";
import { ApiError, BASE, api } from "../lib/api";
import type { BookFrame, Market, MarketHistory, TapeRow } from "../lib/api";
import { tradingState, useExchange } from "../lib/exchange";
import { ago, count, priced, utcClock } from "../lib/format";
import { usePolled, useSize, useStream, useTitle } from "../lib/hooks";
import { useCan, useSession } from "../lib/session";
import { rowKey, useTape } from "../lib/tape";
import type { TapeFilter } from "../lib/tape";

const BOOK_ROW = 20;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

type Range = "15m" | "1h" | "6h" | "24h";
const RANGE_MS: Record<Range, number> = {
  "15m": 900000,
  "1h": 3600000,
  "6h": 21600000,
  "24h": 86400000,
};

function subtract(ask?: string, bid?: string) {
  if (!ask || !bid) return null;
  const places = Math.max((ask.split(".")[1] ?? "").length, (bid.split(".")[1] ?? "").length);
  return (Number(ask) - Number(bid)).toFixed(places);
}

function Stat({ label, value }: { label: string; value: string | null | undefined }) {
  return (
    <div className="min-w-[56px] flex-none">
      <p className="label whitespace-nowrap">{label}</p>
      <p className="tabular whitespace-nowrap font-code text-[14px]">{value ?? "-"}</p>
    </div>
  );
}

function BookRow({
  price,
  size,
  total,
  width,
  side,
}: {
  price: string;
  size: string;
  total: number;
  width: number;
  side: "bid" | "ask";
}) {
  return (
    <div
      style={{ height: BOOK_ROW }}
      className="relative grid grid-cols-[1fr_1fr_1fr] items-center gap-1 px-3 font-code text-[14px]"
    >
      <i
        className={cn(
          "absolute inset-y-px right-0 opacity-[.2]",
          side === "bid" ? "bg-bid" : "bg-ask",
        )}
        style={{ width: `${width}%` }}
      />
      <span className={cn("relative", side === "bid" ? "text-bid-text" : "text-ask-text")}>
        {price}
      </span>
      <span className="relative text-right">{size}</span>
      <span className="relative text-right text-faint">{Number(total.toFixed(4))}</span>
    </div>
  );
}

/** Depth is cumulative from the touch, which is what a large order would actually walk through. */
function OrderBook({
  frame,
  failed,
  tick,
  stale,
}: {
  frame: BookFrame | null;
  failed: string | null;
  tick: string;
  stale: boolean;
}) {
  const [box, { height }] = useSize<HTMLDivElement>();
  const rows = Math.max(3, Math.floor((height - 26) / 2 / BOOK_ROW));
  const asks = frame?.asks.slice(0, rows) ?? [];
  const bids = frame?.bids.slice(0, rows) ?? [];
  const sum = (levels: [string, string][]) => {
    let running = 0;
    return levels.map(([price, size]) => {
      running += Number(size);
      return { price, size, total: running };
    });
  };
  const askDepth = sum(asks);
  const bidDepth = sum(bids);
  const largest = Math.max(
    askDepth[askDepth.length - 1]?.total ?? 0,
    bidDepth[bidDepth.length - 1]?.total ?? 0,
    1,
  );
  const width = (total: number) => Math.min(1, total / largest) * 100;
  const spread = subtract(frame?.asks[0]?.[0], frame?.bids[0]?.[0]);
  const levels = frame ? frame.bid_levels + frame.ask_levels : 0;

  return (
    <Panel
      title="Order book"
      aside={
        frame && frame.coverage !== "loading" ? (
          <span className="tabular">{count(levels)} levels</span>
        ) : undefined
      }
      className="min-h-[320px]"
      bodyClassName="flex flex-col"
    >
      {frame?.coverage === "partial" && (
        <p className="px-3 py-1 text-[14px] text-warn">Levels changed since opening only.</p>
      )}
      <div
        ref={box}
        className={cn(
          "tabular flex min-h-0 flex-1 flex-col overflow-hidden",
          stale && "opacity-50",
        )}
      >
        {!frame || frame.coverage === "loading" ? (
          failed ? (
            <p className="p-3 text-bad">{failed}</p>
          ) : (
            <div role="status" aria-label="Loading the book" className="space-y-1.5 p-3">
              {Array.from({ length: 8 }, (_, index) => (
                <Skeleton key={index} className="h-3 w-full" />
              ))}
            </div>
          )
        ) : frame.bids.length === 0 && frame.asks.length === 0 ? (
          <EmptyState title="Book is empty" />
        ) : (
          <>
            <div className="flex min-h-0 flex-1 flex-col justify-end overflow-hidden">
              {[...askDepth].reverse().map(({ price, size, total }) => (
                <BookRow
                  key={price}
                  price={priced(price, tick)}
                  size={size}
                  total={total}
                  side="ask"
                  width={width(total)}
                />
              ))}
            </div>
            <div className="flex h-[26px] flex-none items-center justify-between border-y border-line bg-raised px-3 text-[14px]">
              <span className="text-faint">Spread</span>
              <span className="font-code">{spread ?? "-"}</span>
            </div>
            <div className="min-h-0 flex-1 overflow-hidden">
              {bidDepth.map(({ price, size, total }) => (
                <BookRow
                  key={price}
                  price={priced(price, tick)}
                  size={size}
                  total={total}
                  side="bid"
                  width={width(total)}
                />
              ))}
            </div>
          </>
        )}
      </div>
    </Panel>
  );
}

const PRICE_SERIES = [
  { key: "price", label: "Last", stroke: "stroke-accent", fill: "bg-accent fill-accent" },
];

function PriceChart({
  symbol,
  range,
  onRange,
  history,
  error,
  live,
}: {
  symbol: string;
  range: Range;
  onRange: (range: Range) => void;
  history: MarketHistory | null;
  error: string | null;
  live: string | null;
}) {
  const to = Date.now();
  const from = to - RANGE_MS[range];
  const points = useMemo(() => {
    const shown: TimePoint[] =
      history?.candles.map((candle) => ({
        time: new Date(candle.time).getTime(),
        values: { price: Number(candle.close) },
      })) ?? [];
    if (live && shown.length) shown.push({ time: to, values: { price: Number(live) } });
    return shown;
  }, [history, live]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <Panel
      title="Last price"
      aside={
        <Segmented
          label="Range"
          value={range}
          onChange={onRange}
          options={(["15m", "1h", "6h", "24h"] as Range[]).map((value) => ({
            value,
            label: value,
          }))}
        />
      }
      className="h-[240px] lg:h-auto"
      bodyClassName="flex flex-col"
    >
      {error && !history ? (
        <p className="p-3 text-bad">{error}</p>
      ) : !history ? (
        <Skeleton className="m-3 flex-1" />
      ) : points.length === 0 ? (
        <EmptyState title={`No trades in the past ${range}`} />
      ) : (
        <TimeChart
          className="flex-1"
          step
          series={PRICE_SERIES}
          points={points}
          from={from}
          to={to}
          unit=""
          format={(value) => String(Number(value.toFixed(4)))}
          ariaLabel={`${symbol} last price, past ${range}`}
        />
      )}
    </Panel>
  );
}

function Details({ market }: { market: Market }) {
  const disagree = market.gateways.filter((gateway) => !gateway.in_sync || !gateway.reachable);
  const code = (text?: string) => <span className="font-code text-[14px]">{text ?? "-"}</span>;
  return (
    <>
      <Panel title="Limits" className="flex-none" bodyClassName="p-3">
        <Facts
          items={[
            ["Tick", code(market.tick_size)],
            ["Min size", code(market.min_size)],
            ["Max size", code(market.max_size)],
            [
              "Open orders",
              code(market.open_orders === undefined ? undefined : count(market.open_orders)),
            ],
          ]}
        />
      </Panel>
      <Panel
        title="Gateways"
        aside={disagree.length === 0 ? <span>{market.gateways.length} agree</span> : undefined}
        className="flex-1"
        bodyClassName="overflow-y-auto"
      >
        <ul>
          {market.gateways.map((gateway) => (
            <li key={gateway.gateway} className="border-b border-hairline px-3 py-1">
              <p className="break-all font-code text-[14px] text-muted">{gateway.gateway}</p>
              {!gateway.reachable ? (
                <Word tone="bad">Unreachable</Word>
              ) : !gateway.in_sync ? (
                <Word tone="warn">Differs from the file</Word>
              ) : gateway.market?.halted !== market.halted ? (
                <Word tone="warn">
                  {gateway.market?.halted ? "Halted here" : "Not halted here"}
                </Word>
              ) : null}
            </li>
          ))}
        </ul>
      </Panel>
    </>
  );
}

function EventPanel({ symbol }: { symbol: string }) {
  const [view, setView] = useState<"events" | "trades">("events");
  const [player, setPlayer] = useState("");
  const [order, setOrder] = useState("");
  const [paused, setPaused] = useState(false);
  const [open, setOpen] = useState<TapeRow | null>(null);

  const badPlayer = !!player.trim() && !UUID.test(player.trim());
  const orderText = order.trim();
  const badOrder = !!orderText && !UUID.test(orderText) && !/^\d+$/.test(orderText);
  const filter: TapeFilter = {
    symbol,
    player: !badPlayer ? player.trim() || undefined : undefined,
    order: !badOrder ? orderText || undefined : undefined,
  };
  const tape = useTape(filter, paused);
  const columns = useTapeColumns(false);
  const rows = tape.rows.filter((row) => (view === "trades") === (row.kind === "trade"));

  return (
    <div className="flex min-h-[300px] min-w-0 flex-col lg:min-h-0">
      {Object.entries(tape.sources).map(([source, error]) => (
        <p key={source} className="break-words bg-surface px-3 py-1 text-[14px] text-bad">
          {source}: {error}
        </p>
      ))}
      <div className="flex min-h-0 flex-1">
        <DataTable
          id="market-events"
          className="min-w-0 flex-1"
          columns={columns}
          data={rows}
          rowId={rowKey}
          virtual
          selected={open ? rowKey(open) : null}
          onRowClick={setOpen}
          defaultSorting={[{ id: "time", desc: true }]}
          empty={tape.link === "live" ? "No events yet" : "Waiting for the tape"}
          title=""
          toolbar={
            <div className="flex flex-wrap items-center gap-2">
              <Segmented
                label="Show"
                value={view}
                onChange={setView}
                options={[
                  { value: "events", label: "Order events" },
                  { value: "trades", label: "Trades" },
                ]}
              />
              <SearchField
                label="Player"
                className="w-40"
                placeholder="Player UUID"
                aria-invalid={badPlayer}
                value={player}
                onChange={(event) => setPlayer(event.target.value)}
              />
              <SearchField
                label="Order"
                className="w-40"
                placeholder="Order UUID or number"
                aria-invalid={badOrder}
                value={order}
                onChange={(event) => setOrder(event.target.value)}
              />
              <Button className="h-[26px]" onClick={() => setPaused(!paused)}>
                {paused ? "Follow" : "Pause"}
              </Button>
              <SampleNote link={tape.link} sample={tape.sample} />
            </div>
          }
        />
        {open && (
          <RowDetail
            row={open}
            onClose={() => setOpen(null)}
            onFilter={(name, value) => {
              setOpen(null);
              if (name === "player") setPlayer(value);
              else setOrder(value);
            }}
          />
        )}
      </div>
    </div>
  );
}

function MoreActions({
  market,
  onDialog,
}: {
  market: Market;
  onDialog: (dialog: MarketDialog) => void;
}) {
  const item =
    "cursor-pointer px-3 py-1 outline-none data-[disabled]:pointer-events-none data-[disabled]:opacity-40 data-[highlighted]:bg-raised";
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <Button>More</Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          className="z-50 w-44 border border-line bg-surface py-1 text-[15px] text-ink shadow-xl"
        >
          <DropdownMenu.Item className={item} onSelect={() => onDialog({ kind: "edit", market })}>
            Edit limits
          </DropdownMenu.Item>
          <DropdownMenu.Separator className="my-1 h-px bg-hairline" />
          <DropdownMenu.Item
            className={cn(item, "text-bad")}
            onSelect={() => onDialog({ kind: "cancel", market })}
          >
            Cancel all orders
          </DropdownMenu.Item>
          <DropdownMenu.Item
            className={cn(item, "text-bad")}
            disabled={!market.in_file}
            onSelect={() => onDialog({ kind: "delete", market })}
          >
            Delete market
          </DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

function Header({
  market,
  history,
  book,
  last,
  canAct,
  onDialog,
  startOpen,
}: {
  market: Market;
  history: MarketHistory | null;
  book: BookFrame | null;
  last: string | null;
  canAct: boolean;
  onDialog: (dialog: MarketDialog) => void;
  startOpen: boolean;
}) {
  const navigate = useNavigate();
  const { canTrade, status } = useExchange();
  const state = tradingState(market, canTrade, !!status);
  const day = history?.day;
  const bid = book?.bids[0]?.[0];
  const ask = book?.asks[0]?.[0];
  return (
    <div className="flex flex-none flex-wrap items-center gap-x-5 gap-y-1.5 border-b border-line bg-surface px-3 py-1.5">
      <div className="flex flex-none items-center gap-2.5">
        <MarketPicker
          value={market.symbol}
          startOpen={startOpen}
          onPick={(symbol) => navigate(`/markets/${symbol}`)}
        />
        <Word tone={state.tone} className="whitespace-nowrap text-[15px]">
          {state.word}
        </Word>
      </div>
      <div className="flex min-w-0 flex-wrap gap-x-5 gap-y-1">
        <Stat label="Last" value={last ?? market.last ?? day?.last} />
        <Stat label="Bid" value={bid} />
        <Stat label="Ask" value={ask} />
        <Stat label="Spread" value={subtract(ask, bid)} />
        <Stat label="24h high" value={day?.high} />
        <Stat label="24h low" value={day?.low} />
        <Stat label="24h volume" value={day?.volume} />
        <Stat label="24h trades" value={day ? String(day.trades) : null} />
      </div>
      {canAct && (
        <div className="ml-auto flex flex-none items-center gap-2">
          <MoreActions market={market} onDialog={onDialog} />
          {market.halted ? (
            <Button tone="primary" onClick={() => onDialog({ kind: "resume", market })}>
              Resume trading
            </Button>
          ) : (
            <Button
              tone="danger"
              disabled={!market.enabled}
              onClick={() => onDialog({ kind: "halt", market })}
            >
              Halt market
            </Button>
          )}
        </div>
      )}
    </div>
  );
}

/** Old prices are still useful in an outage, as long as nobody takes them for current ones. */
function StaleNotice({ book, last }: { book: BookFrame | null; last?: string }) {
  const { canTrade, status, error } = useExchange();
  if (canTrade || !status) return null;
  const at = book?.as_of || last;
  return (
    <Notice state="warn" className="border-x-0 border-t-0 py-1">
      {at
        ? `Book and prices are as of ${utcClock(at)} UTC, ${ago(at)}. They are not current.`
        : "Book and prices are not current."}
      {error ? ` ${error}` : ""}
    </Notice>
  );
}

function Live({
  symbol,
  market,
  refresh,
  startOpen,
}: {
  symbol: string;
  market: Market;
  refresh: () => void;
  startOpen: boolean;
}) {
  const { load } = useSession();
  const { canTrade, status } = useExchange();
  const canAct = useCan("operator");
  const [range, setRange] = useState<Range>("1h");
  const [book, setBook] = useState<BookFrame | null>(null);
  const [last, setLast] = useState<string | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [dialog, setDialog] = useState<MarketDialog>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);

  useEffect(() => rememberMarket(symbol), [symbol]);

  useStream(
    `${BASE}/markets/${symbol}/stream`,
    {
      book: (frame: BookFrame) => {
        setBook(frame);
        setFailed(null);
      },
      ticker: (ticker: { price?: string }) => setLast(ticker.price ?? null),
      candle: () => {},
      ready: () => {},
      source_failed: ({ error }: { error: string }) => setFailed(error),
    },
    () => void load(),
  );

  const history = usePolled(
    () => api.get<MarketHistory>(`/markets/${symbol}/history?range=${range}`),
    30000,
    symbol + range,
  );

  return (
    <div className="flex flex-1 flex-col lg:min-h-0">
      <MarketsNav />
      <Header
        market={market}
        history={history.data}
        book={book}
        last={last}
        canAct={canAct}
        onDialog={setDialog}
        startOpen={startOpen}
      />
      <StaleNotice book={book} last={market.last_time} />
      {outcome && <OutcomeNotice outcome={outcome} onClose={() => setOutcome(null)} />}
      <div className="grid gap-px bg-line lg:min-h-0 lg:flex-1 lg:grid-cols-[minmax(250px,300px)_minmax(0,1fr)_minmax(200px,240px)]">
        <OrderBook
          frame={book}
          failed={failed}
          tick={market.tick_size}
          stale={!canTrade && !!status}
        />
        <div className="grid min-w-0 gap-px bg-line lg:min-h-0 lg:grid-rows-[minmax(180px,2fr)_minmax(0,3fr)]">
          <PriceChart
            symbol={symbol}
            range={range}
            onRange={setRange}
            history={history.data?.range === range ? history.data : null}
            error={history.error}
            live={last}
          />
          <EventPanel symbol={symbol} />
        </div>
        <div className="flex min-h-0 flex-col gap-px bg-line">
          <Details market={market} />
        </div>
      </div>
      <MarketDialogs
        dialog={dialog}
        onClose={() => setDialog(null)}
        onDone={(notice) => {
          setDialog(null);
          setOutcome(notice);
          refresh();
        }}
      />
    </div>
  );
}

export function MarketPage() {
  const { symbol: raw = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const symbol = raw.toUpperCase();
  useTitle(symbol);
  const market = usePolled(() => api.get<Market>(`/markets/${symbol}`), 5000, symbol);
  const missing = market.failure instanceof ApiError && market.failure.status === 404;
  const startOpen = !!params.get("pick");

  useEffect(() => {
    if (startOpen) setParams({}, { replace: true });
  }, [startOpen, setParams]);

  if (missing) {
    return (
      <>
        <MarketsNav />
        <EmptyState
          title={`No market ${symbol}`}
          action={
            <Link to="/all-markets" className={buttonClass()}>
              All markets
            </Link>
          }
        />
      </>
    );
  }
  if (!market.data || market.data.symbol !== symbol) {
    return market.error ? (
      <>
        <MarketsNav />
        <ErrorState
          title={`Can't read ${symbol}`}
          detail={market.error}
          onRetry={() => void market.refresh()}
        />
      </>
    ) : (
      <div className="flex flex-1 flex-col">
        <MarketsNav />
        <div role="status" aria-label="Loading" className="flex-1 space-y-2 p-3">
          <Skeleton className="h-8 w-64" />
          <Skeleton className="h-64 w-full" />
        </div>
      </div>
    );
  }
  return (
    <Live
      key={symbol}
      symbol={symbol}
      market={market.data}
      refresh={() => void market.refresh()}
      startOpen={startOpen}
    />
  );
}
