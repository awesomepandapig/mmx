import { useState } from "react";
import { useSize } from "../lib/hooks";
import { cn } from "./ui";

export interface Point {
  label: string;
  value: number;
  /** Shown beside the label on hover. */
  text: string;
}

/**
 * A single series over time, with a crosshair. Thin line, quiet grid, and the title around it names
 * the series. `step` holds each value until the next, which is honest for prices.
 */
export function LineChart({
  points,
  step = false,
  zero = false,
  ariaLabel,
  className,
  format,
  startLabel,
  endLabel,
}: {
  points: Point[];
  step?: boolean;
  /** Starts the axis at zero, for counts. */
  zero?: boolean;
  ariaLabel: string;
  className?: string;
  format: (value: number) => string;
  startLabel?: string;
  endLabel?: string;
}) {
  const [box, { width, height }] = useSize<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const pad = { top: 16, right: 0, bottom: 20 };
  const values = points.map((point) => point.value);
  const high = Math.max(...values);
  const low = zero ? 0 : Math.min(...values);
  const range = high - low || Math.abs(high) * 0.02 || 1;
  const x = (index: number) =>
    points.length < 2 ? width : (index / (points.length - 1)) * (width - pad.right);
  const y = (value: number) =>
    pad.top + (1 - (value - low) / range) * (height - pad.top - pad.bottom);

  const path = points.reduce(
    (line, point, index) =>
      index === 0
        ? `M${x(0)},${y(point.value)}`
        : step
          ? `${line}H${x(index)}V${y(point.value)}`
          : `${line}L${x(index)},${y(point.value)}`,
    "",
  );
  const last = points.length - 1;
  const hovered =
    hover === null || last < 0
      ? null
      : Math.max(0, Math.min(last, Math.round((hover / Math.max(1, width - pad.right)) * last)));

  return (
    <div
      ref={box}
      role="presentation"
      onPointerMove={(event) =>
        setHover(event.clientX - event.currentTarget.getBoundingClientRect().left)
      }
      onPointerLeave={() => setHover(null)}
      className={cn("relative min-h-0", className)}
    >
      {width > 0 && height > 0 && points.length > 0 && (
        <svg
          role="img"
          aria-label={ariaLabel}
          viewBox={`0 0 ${width} ${height}`}
          className="tabular absolute inset-0 h-full w-full"
        >
          {[low, low + range / 2, low + range].map((value) => (
            <g key={value}>
              <line
                x1="0"
                x2={width}
                y1={y(value)}
                y2={y(value)}
                className="stroke-hairline"
                strokeWidth="1"
              />
              <text
                x={width}
                y={y(value) - 4}
                fontSize="11"
                textAnchor="end"
                className="fill-faint"
              >
                {format(value)}
              </text>
            </g>
          ))}
          <path
            d={path}
            fill="none"
            className="stroke-accent"
            strokeWidth="2"
            strokeLinejoin="round"
            strokeLinecap="round"
          />
          <circle
            cx={x(last)}
            cy={y(points[last].value)}
            r="4"
            className="fill-accent stroke-surface"
            strokeWidth="2"
          />
          {startLabel && (
            <text x="0" y={height - 3} fontSize="11" className="fill-faint">
              {startLabel}
            </text>
          )}
          {endLabel && (
            <text x={width} y={height - 3} fontSize="11" textAnchor="end" className="fill-faint">
              {endLabel}
            </text>
          )}
          {hovered !== null && (
            <>
              <line
                x1={x(hovered)}
                x2={x(hovered)}
                y1={pad.top}
                y2={height - pad.bottom}
                className="stroke-faint"
                strokeOpacity=".5"
              />
              <circle
                cx={x(hovered)}
                cy={y(points[hovered].value)}
                r="4"
                className="fill-accent stroke-surface"
                strokeWidth="2"
              />
            </>
          )}
        </svg>
      )}
      {hovered !== null && (
        <div
          className="tabular pointer-events-none absolute top-0 whitespace-nowrap rounded-md bg-solid px-2 py-0.5 text-xs text-on-solid"
          style={{ left: Math.max(0, Math.min(x(hovered) + 8, width - 150)) }}
        >
          {points[hovered].text} · {points[hovered].label}
        </div>
      )}
    </div>
  );
}
