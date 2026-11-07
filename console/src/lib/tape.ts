import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { BASE, queryString } from "./api";
import type { OrderRow, TapeRow, TradeRow } from "./api";
import { useStream } from "./hooks";
import type { Link } from "./hooks";
import { useSession } from "./session";

/** Rows held in the browser. Past this the oldest go. */
const KEEP = 2000;
/** How often the page redraws, however fast rows arrive. */
const FLUSH_MS = 250;
/** The window the shown rate is measured over. */
const WINDOW_MS = 5000;

export interface TapeFilter {
  symbol?: string;
  player?: string;
  side?: string;
  event?: string;
  order?: string;
}

export interface Sample {
  /** Rows matching per second, across orders and trades. */
  seen: number;
  /** Rows reaching this page per second. */
  sent: number;
}

export function rowKey(row: TapeRow) {
  return `${row.kind}-${row.exec_id}`;
}

/**
 * Follows the sampled tape. Rows wait in a buffer between redraws and only the newest `KEEP` stay,
 * so neither a busy market nor a long-open tab can slow the page down.
 */
export function useTape(filter: TapeFilter, paused: boolean) {
  const { load } = useSession();
  const [rows, setRows] = useState<TapeRow[]>([]);
  const [sample, setSample] = useState<Sample>({ seen: 0, sent: 0 });
  const [sources, setSources] = useState<Record<string, string>>({});
  const pending = useRef<TapeRow[]>([]);
  const samples = useRef<{ at: number; seen: number; sent: number }[]>([]);
  const held = useRef(paused);
  held.current = paused;

  const query = useMemo(
    () => queryString({ scope: "tape", ...filter }),
    [filter.symbol, filter.player, filter.side, filter.event, filter.order], // eslint-disable-line react-hooks/exhaustive-deps
  );

  useEffect(() => {
    setRows([]);
    pending.current = [];
    samples.current = [];
    setSample({ seen: 0, sent: 0 });
  }, [query]);

  useEffect(() => {
    const timer = setInterval(() => {
      const now = Date.now();
      samples.current = samples.current.filter((entry) => now - entry.at < WINDOW_MS);
      const add = (name: "seen" | "sent") =>
        samples.current.reduce((sum, entry) => sum + entry[name], 0) / (WINDOW_MS / 1000);
      setSample({ seen: add("seen"), sent: add("sent") });

      if (pending.current.length === 0 || held.current) return;
      const fresh = pending.current.reverse();
      pending.current = [];
      // Orders and trades arrive table by table, so the merged list is put back in time order.
      setRows((current) =>
        [...fresh, ...current]
          .sort((left, right) => (left.time < right.time ? 1 : left.time > right.time ? -1 : 0))
          .slice(0, KEEP),
      );
    }, FLUSH_MS);
    return () => clearInterval(timer);
  }, []);

  const push = useCallback((row: TapeRow) => {
    if (held.current) return;
    pending.current.push(row);
    if (pending.current.length > KEEP) pending.current.splice(0, pending.current.length - KEEP);
  }, []);

  const handlers = useMemo(
    () => ({
      ready: () => {},
      order: (row: OrderRow) => push({ ...row, kind: "order" }),
      trade: (row: TradeRow) => push({ ...row, kind: "trade" }),
      sample: ({ seen, sent }: { seen: number; sent: number }) => {
        samples.current.push({ at: Date.now(), seen, sent });
      },
      source_failed: ({ source, error }: { source: string; error: string }) =>
        setSources((now) => ({ ...now, [source]: error })),
      source_recovered: ({ source }: { source: string }) =>
        setSources(({ [source]: _gone, ...rest }) => rest),
    }),
    [push],
  );

  const link: Link = useStream(`${BASE}/events${query}`, handlers, () => void load());

  return { rows, sample, sources, link: paused ? ("paused" as const) : link };
}
