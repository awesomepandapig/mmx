import { useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { cn } from "./ui";

/**
 * Draws only the rows in view, so a long list costs what a short one does. Newest rows arrive at
 * the top: someone scrolled down keeps their place, and at the top they keep following.
 */
export function VirtualRows<T>({
  rows,
  height,
  rowKey,
  render,
  className,
  label,
}: {
  rows: T[];
  height: number;
  rowKey: (row: T) => string;
  render: (row: T) => ReactNode;
  className?: string;
  label: string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ top: 0, height: 600 });
  const newest = useRef<string | null>(null);

  useLayoutEffect(() => {
    const element = box.current;
    if (!element) return;
    const measure = () => setView({ top: element.scrollTop, height: element.clientHeight || 600 });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const element = box.current;
    const previous = newest.current;
    newest.current = rows[0] ? rowKey(rows[0]) : null;
    if (!element || !previous || element.scrollTop === 0) return;
    const added = rows.findIndex((row) => rowKey(row) === previous);
    if (added > 0) element.scrollTop += added * height;
  });

  const first = Math.max(0, Math.floor(view.top / height) - 8);
  const last = Math.min(rows.length, Math.ceil((view.top + view.height) / height) + 8);

  return (
    <div
      ref={box}
      role="log"
      aria-label={label}
      onScroll={(event) => {
        const { scrollTop, clientHeight } = event.currentTarget;
        setView({ top: scrollTop, height: clientHeight });
      }}
      className={cn("min-h-0 flex-1 overflow-auto", className)}
    >
      <div style={{ height: rows.length * height, position: "relative", minWidth: "100%" }}>
        <div style={{ transform: `translateY(${first * height}px)` }}>
          {rows.slice(first, last).map((row) => (
            <div key={rowKey(row)} style={{ height }}>
              {render(row)}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
