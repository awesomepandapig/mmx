import { useEffect, useId, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { useNavigate } from "react-router-dom";
import { api, messageOf, queryString } from "../lib/api";
import type { MarketList } from "../lib/api";
import { useDebounced } from "../lib/hooks";
import { Status, cn, inputClass, SearchIcon } from "./ui";
import type { State } from "./ui";

const SHOWN = 8;

interface Found {
  symbol: string;
  state: State;
  text: string;
}

function found(list: MarketList): Found[] {
  return list.markets.map((market) => ({
    symbol: market.symbol,
    state: market.halted ? "bad" : market.enabled ? "good" : "idle",
    text: market.halted ? "Halted" : market.enabled ? "Open" : "Disabled",
  }));
}

/**
 * Finds a market on the server, however many there are. With `onPick` it's a filter. Without, it
 * opens the market, and / or Cmd+K reach it from anywhere.
 */
export function MarketSearch({
  className,
  onPick,
  placeholder = "Search markets",
  value,
}: {
  className?: string;
  onPick?: (symbol: string) => void;
  placeholder?: string;
  value?: string;
}) {
  const navigate = useNavigate();
  const listId = useId();
  const input = useRef<HTMLInputElement>(null);
  const [text, setText] = useState("");
  const [open, setOpen] = useState(false);
  const [results, setResults] = useState<Found[] | null>(null);
  const [total, setTotal] = useState(0);
  const [failure, setFailure] = useState<string | null>(null);
  const [active, setActive] = useState(0);
  const needle = useDebounced(text.trim(), 150);
  const global = !onPick;

  useEffect(() => {
    if (!global) return;
    function keys(event: globalThis.KeyboardEvent) {
      const typing = /INPUT|TEXTAREA|SELECT/.test((event.target as HTMLElement).tagName);
      if (
        (event.key === "k" && (event.metaKey || event.ctrlKey)) ||
        (event.key === "/" && !typing)
      ) {
        event.preventDefault();
        input.current?.focus();
      }
    }
    window.addEventListener("keydown", keys);
    return () => window.removeEventListener("keydown", keys);
  }, [global]);

  useEffect(() => {
    if (!open) return;
    let stale = false;
    api
      .get<MarketList>(`/markets${queryString({ q: needle, limit: SHOWN })}`)
      .then((list) => {
        if (stale) return;
        setResults(found(list));
        setTotal(list.total);
        setFailure(null);
        setActive(0);
      })
      .catch((error) => {
        if (!stale) setFailure(messageOf(error));
      });
    return () => {
      stale = true;
    };
  }, [needle, open]);

  function pick(symbol: string) {
    setOpen(false);
    setText("");
    input.current?.blur();
    if (onPick) onPick(symbol);
    else navigate(`/markets/${symbol}`);
  }

  function onKey(event: KeyboardEvent) {
    const count = results?.length ?? 0;
    if (event.key === "ArrowDown" && count) {
      event.preventDefault();
      setActive((active + 1) % count);
    } else if (event.key === "ArrowUp" && count) {
      event.preventDefault();
      setActive((active - 1 + count) % count);
    } else if (event.key === "Enter" && results?.[active]) {
      event.preventDefault();
      pick(results[active].symbol);
    } else if (event.key === "Escape") {
      setOpen(false);
      input.current?.blur();
    }
  }

  return (
    <div className={cn("relative", className)}>
      <SearchIcon className="pointer-events-none absolute left-2.5 top-2.5 text-faint" />
      <input
        ref={input}
        role="combobox"
        aria-label={placeholder}
        aria-expanded={open}
        aria-controls={listId}
        autoComplete="off"
        spellCheck={false}
        placeholder={value || placeholder}
        value={text}
        onChange={(event) => {
          setText(event.target.value);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={onKey}
        className={cn(inputClass, "pl-8", value && "placeholder:text-ink")}
      />
      {open && (
        <div
          id={listId}
          role="listbox"
          aria-label="Markets"
          className="absolute left-0 right-0 top-10 z-30 min-w-[260px] overflow-hidden rounded-xl border border-line bg-surface py-1 shadow-2xl"
        >
          {failure && <p className="px-3 py-2 text-sm text-bad">{failure}</p>}
          {!failure && results?.length === 0 && (
            <p className="px-3 py-2 text-sm text-muted">No market matches {text.trim()}</p>
          )}
          {results?.map((market, index) => (
            <div
              key={market.symbol}
              role="option"
              tabIndex={-1}
              aria-selected={index === active}
              // Before the input's blur, which would close this first.
              onMouseDown={(event) => {
                event.preventDefault();
                pick(market.symbol);
              }}
              onMouseEnter={() => setActive(index)}
              className={cn(
                "flex cursor-pointer items-center justify-between px-3 py-1.5 text-sm",
                index === active && "bg-raised",
              )}
            >
              <span className="font-code font-medium">{market.symbol}</span>
              <Status state={market.state}>{market.text}</Status>
            </div>
          ))}
          {results && total > results.length && (
            <p className="border-t border-hairline px-3 py-1.5 text-xs text-faint">
              {results.length} of {total.toLocaleString()}
            </p>
          )}
        </div>
      )}
    </div>
  );
}
