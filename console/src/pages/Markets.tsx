import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  Button,
  Card,
  Field,
  Modal,
  Notice,
  PageHeader,
  Status,
  TableFrame,
  cn,
} from "../components/ui";
import type { State } from "../components/ui";
import { ApiError, api, messageOf } from "../lib/api";
import type {
  ActionResult,
  CancelResult,
  Delivery,
  EditResult,
  GatewayResult,
  Market,
  MarketList,
  MarketsFile,
  Problem,
  Validation,
} from "../lib/api";
import { usePolled } from "../lib/hooks";

type Tab = "list" | "yaml";

function marketState(market: Market): { state: State; text: string } {
  if (market.halted) return { state: "bad", text: "Halted" };
  if (!market.enabled) return { state: "idle", text: "Disabled" };
  return { state: "good", text: "Open" };
}

function failedGateways(gateways: GatewayResult[]) {
  return gateways.filter((gateway) => !gateway.ok);
}

function GatewayFailures({ gateways }: { gateways: GatewayResult[] }) {
  return (
    <ul className="mt-1 space-y-0.5">
      {failedGateways(gateways).map((gateway) => (
        <li key={gateway.gateway} className="break-words">
          {gateway.gateway}: {gateway.error}
        </li>
      ))}
    </ul>
  );
}

/** Says how far the stored file has got. A delay in Kubernetes is normal, so it isn't an error. */
function DeliveryNotice({ delivery, onReload }: { delivery: Delivery; onReload: () => void }) {
  if (delivery.state === "in_sync") return null;
  const reload = <Button onClick={onReload}>Reload gateways</Button>;
  if (delivery.state === "reaching_gateways") {
    return (
      <Notice state="idle" title="Saved, reaching gateways" action={reload}>
        Kubernetes takes up to about {delivery.expected_secs ?? 90} seconds to deliver a change to
        each gateway, and MMX reloads them as it lands. Waiting {delivery.waited_secs ?? 0}s so far.
      </Notice>
    );
  }
  if (delivery.state === "stalled") {
    return (
      <Notice state="bad" title="Gateways still don't have this" action={reload}>
        It's been {delivery.waited_secs ?? 0}s, which is longer than delivery should take. Behind:{" "}
        {delivery.behind.join(", ")}
      </Notice>
    );
  }
  return (
    <Notice state="warn" title="Gateways differ from the saved file" action={reload}>
      Behind: {delivery.behind.join(", ")}. If the file was edited by hand, reload to apply it.
    </Notice>
  );
}

type Dialog = { kind: "halt" | "resume" | "cancel"; market: Market } | { kind: "new" } | null;

function ActionDialog({
  dialog,
  onClose,
  onDone,
}: {
  dialog: Exclude<Dialog, null | { kind: "new" }>;
  onClose: () => void;
  onDone: (notice: Outcome) => void;
}) {
  const { kind, market } = dialog;
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  async function act() {
    setBusy(true);
    setFailure(null);
    try {
      if (kind === "cancel") {
        const result = await api.post<CancelResult>(`/markets/${market.symbol}/cancel-all`);
        const counts = `${result.requested} requested, ${result.canceled} canceled, ${result.failed} failed.`;
        onDone(
          result.complete
            ? { state: "good", title: `Canceled open orders in ${market.symbol}`, body: counts }
            : {
                state: "warn",
                title: `Some orders in ${market.symbol} may still be open`,
                body: `${counts}${result.truncated ? " More remain, so run it again." : ""}`,
                gateways: result.gateways,
              },
        );
      } else {
        const result = await api.post<ActionResult>(`/markets/${market.symbol}/${kind}`);
        const verb = kind === "halt" ? "Halted" : "Resumed";
        onDone(
          result.ok
            ? { state: "good", title: `${verb} ${market.symbol} on every gateway` }
            : {
                state: "bad",
                title: `${market.symbol} isn't ${verb.toLowerCase()} on every gateway`,
                gateways: result.gateways,
              },
        );
      }
    } catch (error) {
      setFailure(messageOf(error));
      setBusy(false);
    }
  }

  const copy = {
    halt: {
      title: `Halt ${market.symbol}?`,
      body: "Every gateway stops taking new orders in this market. Open orders stay on the book, and can still be canceled. The halt stays in place until you resume, including across reloads.",
      confirm: "Halt market",
    },
    resume: {
      title: `Resume ${market.symbol}?`,
      body: "Every gateway starts taking new orders in this market again.",
      confirm: "Resume market",
    },
    cancel: {
      title: `Cancel all orders in ${market.symbol}?`,
      body: "This cancels every open order in this market, for every player. It can't be undone. Halt the market first, or new orders can arrive while it runs.",
      confirm: "Cancel all orders",
    },
  }[kind];

  const blocked = kind === "cancel" && typed.toUpperCase() !== market.symbol;

  return (
    <Modal open title={copy.title} description={copy.body} onClose={onClose}>
      <div className="space-y-4">
        {kind === "cancel" && (
          <Field
            label={`Type ${market.symbol} to confirm`}
            autoComplete="off"
            value={typed}
            onChange={(event) => setTyped(event.target.value)}
          />
        )}
        {failure && <Notice state="bad">{failure}</Notice>}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Keep as it is</Button>
          <Button
            tone={kind === "resume" ? "primary" : "danger"}
            disabled={busy || blocked}
            onClick={() => void act()}
          >
            {busy ? "Working" : copy.confirm}
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function NewMarketDialog({
  onClose,
  onDone,
}: {
  onClose: () => void;
  onDone: (notice: Outcome) => void;
}) {
  const [symbol, setSymbol] = useState("");
  const [tick, setTick] = useState("0.01");
  const [min, setMin] = useState("");
  const [max, setMax] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  async function create() {
    setBusy(true);
    setFailure(null);
    try {
      const body: Record<string, string> = { tick_size: tick };
      if (min) body.min_size = min;
      if (max) body.max_size = max;
      const name = symbol.trim().toUpperCase();
      const result = await api.put<EditResult>(`/markets/${encodeURIComponent(name)}`, body);
      onDone(editOutcome(result, name));
    } catch (error) {
      setFailure(messageOf(error));
      setBusy(false);
    }
  }

  return (
    <Modal
      open
      title="New market"
      description="Gateways accept orders for it as soon as they have it. The engine needs no restart."
      onClose={onClose}
    >
      <form
        className="space-y-4"
        onSubmit={(event) => {
          event.preventDefault();
          void create();
        }}
      >
        <Field
          label="Symbol"
          autoFocus
          required
          maxLength={6}
          placeholder="IRON"
          value={symbol}
          onChange={(event) => setSymbol(event.target.value)}
          hint="1 to 6 letters or digits."
        />
        <Field
          label="Tick size"
          required
          value={tick}
          onChange={(event) => setTick(event.target.value)}
          hint="Prices must be a multiple of this."
        />
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            label="Minimum size"
            placeholder="Optional"
            value={min}
            onChange={(event) => setMin(event.target.value)}
          />
          <Field
            label="Maximum size"
            placeholder="Optional"
            value={max}
            onChange={(event) => setMax(event.target.value)}
          />
        </div>
        {failure && <Notice state="bad">{failure}</Notice>}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Cancel</Button>
          <Button tone="primary" type="submit" disabled={busy || !symbol.trim() || !tick}>
            {busy ? "Creating" : "Create market"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

interface Outcome {
  state: State;
  title: string;
  body?: string;
  gateways?: GatewayResult[];
}

function editOutcome(result: EditResult, name?: string): Outcome {
  const subject = name ?? "Markets";
  if (result.state === "applied") {
    return { state: "good", title: `${subject} saved and live on every gateway` };
  }
  if (result.state === "reaching_gateways") {
    return {
      state: "idle",
      title: `${subject} saved, reaching gateways`,
      body: `Kubernetes takes up to about ${result.expected_secs ?? 90} seconds to deliver it.`,
    };
  }
  return {
    state: "bad",
    title: `${subject} saved, but not every gateway took it`,
    body: "The file is saved, so reload rather than editing again.",
    gateways: result.gateways,
  };
}

function MarketRow({ market, onDialog }: { market: Market; onDialog: (dialog: Dialog) => void }) {
  const { state, text } = marketState(market);
  const held = market.gateways.filter((gateway) => gateway.in_sync).length;
  const unreachable = market.gateways.filter((gateway) => !gateway.reachable).length;
  const halting = market.gateways.filter((gateway) => gateway.market?.halted).length;
  const partlyHalted = market.halted && halting < market.gateways.length;
  return (
    <tr className="border-b border-line last:border-b-0">
      <td className="px-5 py-3">
        <Link
          to={`/activity?symbol=${market.symbol}`}
          className="font-mono font-medium hover:underline"
        >
          {market.symbol}
        </Link>
        {!market.in_file && <p className="text-xs text-warn">On a gateway, not in the file</p>}
      </td>
      <td className="px-5 py-3">
        <Status state={state}>{text}</Status>
        {partlyHalted && (
          <p className="text-xs text-warn">
            On {halting} of {market.gateways.length} gateways
          </p>
        )}
      </td>
      <td className="tabular px-5 py-3 font-mono text-xs">{market.tick_size}</td>
      <td className="tabular px-5 py-3 font-mono text-xs">{market.min_size ?? "default"}</td>
      <td className="tabular px-5 py-3 font-mono text-xs">{market.max_size ?? "default"}</td>
      <td className="px-5 py-3 text-xs text-muted">
        {held} of {market.gateways.length} in sync
        {unreachable > 0 && <span className="block text-bad">{unreachable} unreachable</span>}
      </td>
      <td className="px-5 py-3">
        <div className="flex justify-end gap-2">
          {market.halted ? (
            <Button onClick={() => onDialog({ kind: "resume", market })}>Resume</Button>
          ) : (
            <Button disabled={!market.enabled} onClick={() => onDialog({ kind: "halt", market })}>
              Halt
            </Button>
          )}
          <Button tone="danger" onClick={() => onDialog({ kind: "cancel", market })}>
            Cancel all
          </Button>
        </div>
      </td>
    </tr>
  );
}

function MarketTable({ markets, onDialog }: { markets: Market[]; onDialog: (d: Dialog) => void }) {
  if (markets.length === 0) {
    return (
      <div className="p-10 text-center">
        <p className="text-lg font-semibold tracking-heading">No markets yet</p>
        <p className="mt-1 text-muted">Create one, and gateways start taking its orders.</p>
        <Button tone="primary" className="mt-4" onClick={() => onDialog({ kind: "new" })}>
          New market
        </Button>
      </div>
    );
  }
  return (
    <TableFrame>
      <table className="w-full min-w-[820px] text-left text-sm">
        <thead className="text-muted">
          <tr className="border-b border-line">
            <th className="px-5 py-3 font-medium">Market</th>
            <th className="px-5 py-3 font-medium">State</th>
            <th className="px-5 py-3 font-medium">Tick</th>
            <th className="px-5 py-3 font-medium">Min</th>
            <th className="px-5 py-3 font-medium">Max</th>
            <th className="px-5 py-3 font-medium">Gateways</th>
            <th className="px-5 py-3 font-medium">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {markets.map((market) => (
            <MarketRow key={market.symbol} market={market} onDialog={onDialog} />
          ))}
        </tbody>
      </table>
    </TableFrame>
  );
}

function SaveDialog({
  check,
  onClose,
  onSave,
  busy,
}: {
  check: Validation;
  onClose: () => void;
  onSave: () => void;
  busy: boolean;
}) {
  const changes = check.changes;
  const lines = changes
    ? [
        ["Adds", changes.added],
        ["Removes", changes.removed],
        ["Changes", changes.changed],
      ].filter(([, symbols]) => symbols.length > 0)
    : [];
  return (
    <Modal open title="Save markets?" onClose={onClose}>
      <div className="space-y-4 text-sm">
        {lines.length === 0 ? (
          <p className="text-muted">No markets change. Only comments or formatting differ.</p>
        ) : (
          <ul className="space-y-1">
            {lines.map(([verb, symbols]) => (
              <li key={verb as string}>
                <span className="font-medium">{verb}: </span>
                <span className="font-mono">{(symbols as string[]).join(", ")}</span>
              </li>
            ))}
          </ul>
        )}
        {check.warnings?.map((warning) => (
          <Notice state="warn" key={warning}>
            {warning}
          </Notice>
        ))}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Keep editing</Button>
          <Button tone="primary" disabled={busy} onClick={onSave}>
            {busy ? "Saving" : "Save and reload gateways"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function YamlEditor({ onSaved }: { onSaved: (notice: Outcome) => void }) {
  const [file, setFile] = useState<MarketsFile | null>(null);
  const [text, setText] = useState("");
  const [problem, setProblem] = useState<Problem | null>(null);
  const [check, setCheck] = useState<Validation | null>(null);
  const [checking, setChecking] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const gutter = useRef<HTMLDivElement>(null);

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
    if (!file) return;
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
  }, [text, file]);

  async function save() {
    if (!file) return;
    setBusy(true);
    setFailure(null);
    try {
      const result = await api.put<EditResult>("/config/markets", {
        yaml: text,
        revision: file.revision,
      });
      setConfirming(false);
      onSaved(editOutcome(result));
      await load();
    } catch (error) {
      setConfirming(false);
      if (error instanceof ApiError && error.code === "markets_changed") setConflict(true);
      setFailure(messageOf(error));
    } finally {
      setBusy(false);
    }
  }

  if (!file) {
    return failure ? (
      <div className="p-5">
        <Notice state="bad" title="Can't open the markets file">
          {failure}
        </Notice>
      </div>
    ) : null;
  }

  const lines = text.split("\n").length;
  const dirty = text !== file.yaml;
  const location = file.backend === "kubernetes" ? `ConfigMap ${file.location}` : file.location;

  return (
    <div className="space-y-3 p-5">
      <p className="break-all text-xs text-muted">Stored in {location}</p>
      <div className="flex h-[26rem] overflow-hidden rounded-xl border border-line bg-canvas font-mono text-[13px] leading-6 focus-within:border-ink">
        <div
          ref={gutter}
          aria-hidden
          className="w-11 shrink-0 select-none overflow-hidden border-r border-line py-3 text-right text-muted"
        >
          {Array.from({ length: lines }, (_, index) => (
            <div
              key={index}
              className={cn("pr-2", problem?.line === index + 1 && "bg-bad/20 text-bad")}
            >
              {index + 1}
            </div>
          ))}
        </div>
        <textarea
          aria-label="Markets file"
          spellCheck={false}
          wrap="off"
          value={text}
          onChange={(event) => setText(event.target.value)}
          onScroll={(event) => {
            if (gutter.current) gutter.current.scrollTop = event.currentTarget.scrollTop;
          }}
          className="h-full min-w-0 flex-1 resize-none overflow-auto bg-transparent px-3 py-3 focus:outline-none"
        />
      </div>

      {problem && (
        <Notice state="bad" title={problem.line ? `Line ${problem.line}` : "This file isn't valid"}>
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
        <p className="text-sm text-muted">
          {checking
            ? "Checking"
            : problem
              ? "Fix this to save."
              : dirty
                ? "Valid, with unsaved changes."
                : "Valid. No changes."}
        </p>
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
      </div>

      {confirming && check && (
        <SaveDialog
          check={check}
          busy={busy}
          onClose={() => setConfirming(false)}
          onSave={() => void save()}
        />
      )}
    </div>
  );
}

export function Markets() {
  const markets = usePolled(() => api.get<MarketList>("/markets"), 3000);
  const [tab, setTab] = useState<Tab>("list");
  const [dialog, setDialog] = useState<Dialog>(null);
  const [outcome, setOutcome] = useState<Outcome | null>(null);

  function finish(notice: Outcome) {
    setDialog(null);
    // A save on its way has its own banner, which keeps counting until it lands.
    setOutcome(notice.title.endsWith("reaching gateways") ? null : notice);
    void markets.refresh();
  }

  async function reload() {
    try {
      const result = await api.post<EditResult>("/markets/reload");
      finish(editOutcome(result));
    } catch (error) {
      finish({ state: "bad", title: "Reload failed", body: messageOf(error) });
    }
  }

  // The list reads a parsed file, so a broken one shows here and is fixed in the editor.
  const broken = markets.failure instanceof ApiError && markets.failure.code === "markets_file";

  return (
    <>
      <PageHeader
        title="Markets"
        actions={
          <>
            <div role="tablist" aria-label="View" className="flex rounded-full bg-raised p-1">
              {(
                [
                  ["list", "List"],
                  ["yaml", "YAML"],
                ] as const
              ).map(([value, label]) => (
                <button
                  key={value}
                  type="button"
                  role="tab"
                  aria-selected={tab === value}
                  onClick={() => setTab(value)}
                  className={cn(
                    "h-7 rounded-full px-4 text-sm font-medium",
                    tab === value ? "bg-surface text-ink" : "text-muted hover:text-ink",
                  )}
                >
                  {label}
                </button>
              ))}
            </div>
            <Button tone="primary" onClick={() => setDialog({ kind: "new" })}>
              New market
            </Button>
          </>
        }
      />

      <div className="mb-4 space-y-3 empty:hidden">
        {outcome && (
          <Notice
            state={outcome.state}
            title={outcome.title}
            action={<Button onClick={() => setOutcome(null)}>Dismiss</Button>}
          >
            {outcome.body}
            {outcome.gateways && <GatewayFailures gateways={outcome.gateways} />}
          </Notice>
        )}
        {markets.data && (
          <DeliveryNotice delivery={markets.data.delivery} onReload={() => void reload()} />
        )}
        {markets.error && (
          <Notice
            state="bad"
            title={broken ? "The markets file doesn't parse" : "Can't read markets"}
            action={
              broken && tab === "list" ? (
                <Button onClick={() => setTab("yaml")}>Open the editor</Button>
              ) : undefined
            }
          >
            {markets.error}
            {broken && " Gateways keep what they last loaded until this is fixed."}
          </Notice>
        )}
      </div>

      <Card>
        {tab === "list" ? (
          markets.data && <MarketTable markets={markets.data.markets} onDialog={setDialog} />
        ) : (
          <YamlEditor onSaved={finish} />
        )}
      </Card>

      {dialog?.kind === "new" && (
        <NewMarketDialog onClose={() => setDialog(null)} onDone={finish} />
      )}
      {dialog && dialog.kind !== "new" && (
        <ActionDialog dialog={dialog} onClose={() => setDialog(null)} onDone={finish} />
      )}
    </>
  );
}
