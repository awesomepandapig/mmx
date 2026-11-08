import { useMemo, useState } from "react";
import { utcMinute } from "../lib/format";
import { useSize } from "../lib/hooks";
import { cn } from "./ui";

export interface Series {
  key: string;
  label: string;
  /** A Tailwind stroke class. Text next to a line never wears its colour. */
  stroke: string;
  fill: string;
}

export interface TimePoint {
  time: number;
  values: Record<string, number | null>;
}

const NICE = [1, 2, 2.5, 5, 10];

function niceStep(range: number, target: number) {
  const rough = range / Math.max(1, target);
  const power = 10 ** Math.floor(Math.log10(rough || 1));
  const step = NICE.find((nice) => nice * power >= rough) ?? 10;
  return step * power;
}

const TIME_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 180, 360, 720].map((minutes) => minutes * 60000);

/**
 * Lines over real time, in the anatomy every operator knows from Grafana: a clock on the x axis, a
 * unit on the y axis, and a legend carrying each series' current value. `step` holds each value
 * until the next, which is honest for prices.
 */
export function TimeChart({
  series,
  points,
  from,
  to,
  unit,
  step = false,
  zero = false,
  format,
  ariaLabel,
  className,
  legend = true,
}: {
  series: Series[];
  points: TimePoint[];
  from: number;
  to: number;
  unit: string;
  step?: boolean;
  zero?: boolean;
  format: (value: number) => string;
  ariaLabel: string;
  className?: string;
  legend?: boolean;
}) {
  const [box, { width, height }] = useSize<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const pad = { top: 8, right: 8, bottom: 18, left: 44 };
  const plotW = Math.max(1, width - pad.left - pad.right);
  const plotH = Math.max(1, height - pad.top - pad.bottom);

  const { low, high, ticks } = useMemo(() => {
    const values = points.flatMap((point) =>
      series
        .map((line) => point.values[line.key])
        .filter((value): value is number => value !== null && value !== undefined),
    );
    const top = values.length ? Math.max(...values) : 1;
    const bottom = zero ? 0 : values.length ? Math.min(...values) : 0;
    const spread = top - bottom || Math.abs(top) * 0.02 || 1;
    const size = niceStep(spread, 4);
    const low = zero ? 0 : Math.floor(bottom / size) * size;
    const high = Math.max(Math.ceil(top / size) * size, low + size);
    const ticks: number[] = [];
    for (let value = low; value <= high + size / 2; value += size) ticks.push(value);
    return { low, high, ticks };
  }, [points, series, zero]);

  const x = (time: number) => pad.left + ((time - from) / Math.max(1, to - from)) * plotW;
  const y = (value: number) => pad.top + (1 - (value - low) / (high - low || 1)) * plotH;

  const timeTicks = useMemo(() => {
    const span = to - from;
    const room = Math.max(2, Math.floor(plotW / 72));
    const size = TIME_STEPS.find((candidate) => span / candidate <= room) ?? 86400000;
    const out: number[] = [];
    for (let time = Math.ceil(from / size) * size; time <= to; time += size) out.push(time);
    return out;
  }, [from, to, plotW]);

  const path = (key: string) => {
    let line = "";
    let last: number | null = null;
    for (const point of points) {
      const value = point.values[key];
      if (value === null || value === undefined) continue;
      const px = x(point.time).toFixed(1);
      const py = y(value).toFixed(1);
      if (last === null) line += `M${px},${py}`;
      else line += step ? `H${px}V${py}` : `L${px},${py}`;
      last = value;
    }
    return line;
  };

  const nearest =
    hover === null || points.length === 0
      ? null
      : points.reduce((best, point) =>
          Math.abs(x(point.time) - hover) < Math.abs(x(best.time) - hover) ? point : best,
        );
  const latest = points[points.length - 1];

  return (
    <div className={cn("flex min-h-0 flex-col", className)}>
      <div
        ref={box}
        role="presentation"
        onPointerMove={(event) =>
          setHover(event.clientX - event.currentTarget.getBoundingClientRect().left)
        }
        onPointerLeave={() => setHover(null)}
        className="relative min-h-0 flex-1"
      >
        {width > 0 && height > 0 && (
          <svg
            role="img"
            aria-label={ariaLabel}
            viewBox={`0 0 ${width} ${height}`}
            className="tabular absolute inset-0 h-full w-full"
          >
            {ticks.map((tick) => (
              <g key={tick}>
                <line
                  x1={pad.left}
                  x2={width - pad.right}
                  y1={y(tick)}
                  y2={y(tick)}
                  className="stroke-hairline"
                />
                <text
                  x={pad.left - 6}
                  y={y(tick) + 3.5}
                  fontSize="10.5"
                  textAnchor="end"
                  className="fill-faint"
                >
                  {format(tick)}
                </text>
              </g>
            ))}
            <text x={2} y={pad.top + 3} fontSize="10.5" className="fill-faint">
              {unit}
            </text>
            {timeTicks.map((tick) => (
              <text
                key={tick}
                x={x(tick)}
                y={height - 5}
                fontSize="10.5"
                textAnchor="middle"
                className="fill-faint"
              >
                {utcMinute(tick)}
              </text>
            ))}
            {series.map((line) => (
              <path
                key={line.key}
                d={path(line.key)}
                fill="none"
                className={line.stroke}
                strokeWidth="1.5"
                strokeLinejoin="round"
                strokeLinecap="round"
              />
            ))}
            {nearest && (
              <>
                <line
                  x1={x(nearest.time)}
                  x2={x(nearest.time)}
                  y1={pad.top}
                  y2={height - pad.bottom}
                  className="stroke-faint"
                  strokeOpacity=".5"
                />
                {series.map((line) => {
                  const value = nearest.values[line.key];
                  return value === null || value === undefined ? null : (
                    <circle
                      key={line.key}
                      cx={x(nearest.time)}
                      cy={y(value)}
                      r="3"
                      className={cn(line.fill, "stroke-surface")}
                      strokeWidth="1.5"
                    />
                  );
                })}
              </>
            )}
          </svg>
        )}
        {nearest && (
          <div
            className="tabular pointer-events-none absolute top-1 z-10 whitespace-nowrap border border-line bg-surface px-2 py-1 text-[14px] shadow-lg"
            style={
              x(nearest.time) > width / 2
                ? { right: width - x(nearest.time) + 8 }
                : { left: x(nearest.time) + 8 }
            }
          >
            <p className="text-faint">{utcMinute(nearest.time)} UTC</p>
            {series.map((line) => {
              const value = nearest.values[line.key];
              return (
                <p key={line.key} className="flex items-center justify-between gap-4">
                  <span className="text-muted">{line.label}</span>
                  <span>{value === null || value === undefined ? "-" : format(value)}</span>
                </p>
              );
            })}
          </div>
        )}
      </div>
      {legend && series.length > 1 && (
        <ul className="tabular flex flex-none flex-wrap gap-x-4 px-3 pb-1.5 pt-0.5 text-[14px] text-muted">
          {series.map((line) => (
            <li key={line.key} className="flex items-center gap-1.5">
              <i aria-hidden className={cn("h-0.5 w-3", line.fill)} />
              {line.label}
              <span className="text-ink">
                {latest?.values[line.key] === null || latest?.values[line.key] === undefined
                  ? "-"
                  : format(latest.values[line.key] as number)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
