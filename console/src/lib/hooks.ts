import { useCallback, useEffect, useRef, useState } from "react";
import { messageOf } from "./api";

export interface Polled<T> {
  data: T | null;
  error: string | null;
  /** The error object, for callers that treat some failures differently. */
  failure: unknown;
  /** True until the first answer, good or bad. */
  loading: boolean;
  refresh: () => Promise<void>;
}

/** Whether this tab is on screen. Background tabs stop polling and close their streams. */
export function useVisible() {
  const [visible, setVisible] = useState(!document.hidden);
  useEffect(() => {
    const change = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", change);
    return () => document.removeEventListener("visibilitychange", change);
  }, []);
  return visible;
}

/**
 * Reads now and every `everyMs`, and again when `key` changes. A failed read keeps the last good
 * data, marked with the error. Answers to an older `key` are dropped.
 */
export function usePolled<T>(read: () => Promise<T>, everyMs: number, key = ""): Polled<T> {
  const [data, setData] = useState<T | null>(null);
  const [failure, setFailure] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);
  const visible = useVisible();
  const latest = useRef(read);
  latest.current = read;
  const ticket = useRef(0);

  const refresh = useCallback(async () => {
    const mine = ++ticket.current;
    try {
      const value = await latest.current();
      if (mine !== ticket.current) return;
      setData(value);
      setFailure(null);
    } catch (error) {
      if (mine === ticket.current) setFailure(error);
    } finally {
      if (mine === ticket.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!visible) return;
    void refresh();
    const timer = setInterval(() => void refresh(), everyMs);
    return () => clearInterval(timer);
  }, [refresh, everyMs, key, visible]);

  return { data, failure, loading, error: failure ? messageOf(failure) : null, refresh };
}

export function useDebounced<T>(value: T, ms = 200): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return settled;
}

export type Link = "connecting" | "live" | "lost" | "paused";

/**
 * A server-sent stream, closed while the tab is hidden. `handlers` maps event names to callbacks,
 * and can change between renders without reconnecting.
 */
export function useStream(
  url: string | null,
  handlers: Record<string, (data: never) => void>,
  onLost?: () => void,
): Link {
  const [link, setLink] = useState<Link>("connecting");
  const visible = useVisible();
  const latest = useRef(handlers);
  latest.current = handlers;
  const lost = useRef(onLost);
  lost.current = onLost;
  const names = Object.keys(handlers).sort().join(",");

  useEffect(() => {
    if (!url || !visible) {
      setLink("paused");
      return;
    }
    setLink("connecting");
    const stream = new EventSource(url);
    stream.onopen = () => setLink("live");
    // A stream can't show a 401, so the caller finds out whether the session is what ended.
    stream.onerror = () => {
      setLink("lost");
      lost.current?.();
    };
    for (const name of names.split(",")) {
      stream.addEventListener(name, (event) =>
        latest.current[name]?.(JSON.parse((event as MessageEvent).data) as never),
      );
    }
    return () => stream.close();
  }, [url, visible, names]);

  return link;
}

/** Keeps the element's size, for drawing to fit it. */
export function useSize<E extends HTMLElement>() {
  const ref = useRef<E>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    const observer = new ResizeObserver(() =>
      setSize({ width: element.clientWidth, height: element.clientHeight }),
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, size] as const;
}

export function useTitle(title: string) {
  useEffect(() => {
    document.title = `${title} - MMX`;
    return () => {
      document.title = "MMX console";
    };
  }, [title]);
}
