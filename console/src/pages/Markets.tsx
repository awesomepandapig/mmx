import { useEffect, useMemo, useRef, useState } from "react";
import { Navigate, useNavigate, useSearchParams } from "react-router-dom";
import type { ColumnDef, SortingState } from "@tanstack/react-table";
import { Confirm } from "../components/Confirm";
import { DataTable } from "../components/DataTable";
import { lastMarket } from "../components/MarketPicker";
import { MarketsNav } from "../components/MarketsNav";
import { MarketDialogs, OutcomeNotice, editOutcome } from "../components/marketActions";
import type { MarketDialog, Outcome } from "../components/marketActions";
import {
  Button,
  ErrorState,
  Notice,
  Pager,
  SearchField,
  Segmented,
  SkeletonRows,
  Word,
} from "../components/ui";
import { ApiError, api, messageOf, queryString } from "../lib/api";
import type {
  Delivery,
  EditResult,
  Market,
  MarketCounts,
  MarketList,
  MarketsFile,
  Problem,
  Validation,
} from "../lib/api";
import { tradingState, useExchange } from "../lib/exchange";
import { ago, count, utc } from "../lib/format";
import { useDebounced, usePolled, useTitle } from "../lib/hooks";
import { useCan } from "../lib/session";

/** `/markets` is a market, the last one looked at, or the busiest if there isn't one. */
export function MarketsHome() {
  const [params] = useSearchParams();
  const pick = params.get("pick") ? "?pick=1" : "";
  const last = lastMarket();
  const busiest = usePolled(
    () => api.get<MarketList>("/markets?limit=1&sort=volume&dir=desc"),
    60000,
    last ?? "",
  );
  useTitle("Markets");
  if (last) return <Navigate to={`/markets/${last}${pick}`} replace />;
  const first = busiest.data?.markets[0];
  if (first) return <Navigate to={`/markets/${first.symbol}${pick}`} replace />;
  if (busiest.error) {
    return (
      <ErrorState
        title="Can't read markets"
        detail={busiest.error}
        onRetry={() => void busiest.refresh()}
      />
    );
  }
  if (busiest.data) return <Navigate to="/all-markets" replace />;
  return <SkeletonRows rows={8} />;
}

function DeliveryNotice({ delivery, onReload }: { delivery: Delivery; onReload?: () => void }) {
  if (delivery.state === "in_sync") return null;
  const reload = onReload && <Button onClick={onReload}>Reload gateways</Button>;
  const edge = "border-x-0 border-t-0";
  if (delivery.state === "reaching_gateways") {
    return (
      <Notice title="Saved, reaching gateways" action={reload} className={edge}>
        {delivery.waited_secs ?? 0}s of about {delivery.expected_secs ?? 90}s.
      </Notice>
    );
  }
  if (delivery.state === "stalled") {
    return (
      <Notice state="bad" title="Gateways haven't taken this file" action={reload} className={edge}>
        {delivery.waited_secs ?? 0}s. Behind: {delivery.behind.join(", ")}
      </Notice>
    );
  }
  return (
    <Notice
      state="warn"
      title="Gateways differ from the saved file"
      action={reload}
      className={edge}
    >
      Behind: {delivery.behind.join(", ")}
    </Notice>
  );
}

type Filter = "all" | "open" | "halted" | "disabled" | "out_of_sync";

function filters(counts?: MarketCounts): { value: Filter; label: string }[] {
  const label = (name: string, value?: number) =>
    value === undefined ? name : `${name} ${value.toLocaleString()}`;
  return [
    { value: "all", label: label("All", counts?.all) },
    { value: "open", label: label("Open", counts?.open) },
    { value: "halted", label: label("Halted", counts?.halted) },
    { value: "disabled", label: label("Disabled", counts?.disabled) },
    { value: "out_of_sync", label: label("Out of sync", counts?.out_of_sync) },
  ];
}

const SERVER_SORTS = new Set([
  "symbol",
  "state",
  "last",
  "volume",
  "trades",
  "spread",
  "open_orders",
  "last_trade",
]);

export function AllMarkets() {
  useTitle("All markets");
  const canAct = useCan("operator");
  const navigate = useNavigate();
  const { canTrade, status } = useExchange();
  const [params, setParams] = useSearchParams();
  const [dialog, setDialog] = useState<MarketDialog>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [stamp, setStamp] = useState(0);
  const [text, setText] = useState(params.get("q") ?? "");
  const q = useDebounced(text.trim(), 200);
  const state = (params.get("state") ?? "all") as Filter;
  const sort = params.get("sort") ?? "volume";
  const dir = params.get("dir") === "asc" ? "asc" : "desc";
  const limit = Number(params.get("limit")) || 50;
  const offset = Number(params.get("offset")) || 0;

  function change(next: Record<string, string | number>, resetPage = true) {
    const merged = new URLSearchParams(params);
    for (const [name, value] of Object.entries(next)) {
      if (value === "" || value === "all" || value === 0) merged.delete(name);
      else merged.set(name, String(value));
    }
    if (resetPage) merged.delete("offset");
    setParams(merged, { replace: true });
  }

  useEffect(() => {
    if ((params.get("q") ?? "") !== q) change({ q });
  }, [q]); // eslint-disable-line react-hooks/exhaustive-deps

  const query = queryString({
    q,
    state: state === "all" ? undefined : state,
    sort,
    dir,
    limit,
    offset,
  });
  const markets = usePolled(() => api.get<MarketList>(`/markets${query}`), 3000, query + stamp);
  const data = markets.data;
  const known = !!status;

  async function reload() {
    try {
      setOutcome(editOutcome(await api.post<EditResult>("/markets/reload")));
    } catch (error) {
      setOutcome({ state: "bad", title: "Reload failed", body: messageOf(error) });
    }
    void markets.refresh();
  }

  const sorting: SortingState = [{ id: sort, desc: dir === "desc" }];

  const columns = useMemo(() => {
    const number = (
      id: string,
      header: string,
      size: number,
      value: (market: Market) => string | number | undefined,
      sortable = true,
    ): ColumnDef<Market, never> => ({
      id,
      header,
      size,
      enableSorting: sortable,
      accessorFn: (market) => (value(market) ?? "") as never,
      cell: ({ getValue }) => {
        const shown = getValue() as string | number;
        return shown === "" ? <span className="text-faint">-</span> : shown;
      },
      meta: { mono: true, align: "right" },
    });
    const list: ColumnDef<Market, never>[] = [
      {
        id: "symbol",
        header: "Market",
        size: 96,
        accessorFn: (market) => market.symbol as never,
        cell: ({ row }) => <span className="font-medium text-accent">{row.original.symbol}</span>,
        meta: { mono: true, required: true },
      },
      {
        id: "state",
        header: "State",
        size: 130,
        accessorFn: (market) => tradingState(market, canTrade, known).word as never,
        cell: ({ row }) => {
          const { word, tone } = tradingState(row.original, canTrade, known);
          return <Word tone={tone === "plain" ? "muted" : tone}>{word}</Word>;
        },
      },
      number("last", "Last", 110, (market) => market.last),
      number("spread", "Spread", 90, (market) => market.spread),
      number("volume", "24h volume", 120, (market) => market.volume_24h),
      number("trades", "24h trades", 96, (market) =>
        market.trades_24h === undefined ? undefined : count(market.trades_24h),
      ),
      number("open_orders", "Open orders", 96, (market) =>
        market.open_orders === undefined ? undefined : count(market.open_orders),
      ),
      {
        id: "last_trade",
        header: "Last trade (UTC)",
        size: 186,
        accessorFn: (market) => (market.last_time ? utc(market.last_time) : "") as never,
        meta: { mono: true },
      },
      {
        id: "age",
        header: "Traded",
        size: 90,
        enableSorting: false,
        accessorFn: (market) => (market.last_time ? ago(market.last_time) : "") as never,
        meta: { align: "right", hidden: true },
      },
      number("tick", "Tick", 84, (market) => market.tick_size, false),
      number("min", "Min size", 90, (market) => market.min_size, false),
      number("max", "Max size", 100, (market) => market.max_size, false),
      {
        id: "gateways",
        header: "Gateways",
        size: 120,
        enableSorting: false,
        accessorFn: (market) =>
          `${market.gateways.filter((gateway) => gateway.in_sync).length} of ${market.gateways.length}` as never,
        cell: ({ row }) => {
          const { gateways } = row.original;
          const held = gateways.filter((gateway) => gateway.in_sync).length;
          return held === gateways.length ? (
            <span className="text-muted">
              {held} of {gateways.length} agree
            </span>
          ) : (
            <Word tone="warn">
              {held} of {gateways.length} agree
            </Word>
          );
        },
      },
      {
        id: "file",
        header: "In file",
        size: 70,
        enableSorting: false,
        accessorFn: (market) => (market.in_file ? "" : "Not in file") as never,
        cell: ({ getValue }) => <Word tone="warn">{getValue() as string}</Word>,
        meta: { hidden: true },
      },
      {
        id: "actions",
        header: "",
        size: 76,
        enableSorting: false,
        enableResizing: false,
        meta: { label: "Halt or resume" },
        cell: ({ row }) =>
          !canAct || (!row.original.enabled && !row.original.halted) ? null : (
            <Button
              tone={row.original.halted ? "plain" : "caution"}
              className="h-[22px] px-1.5 text-[12px]"
              onClick={(event) => {
                event.stopPropagation();
                setDialog({ kind: row.original.halted ? "resume" : "halt", market: row.original });
              }}
            >
              {row.original.halted ? "Resume" : "Halt"}
            </Button>
          ),
      },
    ];
    return list;
  }, [canAct, canTrade, known]);

  const broken = markets.failure instanceof ApiError && markets.failure.code === "markets_file";

  return (
    <div className="flex min-h-[480px] flex-1 flex-col lg:min-h-0">
      <MarketsNav />
      {data && (
        <DeliveryNotice
          delivery={data.delivery}
          onReload={canAct ? () => void reload() : undefined}
        />
      )}
      {outcome && (
        <div className="flex-none">
          <OutcomeNotice outcome={outcome} onClose={() => setOutcome(null)} />
        </div>
      )}
      {broken && (
        <Notice
          state="bad"
          title="The markets file doesn't parse"
          className="border-x-0 border-t-0"
        >
          {markets.error}
        </Notice>
      )}
      <DataTable
        id="all-markets"
        className="min-h-[360px] flex-1"
        columns={columns}
        data={data?.markets ?? []}
        rowId={(market) => market.symbol}
        onRowClick={(market) => navigate(`/markets/${market.symbol}`)}
        loading={!data && !markets.error}
        error={markets.error}
        onRetry={() => void markets.refresh()}
        sorting={sorting}
        onSorting={(updater) => {
          const next = typeof updater === "function" ? updater(sorting) : updater;
          const first = next[0];
          if (first && SERVER_SORTS.has(first.id)) {
            change({ sort: first.id, dir: first.desc ? "desc" : "asc" });
          }
        }}
        empty={q || state !== "all" ? "No markets match" : "No markets"}
        title={
          <>
            Markets{" "}
            <span className="tabular font-normal text-faint">
              ({(data?.total ?? 0).toLocaleString()})
            </span>
          </>
        }
        toolbar={
          <>
            <SearchField
              label="Search markets"
              className="w-44"
              placeholder="Find market"
              value={text}
              onChange={(event) => setText(event.target.value)}
            />
            <Segmented
              label="State"
              className="max-w-full overflow-x-auto"
              value={state}
              options={filters(data?.counts)}
              onChange={(value) => change({ state: value })}
            />
          </>
        }
        footer={
          <>
            {data && (
              <Pager
                offset={offset}
                limit={limit}
                total={data.total}
                onOffset={(next) => change({ offset: next }, false)}
                onLimit={(next) => change({ limit: next === 50 ? 0 : next })}
              />
            )}
            {canAct && (
              <Button tone="primary" onClick={() => setDialog({ kind: "new" })}>
                New market
              </Button>
            )}
          </>
        }
      />
      <MarketDialogs
        dialog={dialog}
        onClose={() => setDialog(null)}
        onDone={(notice) => {
          setDialog(null);
          // A save on its way has its own banner, which keeps counting until it lands.
          setOutcome(notice.title.endsWith("reaching gateways") ? null : notice);
          setStamp(stamp + 1);
        }}
      />
    </div>
  );
}

function SaveDialog({
  check,
  onClose,
  onSave,
}: {
  check: Validation;
  onClose: () => void;
  onSave: () => Promise<void>;
}) {
  const changes = check.changes;
  const lines = changes
    ? ([
        ["Adds", changes.added],
        ["Removes", changes.removed],
        ["Changes", changes.changed],
      ] as [string, string[]][])
    : [];
  const shown = lines.filter(([, symbols]) => symbols.length > 0);
  return (
    <Confirm
      title="Save markets?"
      action="Save and reload gateways"
      tone="primary"
      stepUp={!!changes?.removed.length}
      run={onSave}
      onClose={onClose}
      body={
        <div className="space-y-2">
          {shown.length === 0 ? (
            <p>Only comments or formatting differ.</p>
          ) : (
            <ul className="space-y-1">
              {shown.map(([verb, symbols]) => (
                <li key={verb}>
                  <span className="font-medium text-ink">{verb}: </span>
                  <span className="font-code">
                    {symbols.slice(0, 40).join(", ")}
                    {symbols.length > 40 ? ` and ${symbols.length - 40} more` : ""}
                  </span>
                </li>
              ))}
            </ul>
          )}
          {check.warnings?.map((warning) => (
            <Notice state="warn" key={warning}>
              {warning}
            </Notice>
          ))}
        </div>
      }
    />
  );
}

function YamlEditor({
  readOnly,
  onSaved,
}: {
  readOnly: boolean;
  onSaved: (notice: Outcome) => void;
}) {
  const [file, setFile] = useState<MarketsFile | null>(null);
  const [text, setText] = useState("");
  const [problem, setProblem] = useState<Problem | null>(null);
  const [check, setCheck] = useState<Validation | null>(null);
  const [checking, setChecking] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const gutter = useRef<HTMLPreElement>(null);

  async function load() {
    setFailure(null);
    setConflict(false);
    try {
      const next = await api.get<MarketsFile>("/config/markets");
      setFile(next);
      setText(next.yaml);
      setProblem(next.problem);
    } catch (error) {
      setFailure(messageOf(error));
    }
  }

  useEffect(() => {
    void load();
  }, []);

  // The server checks with the gateway's rules, so the editor can't disagree with what's saved.
  useEffect(() => {
    if (!file || readOnly) return;
    let stale = false;
    setChecking(true);
    const timer = setTimeout(() => {
      api
        .post<Validation>("/config/markets/validate", { yaml: text })
        .then((result) => {
          if (stale) return;
          setCheck(result);
          setProblem(result.problem ?? null);
        })
        .catch((error) => {
          if (!stale) setFailure(messageOf(error));
        })
        .finally(() => {
          if (!stale) setChecking(false);
        });
    }, 300);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  }, [text, file, readOnly]);

  async function save() {
    if (!file) return;
    try {
      const result = await api.put<EditResult>("/config/markets", {
        yaml: text,
        revision: file.revision,
      });
      setConfirming(false);
      onSaved(editOutcome(result));
      await load();
    } catch (error) {
      if (error instanceof ApiError && error.code === "reauth_required") throw error;
      setConfirming(false);
      if (error instanceof ApiError && error.code === "markets_changed") setConflict(true);
      setFailure(messageOf(error));
    }
  }

  if (!file) {
    return failure ? (
      <ErrorState
        title="Can't open the markets file"
        detail={failure}
        onRetry={() => void load()}
      />
    ) : (
      <SkeletonRows rows={8} />
    );
  }

  const lines = text.split("\n").length;
  const dirty = text !== file.yaml;
  const numbers = Array.from({ length: lines }, (_, index) => index + 1).join("\n");

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-2 p-2">
      <div className="flex min-h-[320px] flex-1 overflow-hidden border border-line bg-deep font-code text-[12px] leading-5 focus-within:border-accent">
        <pre
          ref={gutter}
          aria-hidden
          className="w-12 shrink-0 select-none overflow-hidden border-r border-hairline py-2 pr-2 text-right text-faint"
        >
          {numbers}
        </pre>
        <textarea
          aria-label="Markets file"
          spellCheck={false}
          wrap="off"
          readOnly={readOnly}
          value={text}
          onChange={(event) => setText(event.target.value)}
          onScroll={(event) => {
            if (gutter.current) gutter.current.scrollTop = event.currentTarget.scrollTop;
          }}
          className="h-full min-w-0 flex-1 resize-none overflow-auto bg-transparent px-2 py-2 focus:outline-none"
        />
      </div>

      {problem && (
        <Notice state="bad" title={problem.line ? `Line ${problem.line}` : "Not valid"}>
          {problem.message}
        </Notice>
      )}
      {failure && (
        <Notice
          state="bad"
          action={
            conflict ? <Button onClick={() => void load()}>Load the new copy</Button> : undefined
          }
        >
          {failure}
        </Notice>
      )}

      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="min-w-0 break-all font-code text-[12px] text-faint">{file.location}</p>
        {!readOnly && (
          <div className="flex gap-2">
            <Button disabled={!dirty} onClick={() => setText(file.yaml)}>
              Discard
            </Button>
            <Button
              tone="primary"
              disabled={!dirty || checking || !!problem || !check?.valid}
              onClick={() => setConfirming(true)}
            >
              Save
            </Button>
          </div>
        )}
      </div>

      {confirming && check && (
        <SaveDialog check={check} onClose={() => setConfirming(false)} onSave={save} />
      )}
    </div>
  );
}

export function MarketsFilePage() {
  useTitle("Markets file");
  const canAct = useCan("operator");
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  return (
    <div className="flex min-h-[480px] flex-1 flex-col lg:min-h-0">
      <MarketsNav />
      {outcome && <OutcomeNotice outcome={outcome} onClose={() => setOutcome(null)} />}
      <div className="flex min-h-0 flex-1 flex-col bg-surface">
        <YamlEditor readOnly={!canAct} onSaved={setOutcome} />
      </div>
    </div>
  );
}
