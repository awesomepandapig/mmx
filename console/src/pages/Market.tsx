import { useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { MarketDialogs, OutcomeNotice } from "../components/marketActions";
import type { MarketDialog, Outcome } from "../components/marketActions";
import { SampleNote } from "../components/SampleNote";
import { LineChart } from "../components/Sparkline";
import { RowDrawer, SideText, eventClass, eventOf } from "../components/Tape";
import { VirtualRows } from "../components/VirtualRows";
import {
  Button,
  EmptyState,
  ErrorState,
  Facts,
  Page,
  SearchField,
  Segmented,
  Select,
  Skeleton,
  Status,
  cn,
  buttonClass,
} from "../components/ui";
import { ApiError, BASE, api } from "../lib/api";
import type { BookFrame, Candle, Market, MarketHistory, TapeRow } from "../lib/api";
import { clock, short } from "../lib/format";
import { usePolled, useSize, useStream, useTitle } from "../lib/hooks";
import { useCan, useSession } from "../lib/session";
import { rowKey, useTape } from "../lib/tape";
import type { TapeFilter } from "../lib/tape";
import { marketState } from "./Markets";

const LOG_ROW = 28;
const BOOK_ROW = 22;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

type Range = "15m" | "1h" | "6h" | "24h";

function Stat({ label, value }: { label: string; value: string | null | undefined }) {
  return (
    <div className="min-w-[72px]">
      <p className="text-xs text-faint">{label}</p>
      <p className="tabular font-code text-sm">{value ?? "-"}</p>
    </div>
  );
}

function subtract(ask?: string, bid?: string) {
  if (!ask || !bid) return null;
  const places = Math.max((ask.split(".")[1] ?? "").length, (bid.split(".")[1] ?? "").length);
  return (Number(ask) - Number(bid)).toFixed(places);
}

function BookRow({
  price,
  size,
  width,
  side,
}: {
  price: string;
  size: string;
  width: number;
  side: "bid" | "ask";
}) {
  return (
    <div
      style={{ height: BOOK_ROW }}
      className="relative grid grid-cols-2 items-center px-4 font-code text-[12.5px]"
    >
      <i
        className={cn(
          "absolute inset-y-px right-0 rounded-l-sm opacity-[.22]",
          side === "bid" ? "bg-bid" : "bg-ask",
        )}
        style={{ width: `${width}%` }}
      />
      <span className={cn("relative", side === "bid" ? "text-bid-text" : "text-ask-text")}>
        {price}
      </span>
      <span className="relative text-right">{size}</span>
    </div>
  );
}

/** The largest size in view sets the scale, so depth reads at a glance without any numbers. */
function OrderBook({ frame, failed }: { frame: BookFrame | null; failed: string | null }) {
  const [box, { height }] = useSize<HTMLDivElement>();
  const rows = Math.max(3, Math.floor((height - 32) / 2 / BOOK_ROW));
  const asks = frame?.asks.slice(0, rows) ?? [];
  const bids = frame?.bids.slice(0, rows) ?? [];
  const sizes = [...asks, ...bids].map(([, size]) => Number(size));
  const largest = Math.max(...sizes, 1);
  const width = (size: string) => (Math.min(1, Number(size) / largest) * 56).toFixed(1);
  const spread = subtract(frame?.asks[0]?.[0], frame?.bids[0]?.[0]);
  const hidden = frame ? frame.bid_levels + frame.ask_levels - asks.length - bids.length : 0;

  return (
    <section aria-label="Order book" className="flex min-h-[340px] min-w-0 flex-1 flex-col">
      <div className="flex h-9 flex-none items-center justify-between px-4 text-[13px]">
        <span className="font-medium">Order book</span>
        {frame && (
          <span className="tabular text-faint">
            {hidden > 0
              ? `${asks.length + bids.length} of ${(frame.bid_levels + frame.ask_levels).toLocaleString()} levels`
              : `${frame.bid_levels + frame.ask_levels} levels`}
          </span>
        )}
      </div>
      {frame?.coverage === "partial" && (
        <p className="px-4 pb-1 text-xs text-warn">
          Can't read the tape, so this only shows levels that changed since you opened it.
        </p>
      )}
      <div ref={box} className="tabular flex min-h-0 flex-1 flex-col overflow-hidden">
        {!frame || frame.coverage === "loading" ? (
          failed ? (
            <p className="p-4 text-sm text-bad">{failed}</p>
          ) : (
            <div role="status" aria-label="Loading the book" className="space-y-1.5 p-4">
              {Array.from({ length: 8 }, (_, index) => (
                <Skeleton key={index} className="h-4 w-full" />
              ))}
            </div>
          )
        ) : frame.bids.length === 0 && frame.asks.length === 0 ? (
          <EmptyState title="The book is empty" />
        ) : (
          <>
            <div className="flex min-h-0 flex-1 flex-col justify-end overflow-hidden">
              {[...asks].reverse().map(([price, size]) => (
                <BookRow
                  key={price}
                  price={price}
                  size={size}
                  side="ask"
                  width={Number(width(size))}
                />
              ))}
            </div>
            <div className="flex h-8 flex-none items-center justify-between border-y border-hairline px-4 text-[13px] text-faint">
              <span>Spread</span>
              <span className="font-code text-ink">{spread ?? "-"}</span>
            </div>
            <div className="min-h-0 flex-1 overflow-hidden">
              {bids.map(([price, size]) => (
                <BookRow
                  key={price}
                  price={price}
                  size={size}
                  side="bid"
                  width={Number(width(size))}
                />
              ))}
            </div>
          </>
        )}
      </div>
    </section>
  );
}

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
  const points = useMemo(() => {
    const text = (candle: Candle) => ({
      label: new Date(candle.time).toLocaleTimeString("en-GB", {
        hour: "2-digit",
        minute: "2-digit",
      }),
      value: Number(candle.close),
      text: candle.close,
    });
    const shown = history?.candles.map(text) ?? [];
    if (live && shown.length) {
      shown.push({ label: "Now", value: Number(live), text: live });
    }
    return shown;
  }, [history, live]);

  return (
    <section aria-label="Price" className="flex h-[250px] flex-none flex-col p-4 pb-2">
      <div className="flex items-center justify-between gap-3">
        <span className="text-[13px] font-medium">Last price</span>
        <Segmented
          label="Range"
          value={range}
          onChange={onRange}
          options={(["15m", "1h", "6h", "24h"] as Range[]).map((value) => ({
            value,
            label: value,
          }))}
        />
      </div>
      {error && !history ? (
        <p className="py-6 text-sm text-bad">{error}</p>
      ) : !history ? (
        <Skeleton className="mt-3 flex-1" />
      ) : points.length === 0 ? (
        <EmptyState title={`No trades in the past ${range}`} />
      ) : (
        <LineChart
          className="mt-2 flex-1"
          step
          ariaLabel={`${symbol} last price, past ${range}`}
          format={(value) => String(Number(value.toFixed(4)))}
          points={points}
          startLabel={`${range} ago`}
          endLabel="Now"
        />
      )}
    </section>
  );
}

function Gateways({ market }: { market: Market }) {
  return (
    <section aria-label="Gateways" className="p-4">
      <p className="text-[13px] font-medium">Gateways</p>
      <ul className="mt-1.5 space-y-1.5">
        {market.gateways.map((gateway) => (
          <li key={gateway.gateway} className="text-[13px]">
            <p className="break-all font-code text-xs text-muted">{gateway.gateway}</p>
            {!gateway.reachable ? (
              <Status state="bad">Unreachable</Status>
            ) : gateway.market?.halted ? (
              <Status state="bad">Halted</Status>
            ) : gateway.in_sync ? (
              <Status state="good">In sync</Status>
            ) : (
              <Status state="warn">Out of sync</Status>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

function logMessage(row: TapeRow) {
  const price = row.price ?? "market";
  return `${row.size ?? "-"} @ ${price}  player ${short(row.player)}  order ${short(row.client_order_id)}`;
}

function LogLine({
  row,
  selected,
  onOpen,
}: {
  row: TapeRow;
  selected: boolean;
  onOpen: () => void;
}) {
  const event = eventOf(row);
  const reason = row.kind === "order" && row.reason && row.reason !== "none" ? row.reason : null;
  const result = reason ?? (row.kind === "order" ? row.status : "filled");
  return (
    <button
      type="button"
      onClick={onOpen}
      aria-pressed={selected}
      className={cn(
        "tabular grid h-full w-full grid-cols-[62px_58px_34px_1fr_auto] items-center gap-2 border-t border-hairline px-4 text-left font-code text-[12.5px] hover:bg-raised sm:grid-cols-[92px_62px_34px_1fr_auto]",
        selected && "bg-raised",
      )}
    >
      <time dateTime={row.time} className="truncate text-[11px] text-faint">
        {clock(row.time)}
      </time>
      <b className={cn("font-medium", eventClass(event))}>{event}</b>
      <SideText side={row.side} />
      <span className="min-w-0 truncate">{logMessage(row)}</span>
      <em className={cn("not-italic", reason ? "text-bad" : "text-faint")}>{result}</em>
    </button>
  );
}

function LogPanel({ symbol }: { symbol: string }) {
  const [side, setSide] = useState("");
  const [event, setEvent] = useState("");
  const [player, setPlayer] = useState("");
  const [order, setOrder] = useState("");
  const [paused, setPaused] = useState(false);
  const [open, setOpen] = useState<TapeRow | null>(null);

  const badPlayer = !!player.trim() && !UUID.test(player.trim());
  const orderText = order.trim();
  const badOrder = !!orderText && !UUID.test(orderText) && !/^\d+$/.test(orderText);
  const filter: TapeFilter = {
    symbol,
    side: side || undefined,
    event: event || undefined,
    player: !badPlayer ? player.trim() || undefined : undefined,
    order: !badOrder ? orderText || undefined : undefined,
  };
  const tape = useTape(filter, paused);
  const filtered = !!(side || event || player || order);

  return (
    <section aria-label="Event log" className="flex min-h-0 min-w-0 flex-1 flex-col bg-deep">
      <div className="flex flex-none flex-wrap items-center justify-between gap-2 px-4 py-2.5">
        <SampleNote link={tape.link} sample={tape.sample} />
        <Button className="h-8 px-2.5 text-[13px]" onClick={() => setPaused(!paused)}>
          {paused ? "Resume" : "Pause"}
        </Button>
      </div>
      <div className="flex flex-none flex-wrap items-center gap-2 px-4 pb-2.5">
        <Select label="Side" value={side} onChange={(e) => setSide(e.target.value)}>
          <option value="">Any side</option>
          <option value="buy">Buy</option>
          <option value="sell">Sell</option>
        </Select>
        <Select label="Event" value={event} onChange={(e) => setEvent(e.target.value)}>
          <option value="">Any event</option>
          {["new", "trade", "canceled", "rejected", "expired"].map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </Select>
        <SearchField
          label="Player"
          className="min-w-[170px] flex-1"
          placeholder="Player UUID"
          aria-invalid={badPlayer}
          value={player}
          onChange={(e) => setPlayer(e.target.value)}
        />
        <SearchField
          label="Order"
          className="min-w-[170px] flex-1"
          placeholder="Order UUID or number"
          aria-invalid={badOrder}
          value={order}
          onChange={(e) => setOrder(e.target.value)}
        />
        {filtered && (
          <Button
            tone="quiet"
            onClick={() => {
              setSide("");
              setEvent("");
              setPlayer("");
              setOrder("");
            }}
          >
            Clear
          </Button>
        )}
      </div>
      {Object.entries(tape.sources).map(([source, error]) => (
        <p key={source} className="break-words px-4 pb-2 text-[13px] text-bad">
          {source}: {error}
        </p>
      ))}
      {tape.rows.length === 0 ? (
        <EmptyState title={tape.link === "live" ? "No reports yet" : "Waiting for the tape"}>
          {filtered ? "Nothing matches these filters so far." : undefined}
        </EmptyState>
      ) : (
        <VirtualRows
          label="Events"
          rows={tape.rows}
          height={LOG_ROW}
          rowKey={rowKey}
          render={(row) => (
            <LogLine row={row} selected={open === row} onOpen={() => setOpen(row)} />
          )}
        />
      )}
      <RowDrawer
        row={open}
        onClose={() => setOpen(null)}
        onFilter={(name, value) => {
          setOpen(null);
          if (name === "player") setPlayer(value);
          else setOrder(value);
        }}
      />
    </section>
  );
}

function Header({
  market,
  history,
  book,
  last,
  canAct,
  onDialog,
}: {
  market: Market;
  history: MarketHistory | null;
  book: BookFrame | null;
  last: string | null;
  canAct: boolean;
  onDialog: (dialog: MarketDialog) => void;
}) {
  const { state, text } = marketState(market);
  const day = history?.day;
  return (
    <div className="flex flex-none flex-wrap items-center gap-x-6 gap-y-2 border-b border-line px-4 py-3 lg:px-6">
      <div className="flex items-baseline gap-2.5">
        <h1 className="font-display text-[28px] font-medium leading-8 tracking-heading">
          {market.symbol}
        </h1>
        <Status state={state}>{text}</Status>
      </div>
      <div className="flex flex-wrap gap-x-5 gap-y-1">
        <Stat label="Last" value={last ?? day?.last} />
        <Stat label="Bid" value={book?.bids[0]?.[0]} />
        <Stat label="Ask" value={book?.asks[0]?.[0]} />
        <Stat label="24h high" value={day?.high} />
        <Stat label="24h low" value={day?.low} />
        <Stat label="24h volume" value={day?.volume} />
        <Stat label="24h trades" value={day ? String(day.trades) : null} />
      </div>
      {canAct && (
        <div className="ml-auto flex flex-wrap gap-2">
          {market.halted ? (
            <Button tone="primary" onClick={() => onDialog({ kind: "resume", market })}>
              Resume
            </Button>
          ) : (
            <Button disabled={!market.enabled} onClick={() => onDialog({ kind: "halt", market })}>
              Halt
            </Button>
          )}
          <Button onClick={() => onDialog({ kind: "edit", market })}>Edit</Button>
          <Button tone="danger" onClick={() => onDialog({ kind: "cancel", market })}>
            Cancel all
          </Button>
          <Button
            tone="danger"
            disabled={!market.in_file}
            onClick={() => onDialog({ kind: "delete", market })}
          >
            Delete
          </Button>
        </div>
      )}
    </div>
  );
}

function Live({
  symbol,
  market,
  refresh,
}: {
  symbol: string;
  market: Market;
  refresh: () => void;
}) {
  const { load } = useSession();
  const canAct = useCan("operator");
  const [range, setRange] = useState<Range>("1h");
  const [book, setBook] = useState<BookFrame | null>(null);
  const [last, setLast] = useState<string | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [dialog, setDialog] = useState<MarketDialog>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [pane, setPane] = useState<"market" | "log">("market");

  useStream(
    `${BASE}/markets/${symbol}/stream`,
    {
      book: (frame: BookFrame) => {
        setBook(frame);
        setFailed(null);
      },
      ticker: (ticker: { price?: string }) => setLast(ticker.price ?? null),
      candle: () => {},
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
    <div className="flex min-h-[640px] flex-1 flex-col lg:min-h-0">
      <Header
        market={market}
        history={history.data}
        book={book}
        last={last}
        canAct={canAct}
        onDialog={setDialog}
      />
      {outcome && (
        <div className="flex-none px-4 pt-2 lg:px-6">
          <OutcomeNotice outcome={outcome} onClose={() => setOutcome(null)} />
        </div>
      )}
      <div className="flex-none p-2 lg:hidden">
        <Segmented
          label="Pane"
          value={pane}
          onChange={setPane}
          options={[
            { value: "market", label: "Market" },
            { value: "log", label: "Log" },
          ]}
        />
      </div>
      <div className="flex min-h-0 flex-1 lg:grid lg:grid-cols-2">
        <div
          className={cn(
            "min-h-0 min-w-0 flex-1 flex-col border-r border-line lg:flex",
            pane === "log" ? "flex" : "hidden",
          )}
        >
          <LogPanel symbol={symbol} />
        </div>
        <div
          className={cn(
            "min-h-0 min-w-0 flex-1 flex-col overflow-y-auto lg:flex",
            pane === "market" ? "flex" : "hidden",
          )}
        >
          <PriceChart
            symbol={symbol}
            range={range}
            onRange={setRange}
            history={history.data?.range === range ? history.data : null}
            error={history.error}
            live={last}
          />
          <div className="flex min-h-0 flex-1 flex-col border-t border-line xl:flex-row">
            <OrderBook frame={book} failed={failed} />
            <div className="flex-none border-t border-line xl:w-[260px] xl:border-l xl:border-t-0">
              <section aria-label="Limits" className="border-b border-line p-4">
                <Facts
                  className="tabular"
                  items={[
                    [
                      "Tick",
                      <span key="t" className="font-code text-[13px]">
                        {market.tick_size}
                      </span>,
                    ],
                    [
                      "Min size",
                      <span key="n" className="font-code text-[13px]">
                        {market.min_size ?? "-"}
                      </span>,
                    ],
                    [
                      "Max size",
                      <span key="x" className="font-code text-[13px]">
                        {market.max_size ?? "-"}
                      </span>,
                    ],
                  ]}
                />
              </section>
              <Gateways market={market} />
            </div>
          </div>
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
  const symbol = raw.toUpperCase();
  useTitle(symbol);
  const market = usePolled(() => api.get<Market>(`/markets/${symbol}`), 5000, symbol);
  const missing = market.failure instanceof ApiError && market.failure.status === 404;

  if (missing) {
    return (
      <Page>
        <EmptyState
          title={`No market ${symbol}`}
          action={
            <Link to="/markets" className={buttonClass()}>
              All markets
            </Link>
          }
        />
      </Page>
    );
  }
  if (!market.data) {
    return market.error ? (
      <Page>
        <ErrorState
          title={`Can't read ${symbol}`}
          detail={market.error}
          onRetry={() => void market.refresh()}
        />
      </Page>
    ) : (
      <div className="space-y-3 p-6" role="status" aria-label="Loading">
        <Skeleton className="h-9 w-48" />
        <Skeleton className="h-64 w-full rounded-2xl" />
      </div>
    );
  }
  return (
    <Live key={symbol} symbol={symbol} market={market.data} refresh={() => void market.refresh()} />
  );
}
