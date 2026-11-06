import { useEffect, useState } from "react";
import type { FormEvent } from "react";
import { Confirm } from "../components/Confirm";
import {
  Button,
  Card,
  Code,
  CopyButton,
  ErrorState,
  Facts,
  Field,
  Modal,
  Notice,
  Page,
  PageHeader,
  SectionTitle,
  Skeleton,
  Status,
} from "../components/ui";
import type { State } from "../components/ui";
import { api, messageOf } from "../lib/api";
import type { WebhookConfig, WebhookPosition } from "../lib/api";
import { count } from "../lib/format";
import { usePolled, useTitle } from "../lib/hooks";
import { useCan } from "../lib/session";

const deliveryWords: Record<WebhookPosition["state"], { state: State; text: string }> = {
  not_configured: { state: "idle", text: "Not configured" },
  halted: { state: "bad", text: "Halted" },
  verifying: { state: "warn", text: "Verifying" },
  failing: { state: "bad", text: "Failing" },
  delivering: { state: "warn", text: "Delivering" },
  idle: { state: "good", text: "Caught up" },
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
      className="mb-2"
    >
      {unknown && config.running_error && <p>{config.running_error}</p>}
      <p>
        It then delivers everything on the tape it hasn't sent, which can be a large first batch.
      </p>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Code>{config.restart}</Code>
        <CopyButton text={config.restart} />
      </div>
    </Notice>
  );
}

export function Webhooks() {
  useTitle("Webhooks");
  const canEdit = useCan("admin");
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

  return (
    <Page className="max-w-[920px]">
      <PageHeader
        title="Webhooks"
        detail={words ? <Status state={words.state}>{words.text}</Status> : undefined}
      />

      {config.error && !current && (
        <Card>
          <ErrorState
            title="Can't read webhook settings"
            detail={config.error}
            onRetry={() => void config.refresh()}
          />
        </Card>
      )}
      {current && <Restart config={current} />}

      <div className="grid gap-2">
        <Card className="p-5">
          <SectionTitle>Delivery</SectionTitle>
          {delivery.error ? (
            <p className="mt-2 text-sm text-bad">{delivery.error}</p>
          ) : !position ? (
            <Skeleton className="mt-3 h-14 w-full" />
          ) : (
            <>
              {position.halted && <p className="mt-2 text-sm text-bad">{position.halted}</p>}
              <Facts
                className="tabular mt-3"
                items={[
                  ["Waiting to deliver", `${count(position.backlog)} bytes`],
                  ["Failures in a row", count(position.consecutive_failures)],
                  ["Delivered up to", count(position.delivered_position)],
                  ["Tape position", count(position.durable_position)],
                ]}
              />
            </>
          )}
        </Card>

        <Card className="p-5">
          <SectionTitle>Destination</SectionTitle>
          <form onSubmit={(event) => void saveUrl(event)} className="mt-3 flex items-end gap-2">
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
        </Card>

        <Card className="p-5">
          <SectionTitle
            actions={
              canEdit && (
                <Button
                  tone={current?.secret_hint ? "danger" : "primary"}
                  onClick={() => setConfirming(true)}
                >
                  {current?.secret_hint ? "Replace secret" : "Generate secret"}
                </Button>
              )
            }
          >
            Signing secret
          </SectionTitle>
          <p className="mt-2 text-sm text-muted">
            {current?.secret_hint ? (
              <>
                Ends in <Code>{current.secret_hint}</Code>
              </>
            ) : (
              "None generated here."
            )}
          </p>
        </Card>
      </div>

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
          <p className="break-all rounded-xl bg-deep p-3 font-code text-sm" data-testid="secret">
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
    </Page>
  );
}
