import { useState } from "react";
import OrderBook from "./OrderBook";
import OrderTicket from "./OrderTicket";
import PriceChart from "./PriceChart";
import { type OrderType, type Simulator, money } from "./simulator";

const ORDER_COLUMNS =
  "grid grid-cols-[40px_minmax(0,1fr)_56px_48px_56px] items-center gap-2 px-4 sm:grid-cols-[48px_minmax(0,1fr)_64px_64px_64px_64px]";
const NUMBER_COLUMN = "text-right";
const FILLED_COLUMN = "hidden text-right sm:block";

function OpenOrders({ sim }: { sim: Simulator }) {
  return (
    <div className="h-[124px] flex-none overflow-auto border-t border-[#ececf1] pb-2 text-[13px] tabular-nums">
      <div
        className={`${ORDER_COLUMNS} sticky top-0 h-8 bg-white text-xs text-[#6c6e7c]`}
      >
        <span>Side</span>
        <span>Item</span>
        <span className={NUMBER_COLUMN}>Price</span>
        <span className={NUMBER_COLUMN}>Open</span>
        <span className={FILLED_COLUMN}>Filled</span>
        <span />
      </div>
      {sim.open.length ? (
        sim.open.map((order) => (
          <div key={order.id} className={`${ORDER_COLUMNS} h-7`}>
            <span>{order.side === "buy" ? "Buy" : "Sell"}</span>
            <span className="truncate">{sim.market(order.sym).name}</span>
            <span className={NUMBER_COLUMN}>{money(order.price)}</span>
            <span className={NUMBER_COLUMN}>{order.rem}</span>
            <span className={FILLED_COLUMN}>{order.size - order.rem}</span>
            <button
              type="button"
              onClick={() => sim.cancelOrder(order.id)}
              className="text-right text-[#6c6e7c] hover:text-mmx-ink"
            >
              Cancel
            </button>
          </div>
        ))
      ) : (
        <p className="px-4 text-[#6c6e7c]">
          No open orders. Limit orders that don&rsquo;t match rest here.
        </p>
      )}
    </div>
  );
}

export default function GamePanel({ sim }: { sim: Simulator }) {
  const [type, setType] = useState<OrderType>("limit");
  const [priceDraft, setPriceDraft] = useState<string | null>(null);

  const market = sim.market();
  const opening = market.hist[0].p;
  const change = ((market.last - opening) / opening) * 100;

  const selectMarket = (sym: string) => {
    sim.select(sym);
    setPriceDraft(null);
  };
  const pickBookPrice = (price: number) => {
    setType("limit");
    setPriceDraft(money(price));
  };

  return (
    <section
      aria-label="Sample game market"
      className="order-first flex min-w-0 flex-col overflow-hidden rounded-[14px] bg-white text-mmx-ink xl:order-last xl:h-[792px]"
    >
      <div className="flex min-h-[46px] flex-none items-center justify-between gap-3 px-4 py-3 text-[13px] font-medium">
        <span>Your game</span>
        <span
          role="status"
          className={`font-semibold ${sim.notice?.bad ? "text-mmx-ask" : ""}`}
        >
          {sim.notice?.text}
        </span>
      </div>

      <div className="flex flex-none justify-between gap-2 border-b border-[#ececf1] px-4 sm:grid sm:grid-cols-5 sm:gap-4">
        {sim.markets.map(({ sym, name, last }) => (
          <button
            key={sym}
            type="button"
            aria-pressed={sym === market.sym}
            onClick={() => selectMarket(sym)}
            className={`-mb-px min-w-0 border-b-2 pb-2 text-left ${sym === market.sym ? "border-mmx-pink" : "border-transparent hover:border-[#ececf1]"}`}
          >
            <b className="block whitespace-nowrap text-xs font-medium sm:text-[13px]">
              {name}
            </b>
            <span className="block text-xs tabular-nums text-[#6c6e7c]">
              {money(last)}
            </span>
          </button>
        ))}
      </div>

      <div className="grid flex-1 xl:min-h-0 xl:grid-cols-[minmax(0,1fr)_200px] xl:grid-rows-[minmax(0,1fr)]">
        <div className="flex min-w-0 flex-col gap-4 p-4 xl:min-h-0">
          <div className="flex flex-wrap items-baseline justify-between gap-x-4 tabular-nums">
            <div className="flex items-baseline gap-2">
              <b className="font-display text-2xl font-medium leading-none">
                {money(market.last)}
              </b>
              <span className="text-[13px] text-[#6c6e7c]">
                gold per {market.name.toLowerCase()}
              </span>
            </div>
            <span className="text-[13px] text-[#6c6e7c]">
              {change >= 0 ? "Up" : "Down"} {Math.abs(change).toFixed(1)}% in 3
              min
            </span>
          </div>
          <PriceChart market={market} />
          <OrderTicket
            sim={sim}
            type={type}
            onTypeChange={setType}
            priceDraft={priceDraft}
            onPriceChange={setPriceDraft}
          />
        </div>
        <OrderBook sim={sim} sym={market.sym} onPick={pickBookPrice} />
      </div>

      <OpenOrders sim={sim} />
    </section>
  );
}
