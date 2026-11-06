import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { MarketDialogs, OutcomeNotice, editOutcome } from "../components/marketActions";
import type { MarketDialog, Outcome } from "../components/marketActions";
import { Confirm } from "../components/Confirm";
import {
  Button,
  Card,
  EmptyState,
  ErrorState,
  Notice,
  Page,
  PageHeader,
  Pager,
  SearchField,
  Segmented,
  SkeletonRows,
  SortHeader,
  Status,
  TableFrame,
  Tag,
  cn,
  tdClass,
  thClass,
} from "../components/ui";
import type { State } from "../components/ui";
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
import { useDebounced, usePolled, useTitle } from "../lib/hooks";
import { useCan } from "../lib/session";

export function marketState(market: Market): { state: State; text: string } {
  if (market.halted) return { state: "bad", text: "Halted" };
  if (!market.enabled) return { state: "idle", text: "Disabled" };
  return { state: "good", text: "Open" };
}

function DeliveryNotice({ delivery, onReload }: { delivery: Delivery; onReload?: () => void }) {
  if (delivery.state === "in_sync") return null;
  const reload = onReload && <Button onClick={onReload}>Reload gateways</Button>;
  if (delivery.state === "reaching_gateways") {
    return (
      <Notice title="Saved, reaching gateways" action={reload}>
        {delivery.waited_secs ?? 0}s of about {delivery.expected_secs ?? 90}s.
      </Notice>
    );
  }
  if (delivery.state === "stalled") {
    return (
      <Notice state="bad" title="Gateways haven't taken this file" action={reload}>
        {delivery.waited_secs ?? 0}s. Behind: {delivery.behind.join(", ")}
      </Notice>
    );
  }
  return (
    <Notice state="warn" title="Gateways differ from the saved file" action={reload}>
      Behind: {delivery.behind.join(", ")}
    </Notice>
  );
}

function MarketRow({
  market,
  canAct,
  onDialog,
}: {
  market: Market;
  canAct: boolean;
  onDialog: (dialog: MarketDialog) => void;
}) {
  const navigate = useNavigate();
  const { state, text } = marketState(market);
  const held = market.gateways.filter((gateway) => gateway.in_sync).length;
  const unreachable = market.gateways.filter((gateway) => !gateway.reachable).length;
  const halting = market.gateways.filter((gateway) => gateway.market?.halted).length;
  const partlyHalted = market.halted && halting < market.gateways.length;
  return (
    <tr
      className="cursor-pointer border-t border-hairline hover:bg-raised"
      onClick={() => navigate(`/markets/${market.symbol}`)}
    >
      <td className={tdClass}>
        <Link
          to={`/markets/${market.symbol}`}
          onClick={(event) => event.stopPropagation()}
          className="font-code font-medium hover:text-accent"
        >
          {market.symbol}
        </Link>
        {!market.in_file && <Tag className="ml-2">Not in file</Tag>}
      </td>
      <td className={tdClass}>
        <Status state={state}>{text}</Status>
        {partlyHalted && (
          <span className="ml-2 text-xs text-warn">
            {halting} of {market.gateways.length}
          </span>
        )}
      </td>
      <td className={`${tdClass} tabular font-code text-[12.5px]`}>{market.tick_size}</td>
      <td className={`${tdClass} tabular font-code text-[12.5px] text-muted`}>
        {market.min_size ?? "-"}
      </td>
      <td className={`${tdClass} tabular font-code text-[12.5px] text-muted`}>
        {market.max_size ?? "-"}
      </td>
      <td className={`${tdClass} tabular text-[13px]`}>
        <span className={held < market.gateways.length ? "text-warn" : "text-muted"}>
          {held} of {market.gateways.length} in sync
        </span>
        {unreachable > 0 && <span className="ml-2 text-bad">{unreachable} unreachable</span>}
      </td>
      <td className={`${tdClass} text-right`}>
        {canAct && (
          <Button
            className="h-8 px-2.5 text-[13px]"
            disabled={!market.enabled && !market.halted}
            onClick={(event) => {
              event.stopPropagation();
              onDialog({ kind: market.halted ? "resume" : "halt", market });
            }}
          >
            {market.halted ? "Resume" : "Halt"}
          </Button>
        )}
      </td>
    </tr>
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

function List({
  canAct,
  onDialog,
  outcome,
  onOutcome,
  onEditor,
  stamp,
}: {
  canAct: boolean;
  onDialog: (dialog: MarketDialog) => void;
  outcome: Outcome | null;
  onOutcome: (outcome: Outcome | null) => void;
  onEditor: () => void;
  stamp: number;
}) {
  const [params, setParams] = useSearchParams();
  const [text, setText] = useState(params.get("q") ?? "");
  const q = useDebounced(text.trim(), 200);
  const state = (params.get("state") ?? "all") as Filter;
  const sort = params.get("sort") === "state" ? "state" : "symbol";
  const dir = params.get("dir") === "desc" ? "desc" : "asc";
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
  const toggle = (name: "symbol" | "state") =>
    change({ sort: name, dir: sort === name && dir === "asc" ? "desc" : "asc" });

  async function reload() {
    try {
      onOutcome(editOutcome(await api.post<EditResult>("/markets/reload")));
    } catch (error) {
      onOutcome({ state: "bad", title: "Reload failed", body: messageOf(error) });
    }
    void markets.refresh();
  }

  const broken = markets.failure instanceof ApiError && markets.failure.code === "markets_file";
  const data = markets.data;
  const searching = !!q || state !== "all";

  return (
    <>
      <div className="mb-2 space-y-2 empty:hidden">
        {outcome && <OutcomeNotice outcome={outcome} onClose={() => onOutcome(null)} />}
        {data && (
          <DeliveryNotice
            delivery={data.delivery}
            onReload={canAct ? () => void reload() : undefined}
          />
        )}
      </div>
      <Card className="overflow-hidden">
        <div className="flex flex-wrap items-center gap-2 p-3">
          <SearchField
            label="Search markets"
            className="w-full sm:w-72"
            placeholder="Search by symbol"
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
        </div>
        {markets.error && !data ? (
          <ErrorState
            title={broken ? "The markets file doesn't parse" : "Can't read markets"}
            detail={markets.error}
            onRetry={broken ? undefined : () => void markets.refresh()}
          />
        ) : !data ? (
          <SkeletonRows rows={8} />
        ) : data.markets.length === 0 ? (
          searching ? (
            <EmptyState title="No markets match">
              {q ? `Nothing with "${q}" in this view.` : undefined}
            </EmptyState>
          ) : (
            <EmptyState
              title="No markets"
              action={
                canAct ? (
                  <Button tone="primary" onClick={() => onDialog({ kind: "new" })}>
                    New market
                  </Button>
                ) : undefined
              }
            />
          )
        ) : (
          <>
            {markets.error && (
              <p className="border-t border-line px-4 py-1.5 text-[13px] text-bad">
                {broken ? (
                  <button type="button" className="underline" onClick={onEditor}>
                    {markets.error}
                  </button>
                ) : (
                  markets.error
                )}
              </p>
            )}
            <TableFrame>
              <table className="w-full min-w-[820px] text-left text-sm">
                <thead>
                  <tr className="border-t border-line">
                    <SortHeader
                      label="Market"
                      active={sort === "symbol"}
                      dir={dir}
                      onClick={() => toggle("symbol")}
                    />
                    <SortHeader
                      label="State"
                      active={sort === "state"}
                      dir={dir}
                      onClick={() => toggle("state")}
                    />
                    <th className={thClass}>Tick</th>
                    <th className={thClass}>Min</th>
                    <th className={thClass}>Max</th>
                    <th className={thClass}>Gateways</th>
                    <th className={thClass}>
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.markets.map((market) => (
                    <MarketRow
                      key={market.symbol}
                      market={market}
                      canAct={canAct}
                      onDialog={onDialog}
                    />
                  ))}
                </tbody>
              </table>
            </TableFrame>
            <Pager
              offset={offset}
              limit={limit}
              total={data.total}
              onOffset={(next) => change({ offset: next }, false)}
              onLimit={(next) => change({ limit: next === 50 ? 0 : next })}
            />
          </>
        )}
      </Card>
    </>
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
    <div className="space-y-2 p-3">
      <div className="flex h-[min(60dvh,560px)] overflow-hidden rounded-xl border border-line bg-deep font-code text-[13px] leading-6 focus-within:border-accent">
        <pre
          ref={gutter}
          aria-hidden
          className="w-14 shrink-0 select-none overflow-hidden border-r border-hairline py-3 pr-2 text-right text-faint"
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
          className="h-full min-w-0 flex-1 resize-none overflow-auto bg-transparent px-3 py-3 focus:outline-none"
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
        <p className="min-w-0 break-all text-[13px] text-faint">{file.location}</p>
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

export function Markets() {
  useTitle("Markets");
  const canAct = useCan("operator");
  const [params, setParams] = useSearchParams();
  const view = params.get("view") === "file" ? "file" : "list";
  const [dialog, setDialog] = useState<MarketDialog>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [stamp, setStamp] = useState(0);

  function finish(notice: Outcome) {
    setDialog(null);
    // A save on its way has its own banner, which keeps counting until it lands.
    setOutcome(notice.title.endsWith("reaching gateways") ? null : notice);
    setStamp(stamp + 1);
  }

  function show(next: "list" | "file") {
    setParams(next === "file" ? { view: "file" } : {}, { replace: true });
  }

  return (
    <Page>
      <PageHeader
        title="Markets"
        actions={
          <>
            <Segmented
              label="View"
              value={view}
              options={[
                { value: "list", label: "List" },
                { value: "file", label: "File" },
              ]}
              onChange={show}
            />
            {canAct && (
              <Button tone="primary" onClick={() => setDialog({ kind: "new" })}>
                New market
              </Button>
            )}
          </>
        }
      />
      {view === "list" ? (
        <List
          canAct={canAct}
          onDialog={setDialog}
          outcome={outcome}
          onOutcome={setOutcome}
          onEditor={() => show("file")}
          stamp={stamp}
        />
      ) : (
        <>
          {outcome && (
            <div className={cn("mb-2")}>
              <OutcomeNotice outcome={outcome} onClose={() => setOutcome(null)} />
            </div>
          )}
          <Card>
            <YamlEditor readOnly={!canAct} onSaved={finish} />
          </Card>
        </>
      )}
      <MarketDialogs dialog={dialog} onClose={() => setDialog(null)} onDone={finish} />
    </Page>
  );
}
