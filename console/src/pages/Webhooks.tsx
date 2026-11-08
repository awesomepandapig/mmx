import { useEffect, useState } from "react";
import type { FormEvent } from "react";
import { Confirm } from "../components/Confirm";
import {
  Button,
  Code,
  CopyButton,
  ErrorState,
  Field,
  Metric,
  Modal,
  Notice,
  Panel,
  Tiles,
  Word,
} from "../components/ui";
import { api, messageOf } from "../lib/api";
import type { WebhookConfig, WebhookPosition } from "../lib/api";
import { useExchange } from "../lib/exchange";
import type { Tone } from "../lib/exchange";
import { bytes, count } from "../lib/format";
import { usePolled, useTitle } from "../lib/hooks";
import { useCan } from "../lib/session";

const deliveryWords: Record<WebhookPosition["state"], { word: string; tone: Tone }> = {
  not_configured: { word: "Not configured", tone: "warn" },
  halted: { word: "Halted", tone: "bad" },
  verifying: { word: "Verifying", tone: "plain" },
  failing: { word: "Failing", tone: "bad" },
  delivering: { word: "Delivering", tone: "plain" },
  idle: { word: "Caught up", tone: "plain" },
};

function Restart({ config }: { config: WebhookConfig }) {
  if (config.state !== "restart_required" && config.state !== "unknown") return null;
  const unknown = config.state === "unknown";
  return (
    <Notice
      state="warn"
      title={
        unknown ? "Can't confirm what the outbox is using" : "Restart the outbox to apply this"
      }
      className="border-x-0 border-t-0"
    >
      {unknown && config.running_error && <p>{config.running_error}</p>}
      <p>It then delivers everything on the tape it hasn't sent, which can be a large batch.</p>
      <div className="mt-1 flex flex-wrap items-center gap-2">
        <Code>{config.restart}</Code>
        <CopyButton text={config.restart} />
      </div>
    </Notice>
  );
}

export function Webhooks() {
  useTitle("Webhooks");
  const canEdit = useCan("admin");
  const { canTrade, status } = useExchange();
  const config = usePolled(() => api.get<WebhookConfig>("/webhooks/config"), 5000);
  const delivery = usePolled(() => api.get<WebhookPosition>("/webhooks"), 3000);
  const [url, setUrl] = useState("");
  const [edited, setEdited] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);

  const current = config.data;
  const position = delivery.data;

  useEffect(() => {
    if (!edited) setUrl(current?.url ?? current?.running_url ?? "");
  }, [current, edited]);

  async function saveUrl(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await api.put<WebhookConfig>("/webhooks/config", { url });
      setEdited(false);
      await config.refresh();
    } catch (error) {
      setFailure(messageOf(error));
    } finally {
      setBusy(false);
    }
  }

  async function generate() {
    const reply = await api.post<WebhookConfig>("/webhooks/secret");
    setSecret(reply.secret ?? null);
    setConfirming(false);
    await config.refresh();
  }

  const words = position ? deliveryWords[position.state] : null;
  const stopped = !!status && !canTrade && words?.tone !== "bad";
  const outbox = status?.consumers.find((row) => row.name === "outbox");

  return (
    <div className="flex flex-1 flex-col">
      {current && <Restart config={current} />}
      <Tiles className="flex-none grid-cols-2 border-b border-line md:grid-cols-5">
        <Metric
          label="Delivery"
          value={stopped ? "Tape stopped" : (words?.word ?? (delivery.error ? "-" : null))}
          tone={stopped ? "muted" : words?.tone}
          note={position?.halted ?? undefined}
        />
        <Metric
          label="Backlog"
          value={position ? bytes(position.backlog) : delivery.error ? "-" : null}
        />
        <Metric
          label="Failures in a row"
          value={position ? count(position.consecutive_failures) : delivery.error ? "-" : null}
          tone={position && position.consecutive_failures > 0 ? "bad" : "plain"}
        />
        <Metric
          label="Delivered to"
          value={position ? bytes(position.delivered_position) : delivery.error ? "-" : null}
          note={position ? `${count(position.delivered_position)} bytes` : undefined}
        />
        <Metric
          label="Behind the tape"
          value={
            outbox?.behind !== undefined && !stopped ? `${count(outbox.behind)} events` : "Unknown"
          }
          note={position ? `Tape ${count(position.durable_position)} bytes` : undefined}
        />
      </Tiles>
      {delivery.error && <p className="bg-surface px-3 py-1 text-bad">{delivery.error}</p>}
      {config.error && !current && (
        <ErrorState
          title="Can't read webhook settings"
          detail={config.error}
          onRetry={() => void config.refresh()}
        />
      )}
      <Tiles className="grid-cols-1 border-b border-line md:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <Panel title="Destination" bodyClassName="p-3">
          <form onSubmit={(event) => void saveUrl(event)} className="flex items-end gap-2">
            <Field
              label="Webhook URL"
              type="url"
              className="min-w-0 flex-1"
              required
              readOnly={!canEdit}
              placeholder="https://game.example/mmx/events"
              value={url}
              onChange={(event) => {
                setUrl(event.target.value);
                setEdited(true);
              }}
            />
            {canEdit && (
              <Button tone="primary" type="submit" disabled={busy || !edited || !url}>
                Save
              </Button>
            )}
          </form>
          {failure && (
            <Notice state="bad" className="mt-2">
              {failure}
            </Notice>
          )}
        </Panel>
        <Panel
          title="Signing secret"
          aside={
            canEdit && (
              <Button
                tone={current?.secret_hint ? "caution" : "primary"}
                className="h-[22px] px-1.5 text-[14px]"
                disabled={!current}
                onClick={() => setConfirming(true)}
              >
                {current?.secret_hint ? "Replace secret" : "Generate secret"}
              </Button>
            )
          }
          bodyClassName="p-3"
        >
          {current?.secret_hint ? (
            <Word tone="muted">
              Ends in <Code>{current.secret_hint}</Code>
            </Word>
          ) : (
            <Word tone="muted">None generated here.</Word>
          )}
        </Panel>
      </Tiles>

      {confirming && (
        <Confirm
          title={
            current?.secret_hint ? "Replace the signing secret?" : "Generate a signing secret?"
          }
          body={
            current?.secret_hint
              ? "Your game must accept the new secret before the outbox restarts."
              : undefined
          }
          action={current?.secret_hint ? "Replace secret" : "Generate secret"}
          tone={current?.secret_hint ? "danger" : "primary"}
          stepUp
          run={generate}
          onClose={() => setConfirming(false)}
        />
      )}

      <Modal
        open={secret !== null}
        title="Copy this signing secret"
        description="It won't be shown again."
        onClose={() => setSecret(null)}
      >
        <div className="space-y-3">
          <p className="break-all bg-deep p-2 font-code text-[14px]" data-testid="secret">
            {secret}
          </p>
          <div className="flex justify-end gap-2">
            <CopyButton text={secret ?? ""} />
            <Button tone="primary" onClick={() => setSecret(null)}>
              Done
            </Button>
          </div>
        </div>
      </Modal>
    </div>
  );
}
