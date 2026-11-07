import { useRef, useState } from "react";
import { MARKET_ITEMS, ORDER_EVENTS } from "./data";
import { useVisibleInterval } from "./useVisibleInterval";

const EVENT_COUNT = ORDER_EVENTS.length;
const DONE = EVENT_COUNT + 1;
const LAST_STEP = EVENT_COUNT + 3; // holds on the finished state before looping

/** One purchase plays through the MMX log and the game at the same time. */
export default function OrderDemo() {
  const stage = useRef<HTMLDivElement>(null);
  // Starts finished, so the page reads correctly before any animation runs.
  const [step, setStep] = useState(DONE);
  useVisibleInterval(stage, 800, () =>
    setStep((s) => (s >= LAST_STEP ? 0 : s + 1)),
  );

  const done = step >= DONE;

  return (
    <div
      ref={stage}
      className="grid gap-2 rounded-3xl bg-mmx-blue p-2 lg:grid-cols-2"
    >
      <section
        aria-label="Example market in your game"
        className="order-first flex min-w-0 flex-col overflow-hidden rounded-2xl bg-white text-mmx-ink lg:order-last lg:h-[470px]"
      >
        <p className="px-6 py-4 text-[13px] font-medium">Your game</p>
        <div className="mx-6 overflow-hidden rounded-xl border border-[#ececf1]">
          <div className="px-3.5 py-2.5 font-semibold">Market</div>
          <div className="grid grid-cols-[1fr_62px_62px_60px] border-t border-[#ececf1] px-3.5 py-1 text-xs text-[#6c6e7c]">
            <span>Item</span>
            <span>Bid</span>
            <span>Ask</span>
          </div>
          {MARKET_ITEMS.map(({ name, bid, ask }) => {
            const selected = name === "Iron ore";
            return (
              <div
                key={name}
                className={`grid grid-cols-[1fr_62px_62px_60px] items-center border-t border-[#ececf1] px-3.5 py-2 text-[15px] tabular-nums ${selected ? "bg-[#fff0f8]" : ""}`}
              >
                <span>{name}</span>
                <span>{bid}</span>
                <span>{ask}</span>
                <span
                  className={`rounded-md py-1 text-center text-[12.5px] font-medium transition-colors ${selected && step === 0 ? "bg-mmx-pink" : "bg-[#ececf1]"}`}
                >
                  Buy
                </span>
              </div>
            );
          })}
        </div>
        <div className="mx-6 mb-6 mt-auto flex flex-wrap items-center gap-2 pt-4 text-[15px] font-medium tabular-nums">
          <span className="rounded-lg bg-[#f1f1f5] px-3 py-1.5">
            Gold {done ? "1,450" : "2,400"}
          </span>
          <span className="rounded-lg bg-[#f1f1f5] px-3 py-1.5">
            Iron ore {done ? 100 : 0}
          </span>
          {done ? (
            <span className="ml-auto font-semibold">Bought 100 iron ore</span>
          ) : null}
        </div>
      </section>

      <section
        aria-label="MMX event log"
        className="flex h-[400px] min-w-0 flex-col overflow-hidden rounded-2xl bg-mmx-deep lg:h-[470px]"
      >
        <div className="flex justify-between px-6 py-4 text-[13px] font-medium">
          <span>Your infrastructure</span>
          <span className="hidden font-code text-xs font-normal text-[#8f92ad] min-[360px]:inline">
            mmx.yourstudio.internal
          </span>
        </div>
        <div className="grid flex-1 auto-rows-fr px-6 pb-2 font-code text-[13px] leading-snug lg:text-[15px]">
          {ORDER_EVENTS.map(({ service, color, message }, index) => (
            <div
              key={message}
              className={`grid grid-cols-[78px_1fr] items-center gap-2.5 border-t border-[#20223a] transition-opacity duration-200 ${index < step ? "opacity-100" : "opacity-[.34]"}`}
            >
              <b className="font-medium" style={{ color }}>
                {service}
              </b>
              <span className="min-w-0 text-[#dfe1f2] [overflow-wrap:anywhere]">
                {message}
              </span>
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
