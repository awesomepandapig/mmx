import { useState } from "react";
import { LIFECYCLE } from "./data";
import { CARD, H2, SECTION } from "./layout";

const TONES = {
  good: "text-mmx-mint",
  bad: "text-mmx-pink",
  pending: "text-mmx-gold",
};

export default function Lifecycle() {
  const [index, setIndex] = useState(0);
  const step = LIFECYCLE[index];

  return (
    <section id="fault-tolerance" className={SECTION}>
      <h2 className={`max-w-[24em] ${H2}`}>
        Fault-tolerant from order to fill.{" "}
        <span className="text-mmx-mute">
          Follow a trade as servers fail around it.
        </span>
      </h2>

      <ol className="mt-8 grid grid-cols-2 gap-2 lg:grid-cols-6">
        {LIFECYCLE.map(({ label, detail, failed }, i) => {
          const on = i === index;
          const tone = !on
            ? "border-mmx-line bg-mmx-surface hover:border-mmx-periwinkle"
            : failed
              ? "border-mmx-pink bg-mmx-pink text-mmx-ink"
              : "border-mmx-blue bg-mmx-blue";
          return (
            <li key={label}>
              <button
                type="button"
                aria-pressed={on}
                onClick={() => setIndex(i)}
                className={`block h-full w-full rounded-2xl border p-4 text-left font-medium transition-colors ${tone}`}
              >
                {label}
                <small
                  className={`block font-code text-xs font-normal ${on ? (failed ? "text-mmx-ink" : "text-[#d3d7ff]") : "text-mmx-mute"}`}
                >
                  {detail}
                </small>
              </button>
            </li>
          );
        })}
      </ol>

      <div className="mt-2 grid gap-2 lg:grid-cols-[1fr_1.35fr]">
        <div aria-live="polite" className={`${CARD} min-h-[196px]`}>
          <h3 className="font-display text-[30px] font-medium leading-[1.15] tracking-[-0.01em]">
            {step.title}
          </h3>
          <p className="mt-1.5 text-mmx-mute">{step.body}</p>
        </div>
        <div className="overflow-x-auto rounded-2xl border border-mmx-line bg-mmx-surface px-6 py-3 font-code text-[14.5px] leading-normal min-h-[196px]">
          {step.log.map(({ source, message, result, tone }, i) => (
            <div
              key={`${source}-${message}`}
              className={`grid grid-cols-[76px_1fr_auto] gap-3.5 whitespace-nowrap py-3 ${i ? "border-t border-[#20223a]" : ""}`}
            >
              <span className="text-[#8f92ad]">{source}</span>
              <span>{message}</span>
              <span className={TONES[tone]}>{result}</span>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
