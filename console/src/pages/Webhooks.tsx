import { useEffect, useState } from "react";
import type { FormEvent } from "react";
import {
  Button,
  Card,
  Code,
  CopyButton,
  Field,
  Modal,
  Notice,
  PageHeader,
  SectionTitle,
} from "../components/ui";
import { api, messageOf } from "../lib/api";
import type { WebhookConfig, WebhookPosition } from "../lib/api";
import { usePolled } from "../lib/hooks";
import { WebhookFacts } from "./Activity";

function Applied({ config }: { config: WebhookConfig }) {
  const restart = (
    <div className="mt-2 flex flex-wrap items-center gap-2">
      <Code>{config.restart}</Code>
      <CopyButton text={config.restart} />
    </div>
  );
  switch (config.state) {
    case "applied":
      return <Notice state="good" title="The outbox is using these settings" />;
    case "restart_required":
      return (
        <Notice state="warn" title="Saved. Restart the outbox to apply it">
          The outbox reads these settings when it starts, so it's still using the old ones. Once it
          starts, it sends everything on the tape it hasn't delivered yet, which on a first setup is
          the whole history.
          {restart}
        </Notice>
      );
    case "from_environment":
      return (
        <Notice state="idle" title="The outbox is using settings from its environment">
          It's sending to {config.running_url}. Saving a URL and a secret here replaces them after a
          restart.
        </Notice>
      );
    case "incomplete":
      return (
        <Notice state="idle" title="Set a URL and a signing secret">
          The outbox needs both, and a restart to pick them up.
        </Notice>
      );
    default:
      return (
        <Notice state="warn" title="Can't confirm what the outbox is using">
          {config.running_error ?? "This outbox doesn't report its settings."} Restart it to be sure
          it has these.
          {restart}
        </Notice>
      );
  }
}

export function Webhooks() {
  const config = usePolled(() => api.get<WebhookConfig>("/webhooks/config"), 5000);
  const delivery = usePolled(() => api.get<WebhookPosition>("/webhooks"), 3000);
  const [url, setUrl] = useState("");
  const [edited, setEdited] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);

  useEffect(() => {
    if (!edited) setUrl(config.data?.url ?? "");
  }, [config.data, edited]);

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
    setBusy(true);
    setFailure(null);
    try {
      const reply = await api.post<WebhookConfig>("/webhooks/secret");
      setSecret(reply.secret ?? null);
      setConfirming(false);
      await config.refresh();
    } catch (error) {
      setFailure(messageOf(error));
      setConfirming(false);
    } finally {
      setBusy(false);
    }
  }

  const hasSecret = !!config.data?.secret_hint;

  return (
    <>
      <PageHeader
        title="Webhooks"
        detail="MMX sends fills, cancels and rejects to your game. Use fill_id to ignore repeats, because a delivery can arrive more than once."
      />

      <div className="grid gap-4">
        {config.data && <Applied config={config.data} />}
        {config.error && (
          <Notice state="bad" title="Can't read webhook settings">
            {config.error}
          </Notice>
        )}

        <Card className="p-5">
          <SectionTitle>Destination</SectionTitle>
          <form onSubmit={(event) => void saveUrl(event)} className="mt-3 space-y-3">
            <Field
              label="Webhook URL"
              type="url"
              required
              placeholder="https://game.example/mmx/events"
              value={url}
              onChange={(event) => {
                setUrl(event.target.value);
                setEdited(true);
              }}
            />
            {failure && <Notice state="bad">{failure}</Notice>}
            <Button tone="primary" type="submit" disabled={busy || !edited || !url}>
              Save URL
            </Button>
          </form>
        </Card>

        <Card className="p-5">
          <SectionTitle>Signing secret</SectionTitle>
          <p className="mt-1 text-sm text-muted">
            Each delivery carries <Code>X-MMX-Timestamp</Code> and <Code>X-MMX-Signature</Code>,
            which is <Code>v1=</Code> and an HMAC-SHA256 of the timestamp, a dot and the raw body.
          </p>
          <p className="mt-3 text-sm">
            {hasSecret ? (
              <>
                Current secret ends in <Code>{config.data?.secret_hint}</Code>
              </>
            ) : (
              "No secret has been generated here."
            )}
          </p>
          <Button
            className="mt-3"
            onClick={() => (hasSecret ? setConfirming(true) : void generate())}
          >
            {hasSecret ? "Replace secret" : "Generate secret"}
          </Button>
        </Card>

        <Card className="p-5">
          <SectionTitle>Delivery</SectionTitle>
          <div className="mt-3">
            {delivery.data && <WebhookFacts position={delivery.data} />}
            {delivery.error && (
              <Notice state="bad" title="Can't reach the outbox">
                {delivery.error}
              </Notice>
            )}
          </div>
        </Card>
      </div>

      <Modal
        open={confirming}
        title="Replace the signing secret?"
        description="Once the outbox restarts it signs with the new secret, and your game must accept it. Deliveries signed with the new secret fail against the old one."
        onClose={() => setConfirming(false)}
      >
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirming(false)}>Keep the current one</Button>
          <Button tone="danger" disabled={busy} onClick={() => void generate()}>
            Replace secret
          </Button>
        </div>
      </Modal>

      <Modal
        open={secret !== null}
        title="Copy this signing secret"
        description="This is the only time it's shown. Store it with your game's secrets."
        onClose={() => setSecret(null)}
      >
        <div className="space-y-4">
          <p className="break-all rounded-xl bg-raised p-3 font-mono text-sm" data-testid="secret">
            {secret}
          </p>
          <div className="flex justify-end gap-2">
            <CopyButton text={secret ?? ""} />
            <Button tone="primary" onClick={() => setSecret(null)}>
              I've stored it
            </Button>
          </div>
        </div>
      </Modal>
    </>
  );
}
