import { useCallback, useEffect, useRef, useState } from "react";
import { messageOf } from "./api";

export interface Polled<T> {
  data: T | null;
  error: string | null;
  /** The error object, for callers that treat some failures differently. */
  failure: unknown;
  refresh: () => Promise<void>;
}

/** Reads now and every `everyMs`. A failed read keeps the last good data, marked with the error. */
export function usePolled<T>(read: () => Promise<T>, everyMs: number): Polled<T> {
  const [data, setData] = useState<T | null>(null);
  const [failure, setFailure] = useState<unknown>(null);
  const latest = useRef(read);
  latest.current = read;

  const refresh = useCallback(async () => {
    try {
      setData(await latest.current());
      setFailure(null);
    } catch (error) {
      setFailure(error);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), everyMs);
    return () => clearInterval(timer);
  }, [refresh, everyMs]);

  return { data, failure, error: failure ? messageOf(failure) : null, refresh };
}
