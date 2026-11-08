import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { type Level, type Simulator, money } from "./simulator";

const MIN_ROW_HEIGHT = 23;
const SPARE_ROWS = 3; // sit just outside the box and are revealed while the ladder slides
const MAX_SLIDE = 3;
const MAX_BAR_WIDTH = 56; // percent of the row; bars stay clear of the price column

const useIsomorphicLayoutEffect =
  typeof document === "undefined" ? useEffect : useLayoutEffect;

function offset(before: Level[], after: Level[]) {
  const from = before.findIndex((level) =>
    after.some((next) => next.price === level.price),
  );
  if (from < 0) return 0;
  const to = after.findIndex((next) => next.price === before[from].price);
  return Math.max(-MAX_SLIDE, Math.min(MAX_SLIDE, to - from));
}

// Rows start where they were, then ease into their new place.
function slide(element: HTMLElement | null, rows: number, rowHeight: number) {
  if (!element || !rows) return;
  element.style.transition = "none";
  element.style.transform = `translateY(${rows * rowHeight}px)`;
  element.getBoundingClientRect();
  element.style.transition = "transform .18s ease-out";
  element.style.transform = "";
}

interface OrderBookProps {
  sim: Simulator;
  sym: string;
  onPick: (price: number) => void;
}

export default function OrderBook({ sim, sym, onPick }: OrderBookProps) {
  // Rows stretch slightly to fill the box exactly, so none is ever cut off at the edge.
  const [{ visible, rowHeight }, setFit] = useState({
    visible: 10,
    rowHeight: MIN_ROW_HEIGHT,
  });
  const bidBox = useRef<HTMLDivElement>(null);
  const askLadder = useRef<HTMLDivElement>(null);
  const bidLadder = useRef<HTMLDivElement>(null);
  const previous = useRef<{ sym: string; asks: Level[]; bids: Level[] } | null>(
    null,
  );

  const count = visible + SPARE_ROWS;
  const asks = sim.levels(sym, "sell", count);
  const bids = sim.levels(sym, "buy", count);
  const largest = Math.max(
    ...asks
      .slice(0, visible)
      .concat(bids.slice(0, visible))
      .map((level) => level.qty),
    1,
  );

  useEffect(() => {
    const box = bidBox.current;
    if (!box) return;
    const observer = new ResizeObserver(() => {
      const rows = Math.max(1, Math.floor(box.clientHeight / MIN_ROW_HEIGHT));
      setFit({ visible: rows, rowHeight: box.clientHeight / rows });
    });
    observer.observe(box);
    return () => observer.disconnect();
  }, []);

  useEffect(() => sim.setDepth(count), [sim, count]);

  useIsomorphicLayoutEffect(() => {
    const before = previous.current;
    previous.current = { sym, asks, bids };
    if (
      before?.sym !== sym ||
      matchMedia("(prefers-reduced-motion: reduce)").matches
    )
      return;
    slide(askLadder.current, offset(before.asks, asks), rowHeight);
    slide(bidLadder.current, -offset(before.bids, bids), rowHeight);
  });

  const row = (level: Level, side: "bid" | "ask") => (
    <button
      key={level.price}
      type="button"
      onClick={() => onPick(level.price)}
      style={{ height: rowHeight }}
      aria-label={`Use price ${money(level.price)}, ${level.qty} available`}
      className="relative grid w-full flex-none grid-cols-2 items-center px-4 text-left hover:bg-[#f1f1f5]"
    >
      <i
        className={`absolute inset-y-px right-0 rounded-l-sm opacity-[.14] ${side === "bid" ? "bg-mmx-bid" : "bg-mmx-ask"}`}
        style={{
          width: `${(Math.min(1, level.qty / largest) * MAX_BAR_WIDTH).toFixed(1)}%`,
        }}
      />
      <span className="relative">
        {money(level.price)}
        {level.mine ? (
          <span
            aria-label="Your order"
            className="ml-1.5 inline-block h-1.5 w-1.5 rounded-full bg-mmx-ink align-[2px]"
          />
        ) : null}
      </span>
      <span className="relative text-right">{level.qty}</span>
    </button>
  );

  return (
    <div
      aria-label="Order book"
      className="flex min-h-0 flex-col overflow-hidden border-t border-[#ececf1] text-[13px] tabular-nums xl:border-l xl:border-t-0"
    >
      <div className="grid h-8 flex-none grid-cols-2 items-center px-4 text-xs text-[#6c6e7c]">
        <span>Price</span>
        <span className="text-right">Quantity</span>
      </div>
      <div className="flex h-[184px] min-h-0 flex-none flex-col justify-end overflow-hidden xl:h-auto xl:flex-1 xl:basis-0">
        <div ref={askLadder} className="flex-none">
          {asks
            .slice()
            .reverse()
            .map((level) => row(level, "ask"))}
        </div>
      </div>
      <div className="flex h-8 flex-none items-center justify-between border-y border-[#ececf1] px-4 text-xs text-[#6c6e7c]">
        <span>Spread</span>
        <b className="font-medium text-mmx-ink">
          {money(asks[0].price - bids[0].price)}
        </b>
      </div>
      <div
        ref={bidBox}
        className="flex h-[184px] min-h-0 flex-none flex-col overflow-hidden xl:h-auto xl:flex-1 xl:basis-0"
      >
        <div ref={bidLadder} className="flex-none">
          {bids.map((level) => row(level, "bid"))}
        </div>
      </div>
    </div>
  );
}
