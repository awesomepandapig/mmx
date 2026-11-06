import { useState } from "react";
import type { ActionResult, CancelResult, EditResult, GatewayResult, Market } from "../lib/api";
import { api } from "../lib/api";
import { Confirm } from "./Confirm";
import { Button, Field, Modal, Notice } from "./ui";
import type { State } from "./ui";
import { messageOf } from "../lib/api";

export interface Outcome {
  state: State;
  title: string;
  body?: string;
  gateways?: GatewayResult[];
}

function failed(gateways: GatewayResult[]) {
  return gateways.filter((gateway) => !gateway.ok);
}

export function OutcomeNotice({ outcome, onClose }: { outcome: Outcome; onClose: () => void }) {
  return (
    <Notice
      state={outcome.state}
      title={outcome.title}
      action={<Button onClick={onClose}>Dismiss</Button>}
    >
      {outcome.body}
      {outcome.gateways && failed(outcome.gateways).length > 0 && (
        <ul className="mt-1 space-y-0.5">
          {failed(outcome.gateways).map((gateway) => (
            <li key={gateway.gateway} className="break-words">
              {gateway.gateway}: {gateway.error}
            </li>
          ))}
        </ul>
      )}
    </Notice>
  );
}

export function editOutcome(result: EditResult, name?: string): Outcome {
  const subject = name ?? "Markets";
  if (result.state === "applied") {
    return { state: "good", title: `${subject} saved on every gateway` };
  }
  if (result.state === "reaching_gateways") {
    return { state: "idle", title: `${subject} saved, reaching gateways` };
  }
  return {
    state: "bad",
    title: `${subject} saved, but not every gateway took it`,
    body: "Reload gateways. Don't edit it again.",
    gateways: result.gateways,
  };
}

export type MarketDialog =
  | { kind: "halt" | "resume" | "cancel" | "delete" | "edit"; market: Market }
  | { kind: "new" }
  | null;

function run(
  kind: "halt" | "resume" | "cancel" | "delete",
  symbol: string,
  onDone: (outcome: Outcome) => void,
) {
  return async () => {
    if (kind === "delete") {
      const result = await api.delete<EditResult>(`/markets/${symbol}`);
      onDone(editOutcome(result, symbol));
      return;
    }
    if (kind === "cancel") {
      const result = await api.post<CancelResult>(`/markets/${symbol}/cancel-all`);
      const counts = `${result.requested} requested, ${result.canceled} canceled, ${result.failed} failed.`;
      onDone(
        result.complete
          ? { state: "good", title: `Canceled open orders in ${symbol}`, body: counts }
          : {
              state: "warn",
              title: `Some orders in ${symbol} may still be open`,
              body: `${counts}${result.truncated ? " More remain, so run it again." : ""}`,
              gateways: result.gateways,
            },
      );
      return;
    }
    const result = await api.post<ActionResult>(`/markets/${symbol}/${kind}`);
    const verb = kind === "halt" ? "Halted" : "Resumed";
    onDone(
      result.ok
        ? { state: "good", title: `${verb} ${symbol} on every gateway` }
        : {
            state: "bad",
            title: `${symbol} isn't ${verb.toLowerCase()} on every gateway`,
            gateways: result.gateways,
          },
    );
  };
}

function MarketForm({
  market,
  onClose,
  onDone,
}: {
  market?: Market;
  onClose: () => void;
  onDone: (outcome: Outcome) => void;
}) {
  const [symbol, setSymbol] = useState(market?.symbol ?? "");
  const [tick, setTick] = useState(market?.tick_size ?? "0.01");
  const [min, setMin] = useState(market?.min_size ?? "");
  const [max, setMax] = useState(market?.max_size ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  async function save() {
    setBusy(true);
    setFailure(null);
    try {
      const body: Record<string, string | boolean> = { tick_size: tick };
      if (min) body.min_size = min;
      if (max) body.max_size = max;
      if (market && !market.enabled) body.enabled = false;
      const name = symbol.trim().toUpperCase();
      const result = await api.put<EditResult>(`/markets/${encodeURIComponent(name)}`, body);
      onDone(editOutcome(result, name));
    } catch (error) {
      setFailure(messageOf(error));
      setBusy(false);
    }
  }

  return (
    <Modal open title={market ? `Edit ${market.symbol}` : "New market"} onClose={onClose}>
      <form
        className="space-y-3"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        {!market && (
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
        )}
        <Field
          label="Tick size"
          required
          value={tick}
          onChange={(event) => setTick(event.target.value)}
        />
        <div className="grid gap-3 sm:grid-cols-2">
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
        {market && <p className="text-sm text-muted">Open orders keep their price and size.</p>}
        {failure && <Notice state="bad">{failure}</Notice>}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Cancel</Button>
          <Button tone="primary" type="submit" disabled={busy || !symbol.trim() || !tick}>
            {busy ? "Saving" : market ? "Save" : "Create market"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

export function MarketDialogs({
  dialog,
  onClose,
  onDone,
}: {
  dialog: MarketDialog;
  onClose: () => void;
  onDone: (outcome: Outcome) => void;
}) {
  if (!dialog) return null;
  if (dialog.kind === "new") return <MarketForm onClose={onClose} onDone={onDone} />;
  if (dialog.kind === "edit") {
    return <MarketForm market={dialog.market} onClose={onClose} onDone={onDone} />;
  }
  const { kind, market } = dialog;
  const symbol = market.symbol;
  const copy = {
    halt: {
      title: `Halt ${symbol}?`,
      body: "Gateways refuse new orders until you resume. Open orders stay on the book.",
      action: "Halt market",
    },
    resume: { title: `Resume ${symbol}?`, body: undefined, action: "Resume market" },
    cancel: {
      title: `Cancel all orders in ${symbol}?`,
      body: "Every open order, for every player. This can't be undone. Halt first, or new orders can arrive while it runs.",
      action: "Cancel all orders",
    },
    delete: {
      title: `Delete ${symbol}?`,
      body: "Open orders stay in the engine, so cancel them first.",
      action: "Delete market",
    },
  }[kind];
  return (
    <Confirm
      title={copy.title}
      body={copy.body}
      action={copy.action}
      tone={kind === "resume" ? "primary" : "danger"}
      word={kind === "cancel" || kind === "delete" ? symbol : undefined}
      stepUp={kind === "cancel" || kind === "delete"}
      run={run(kind, symbol, onDone)}
      onClose={onClose}
    />
  );
}
