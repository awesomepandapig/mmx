import * as Popover from "@radix-ui/react-popover";
import { useCallback, useEffect, useRef, useState } from "react";
import type { KeyboardEvent, UIEvent } from "react";
import { api, messageOf, queryString } from "../lib/api";
import type { Market, MarketList } from "../lib/api";
import { useExchange, tradingState } from "../lib/exchange";
import { useDebounced } from "../lib/hooks";
import { SearchIcon, Word, cn } from "./ui";

const PAGE = 40;
/** Rows kept in the list. Past this, searching is quicker than scrolling. */
const MOST = 400;
const ROW = 28;
const RECENT_KEY = "mmx.market.recent";
const LAST_KEY = "mmx.market.last";

export function lastMarket(): string | null {
  try {
    return localStorage.getItem(LAST_KEY);
  } catch {
    return null;
  }
}

function recentMarkets(): string[] {
  try {
    return JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]") as string[];
  } catch {
    return [];
  }
}

export function rememberMarket(symbol: string) {
  try {
    localStorage.setItem(LAST_KEY, symbol);
    const recent = [symbol, ...recentMarkets().filter((name) => name !== symbol)].slice(0, 5);
    localStorage.setItem(RECENT_KEY, JSON.stringify(recent));
  } catch {
    // Without storage the console just doesn't remember.
  }
}

type View = "volume" | "halted";

function Row({
  market,
  active,
  onPick,
  onHover,
}: {
  market: Market;
  active: boolean;
  onPick: () => void;
  onHover: () => void;
}) {
  const { canTrade, status } = useExchange();
  const state = tradingState(market, canTrade, !!status);
  return (
    <div
      role="option"
      tabIndex={-1}
      aria-selected={active}
      onClick={onPick}
      onKeyDown={(event) => event.key === "Enter" && onPick()}
      onMouseMove={onHover}
      style={{ height: ROW }}
      className={cn(
        "grid cursor-pointer grid-cols-[72px_1fr_1fr_auto] items-center gap-2 px-3",
        active && "bg-raised",
      )}
    >
      <span className="font-code text-[14px] font-medium">{market.symbol}</span>
      <span className="tabular text-right font-code text-[14px] text-muted">
        {market.last ?? "-"}
      </span>
      <span className="tabular text-right font-code text-[14px] text-faint">
        {market.volume_24h ?? "-"}
      </span>
      <span className="w-[76px] truncate text-right text-[14px]">
        {state.word !== "Open" && <Word tone={state.tone}>{state.word}</Word>}
      </span>
    </div>
  );
}

/**
 * The switch between markets, like a cluster or namespace picker. The list stays on the server: it
 * reads a page at a time, busiest first, so it costs the same at 5 markets or 5,000.
 */
export function MarketPicker({
  value,
  onPick,
  startOpen = false,
}: {
  value: string;
  onPick: (symbol: string) => void;
  startOpen?: boolean;
}) {
  const [open, setOpen] = useState(startOpen);
  const [text, setText] = useState("");
  const [view, setView] = useState<View>("volume");
  const [rows, setRows] = useState<Market[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [halted, setHalted] = useState(0);
  const [recent, setRecent] = useState<Market[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [active, setActive] = useState(0);
  const loading = useRef(false);
  const list = useRef<HTMLDivElement>(null);
  const needle = useDebounced(text.trim(), 120);

  useEffect(() => {
    const show = () => setOpen(true);
    window.addEventListener("mmx-open-picker", show);
    return () => window.removeEventListener("mmx-open-picker", show);
  }, []);

  const read = useCallback(
    async (offset: number) => {
      const query = queryString({
        q: needle,
        limit: PAGE,
        offset,
        state: view === "halted" ? "halted" : undefined,
        sort: needle ? undefined : "volume",
        dir: needle ? undefined : "desc",
      });
      return api.get<MarketList>(`/markets${query}`);
    },
    [needle, view],
  );

  useEffect(() => {
    if (!open) return;
    let stale = false;
    read(0)
      .then((page) => {
        if (stale) return;
        setRows(page.markets);
        setTotal(page.total);
        setHalted(page.counts.halted);
        setFailure(null);
        setActive(0);
        list.current?.scrollTo(0, 0);
      })
      .catch((error) => {
        if (!stale) setFailure(messageOf(error));
      });
    return () => {
      stale = true;
    };
  }, [open, read]);

  useEffect(() => {
    if (!open) return;
    let stale = false;
    const names = recentMarkets().filter((name) => name !== value);
    void Promise.allSettled(names.map((name) => api.get<Market>(`/markets/${name}`))).then(
      (found) => {
        if (stale) return;
        setRecent(found.flatMap((result) => (result.status === "fulfilled" ? [result.value] : [])));
      },
    );
    return () => {
      stale = true;
    };
  }, [open, value]);

  async function more() {
    if (loading.current || total === null || rows.length >= Math.min(total, MOST)) return;
    loading.current = true;
    try {
      const page = await read(rows.length);
      setRows((current) => [...current, ...page.markets].slice(0, MOST));
    } catch (error) {
      setFailure(messageOf(error));
    } finally {
      loading.current = false;
    }
  }

  function onScroll(event: UIEvent<HTMLDivElement>) {
    const { scrollTop, scrollHeight, clientHeight } = event.currentTarget;
    if (scrollHeight - scrollTop - clientHeight < ROW * 6) void more();
  }

  const showRecent = !needle && view === "volume" && recent.length > 0;
  const options = [...(showRecent ? recent : []), ...rows];

  function pick(symbol: string) {
    setOpen(false);
    setText("");
    onPick(symbol);
  }

  function onKey(event: KeyboardEvent) {
    const size = options.length;
    if (event.key === "ArrowDown" && size) {
      event.preventDefault();
      setActive((active + 1) % size);
    } else if (event.key === "ArrowUp" && size) {
      event.preventDefault();
      setActive((active - 1 + size) % size);
    } else if (event.key === "Enter" && options[active]) {
      event.preventDefault();
      pick(options[active].symbol);
    }
  }

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          data-market-picker
          aria-label="Switch market"
          className="flex h-8 min-w-[132px] flex-none items-center justify-between gap-3 rounded-[3px] border border-line bg-canvas pl-2.5 pr-2 hover:border-faint"
        >
          <span className="font-code text-[17px] font-semibold">{value}</span>
          <svg aria-hidden viewBox="0 0 10 6" className="h-1.5 w-2.5 stroke-faint">
            <path d="m1 1 4 4 4-4" fill="none" strokeWidth="1.5" />
          </svg>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={4}
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            (event.currentTarget as HTMLElement).querySelector("input")?.focus();
          }}
          className="z-50 w-[min(420px,calc(100vw-1rem))] border border-line bg-surface text-ink shadow-2xl"
        >
          <div className="relative border-b border-line">
            <SearchIcon className="pointer-events-none absolute left-3 top-[11px] text-faint" />
            <input
              role="combobox"
              aria-label="Search markets"
              aria-expanded
              aria-controls="market-options"
              autoComplete="off"
              spellCheck={false}
              placeholder={
                total === null ? "Search markets" : `Search ${total.toLocaleString()} markets`
              }
              value={text}
              onChange={(event) => setText(event.target.value)}
              onKeyDown={onKey}
              className="h-9 w-full bg-transparent pl-8 pr-3 text-[15px] placeholder:text-faint focus:outline-none"
            />
          </div>
          {!needle && (
            <div className="flex items-center gap-3 border-b border-hairline px-3 text-[14px]">
              {(
                [
                  ["volume", "Busiest"],
                  ["halted", `Halted ${halted.toLocaleString()}`],
                ] as [View, string][]
              ).map(([name, label]) => (
                <button
                  key={name}
                  type="button"
                  aria-pressed={view === name}
                  onClick={() => setView(name)}
                  className={cn(
                    "tabular border-b-2 py-1 font-medium",
                    view === name
                      ? "border-accent text-ink"
                      : "border-transparent text-muted hover:text-ink",
                  )}
                >
                  {label}
                </button>
              ))}
              <span className="ml-auto text-faint">Last, 24h volume</span>
            </div>
          )}
          <div
            ref={list}
            id="market-options"
            role="listbox"
            aria-label="Markets"
            onScroll={onScroll}
            className="max-h-[364px] overflow-y-auto py-1"
          >
            {failure && <p className="px-3 py-2 text-bad">{failure}</p>}
            {showRecent && (
              <>
                <p className="label px-3 py-0.5">Recent</p>
                {recent.map((market, index) => (
                  <Row
                    key={`recent-${market.symbol}`}
                    market={market}
                    active={active === index}
                    onHover={() => setActive(index)}
                    onPick={() => pick(market.symbol)}
                  />
                ))}
                <p className="label px-3 py-0.5">All</p>
              </>
            )}
            {rows.map((market, index) => {
              const at = index + (showRecent ? recent.length : 0);
              return (
                <Row
                  key={market.symbol}
                  market={market}
                  active={active === at}
                  onHover={() => setActive(at)}
                  onPick={() => pick(market.symbol)}
                />
              );
            })}
            {!failure && total === 0 && (
              <p className="px-3 py-2 text-muted">No market matches {text.trim()}</p>
            )}
            {total !== null && rows.length >= MOST && rows.length < total && (
              <p className="px-3 py-1.5 text-[14px] text-faint">
                First {MOST} of {total.toLocaleString()}. Search to narrow.
              </p>
            )}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
