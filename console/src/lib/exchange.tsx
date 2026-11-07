import { createContext, useContext, useMemo } from "react";
import type { ReactNode } from "react";
import { api } from "./api";
import type { Market, Status } from "./api";
import { usePolled } from "./hooks";

interface Exchange {
  status: Status | null;
  /** Why the last read failed, in which case `status` is what was last known. */
  error: string | null;
  /** When `status` was true, in Unix milliseconds. */
  asOf: number | null;
  /** False while there's no reason to believe orders can trade. */
  canTrade: boolean;
  refresh: () => Promise<void>;
}

const Context = createContext<Exchange | null>(null);

/** One reading for the whole console, so no two panels can disagree about the exchange. */
export function ExchangeProvider({ children }: { children: ReactNode }) {
  const polled = usePolled(() => api.get<Status>("/status"), 5000);
  const { data: status, error, refresh } = polled;
  const value = useMemo<Exchange>(
    () => ({
      status,
      error,
      asOf: status ? new Date(status.as_of).getTime() : null,
      canTrade: !!status?.can_trade && !error,
      refresh,
    }),
    [status, error, refresh],
  );
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useExchange() {
  const value = useContext(Context);
  if (!value) throw new Error("useExchange needs an ExchangeProvider");
  return value;
}

export type Tone = "plain" | "muted" | "warn" | "bad";

export interface Trading {
  word: string;
  tone: Tone;
  detail?: string;
}

/** What actually happens to an order sent to this market, which is the only state it has. */
export function tradingState(market: Market, canTrade: boolean, known = true): Trading {
  if (!market.enabled && !market.halted) return { word: "Disabled", tone: "muted" };
  if (known && !canTrade) return { word: "Cannot trade", tone: "bad" };
  if (market.halted) {
    const halting = market.gateways.filter((gateway) => gateway.market?.halted).length;
    return halting < market.gateways.length
      ? { word: `Halted on ${halting} of ${market.gateways.length} gateways`, tone: "bad" }
      : { word: "Halted", tone: "bad" };
  }
  return { word: "Open", tone: "plain" };
}
