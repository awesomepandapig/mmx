import { useEffect, useRef, useState } from "react";
import { type Market, money } from "./simulator";

const PADDING = { top: 20, right: 44, bottom: 20 };

/** Last traded price over the past three minutes. */
export default function PriceChart({ market }: { market: Market }) {
  const box = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [hover, setHover] = useState<number | null>(null);

  useEffect(() => {
    const element = box.current;
    if (!element) return;
    const observer = new ResizeObserver(() =>
      setSize({ width: element.clientWidth, height: element.clientHeight }),
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const { width, height } = size;
  const points = market.hist;
  const prices = points.map((point) => point.p);
  const low = Math.min(...prices);
  const range = Math.max(Math.max(...prices) - low, market.tick * 2);
  const x = (i: number) => (i / (points.length - 1)) * (width - PADDING.right);
  const y = (price: number) =>
    PADDING.top +
    (1 - (price - low) / range) * (height - PADDING.top - PADDING.bottom);

  const path = points.reduce(
    (line, point, i) =>
      i ? `${line}H${x(i)}V${y(point.p)}` : `M0,${y(point.p)}`,
    "",
  );
  const last = points.length - 1;
  const hovered =
    hover === null
      ? null
      : Math.max(
          0,
          Math.min(last, Math.round((hover / (width - PADDING.right)) * last)),
        );

  return (
    <div
      ref={box}
      onPointerMove={(event) =>
        setHover(
          event.clientX - event.currentTarget.getBoundingClientRect().left,
        )
      }
      onPointerLeave={() => setHover(null)}
      className="relative h-[170px] flex-none xl:h-auto xl:min-h-[120px] xl:flex-1"
    >
      {width > 0 ? (
        <svg
          role="img"
          aria-label={`${market.name} price, past three minutes`}
          viewBox={`0 0 ${width} ${height}`}
          className="absolute inset-0 h-full w-full tabular-nums"
        >
          {[
            low,
            Math.round((low + range / 2) / market.tick) * market.tick,
            low + range,
          ].map((value) => (
            <g key={value}>
              <line
                x1="0"
                x2={width}
                y1={y(value)}
                y2={y(value)}
                stroke="#ececf1"
              />
              <text
                x={width}
                y={y(value) - 6}
                fontSize="12"
                fill="#6c6e7c"
                textAnchor="end"
              >
                {money(value)}
              </text>
            </g>
          ))}
          <path
            d={path}
            fill="none"
            stroke="#2436f0"
            strokeWidth="2"
            strokeLinejoin="round"
          />
          <circle
            cx={x(last)}
            cy={y(points[last].p)}
            r="4"
            fill="#2436f0"
            stroke="#fff"
            strokeWidth="2"
          />
          <text x="0" y={height - 2} fontSize="12" fill="#6c6e7c">
            3 min ago
          </text>
          <text
            x={width}
            y={height - 2}
            fontSize="12"
            fill="#6c6e7c"
            textAnchor="end"
          >
            Now
          </text>
          {hovered !== null ? (
            <>
              <line
                x1={x(hovered)}
                x2={x(hovered)}
                y1={PADDING.top}
                y2={height - PADDING.bottom}
                stroke="#08090f"
                strokeOpacity=".3"
              />
              <circle
                cx={x(hovered)}
                cy={y(points[hovered].p)}
                r="4"
                fill="#2436f0"
                stroke="#fff"
                strokeWidth="2"
              />
            </>
          ) : null}
        </svg>
      ) : null}
      {hovered !== null ? (
        <div
          className="pointer-events-none absolute top-0 whitespace-nowrap rounded-md bg-mmx-ink px-[7px] py-[3px] text-xs tabular-nums text-white"
          style={{ left: Math.min(x(hovered) + 8, width - 130) }}
        >
          {money(points[hovered].p)} gold ·{" "}
          {new Date(points[hovered].t).toLocaleTimeString("en-GB")}
        </div>
      ) : null}
    </div>
  );
}
