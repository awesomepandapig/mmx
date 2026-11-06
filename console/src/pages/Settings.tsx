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
  Switch,
} from "../components/ui";
import { api, messageOf } from "../lib/api";
import type { OidcConfig, Session, Settings as SettingsData } from "../lib/api";
import { usePolled } from "../lib/hooks";
import { useSession } from "../lib/session";

type Result = { state: "good" | "bad"; text: string } | null;

function Line({ result }: { result: Result }) {
  return result ? <Notice state={result.state}>{result.text}</Notice> : null;
}

function PublicUrl({ data, onSaved }: { data: SettingsData; onSaved: () => void }) {
  const [url, setUrl] = useState(data.public_url ?? "");
  const [result, setResult] = useState<Result>(null);

  async function save(event: FormEvent) {
    event.preventDefault();
    try {
      await api.put("/settings", { public_url: url });
      setResult({ state: "good", text: "Saved." });
      onSaved();
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <Card className="p-5">
      <SectionTitle>Public URL</SectionTitle>
      <p className="mt-1 text-sm text-muted">
        Where people reach this console. Single sign-on builds its redirect URI from this, and an
        https address makes session cookies Secure.
      </p>
      <form onSubmit={(event) => void save(event)} className="mt-3 space-y-3">
        <Field
          label="URL"
          placeholder="https://mmx.game.example"
          value={url}
          onChange={(event) => setUrl(event.target.value)}
        />
        {!url && (
          <Button onClick={() => setUrl(window.location.origin)}>
            Use {window.location.origin}
          </Button>
        )}
        <Line result={result} />
        <Button tone="primary" type="submit" disabled={url === (data.public_url ?? "")}>
          Save
        </Button>
      </form>
    </Card>
  );
}

function Password({ session }: { session: Session }) {
  const { accept } = useSession();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [result, setResult] = useState<Result>(null);
  const needsCurrent = session.method === "password";

  async function save(event: FormEvent) {
    event.preventDefault();
    setResult(null);
    try {
      const body = needsCurrent ? { current, new: next } : { new: next };
      accept(await api.post<Session>("/password", body));
      setCurrent("");
      setNext("");
      setResult({ state: "good", text: "Password changed. Other sessions were signed out." });
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <form onSubmit={(event) => void save(event)} className="mt-3 grid gap-3 sm:grid-cols-2">
      {needsCurrent && (
        <Field
          label="Current password"
          type="password"
          autoComplete="current-password"
          required
          value={current}
          onChange={(event) => setCurrent(event.target.value)}
        />
      )}
      <Field
        label="New password"
        type="password"
        autoComplete="new-password"
        required
        minLength={12}
        value={next}
        onChange={(event) => setNext(event.target.value)}
        hint="At least 12 characters."
      />
      <div className="space-y-3 sm:col-span-2">
        <Line result={result} />
        <Button type="submit" disabled={!next}>
          Change password
        </Button>
      </div>
    </form>
  );
}

function SignIn({
  data,
  session,
  onSaved,
}: {
  data: SettingsData;
  session: Session;
  onSaved: () => void;
}) {
  const { accept } = useSession();
  const [result, setResult] = useState<Result>(null);
  const [confirming, setConfirming] = useState(false);

  async function setPasswordLogin(on: boolean) {
    setResult(null);
    try {
      await api.put("/settings", { password_login: on });
      onSaved();
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  async function revoke() {
    setConfirming(false);
    try {
      accept(await api.post<Session>("/sessions/revoke"));
      setResult({ state: "good", text: "Every other session was signed out." });
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <Card className="p-5">
      <SectionTitle>Sign-in</SectionTitle>
      <div className="mt-3 flex items-start justify-between gap-4">
        <div>
          <p className="font-medium">Password login</p>
          <p className="text-sm text-muted">
            It can only be turned off by someone signed in through single sign-on, which proves that
            works. <Code>mmx auth password-login on</Code> turns it back on.
          </p>
        </div>
        <Switch
          label="Password login"
          checked={data.password_login}
          onChange={(on) => void setPasswordLogin(on)}
        />
      </div>
      {data.password_login && <Password session={session} />}
      <div className="mt-4 border-t border-line pt-4">
        <Line result={result} />
        <Button className="mt-2" onClick={() => setConfirming(true)}>
          Sign out other sessions
        </Button>
      </div>
      <Modal
        open={confirming}
        title="Sign out every other session?"
        description="Everyone else has to sign in again. You stay signed in here."
        onClose={() => setConfirming(false)}
      >
        <div className="flex justify-end gap-2">
          <Button onClick={() => setConfirming(false)}>Cancel</Button>
          <Button tone="danger" onClick={() => void revoke()}>
            Sign them out
          </Button>
        </div>
      </Modal>
    </Card>
  );
}

function SingleSignOn({ data, onSaved }: { data: SettingsData; onSaved: () => void }) {
  const saved: OidcConfig = data.oidc;
  const [enabled, setEnabled] = useState(saved.enabled);
  const [issuer, setIssuer] = useState(saved.issuer);
  const [clientId, setClientId] = useState(saved.client_id);
  const [secret, setSecret] = useState("");
  const [label, setLabel] = useState(saved.display_name ?? "");
  const [allowed, setAllowed] = useState(saved.allowed.join("\n"));
  const [result, setResult] = useState<Result>(null);
  const [probe, setProbe] = useState<Result>(null);

  useEffect(() => setEnabled(saved.enabled), [saved.enabled]);

  async function save(event: FormEvent) {
    event.preventDefault();
    setResult(null);
    try {
      await api.put("/settings/oidc", {
        enabled,
        issuer,
        client_id: clientId,
        client_secret: secret || undefined,
        display_name: label || undefined,
        allowed: allowed.split(/[\n,]/),
      });
      setSecret("");
      setResult({
        state: "good",
        text: enabled
          ? "Saved. Sign out and use the provider once to prove it works."
          : "Saved. Single sign-on is off.",
      });
      onSaved();
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  async function test() {
    setProbe(null);
    try {
      const reply = await api.post<{ ok: boolean; error?: string; issuer?: string }>(
        "/settings/oidc/test",
        { issuer },
      );
      setProbe(
        reply.ok
          ? {
              state: "good",
              text: `Found the provider at ${reply.issuer}. That doesn't test the client ID or secret: only a sign-in does.`,
            }
          : { state: "bad", text: reply.error ?? "It didn't answer." },
      );
    } catch (error) {
      setProbe({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <Card className="p-5">
      <div className="flex items-start justify-between gap-4">
        <div>
          <SectionTitle>Single sign-on</SectionTitle>
          <p className="mt-1 text-sm text-muted">
            Sign in to this console through an OpenID Connect provider, using the authorization code
            flow with PKCE.
          </p>
        </div>
        <Switch label="Single sign-on" checked={enabled} onChange={setEnabled} />
      </div>

      <div className="mt-3">
        <p className="text-sm font-medium">Redirect URI to register with the provider</p>
        {saved.redirect_uri ? (
          <div className="mt-1 flex flex-wrap items-center gap-2">
            <Code>{saved.redirect_uri}</Code>
            <CopyButton text={saved.redirect_uri} />
          </div>
        ) : (
          <p className="text-sm text-muted">Set the public URL above to get this.</p>
        )}
      </div>

      <form onSubmit={(event) => void save(event)} className="mt-4 grid gap-3 sm:grid-cols-2">
        <Field
          label="Issuer"
          className="sm:col-span-2"
          placeholder="https://id.game.example"
          value={issuer}
          onChange={(event) => setIssuer(event.target.value)}
        />
        <Field
          label="Client ID"
          autoComplete="off"
          value={clientId}
          onChange={(event) => setClientId(event.target.value)}
        />
        <Field
          label="Client secret"
          type="password"
          autoComplete="new-password"
          value={secret}
          onChange={(event) => setSecret(event.target.value)}
          placeholder={saved.client_secret_hint ?? ""}
          hint={saved.client_secret_hint ? "Leave empty to keep the current one." : undefined}
        />
        <Field
          label="Button label"
          placeholder="single sign-on"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          hint="Shown as Continue with this."
        />
        <div className="sm:col-span-2">
          <label htmlFor="allowed" className="mb-1 block text-sm font-medium">
            Who may sign in
          </label>
          <textarea
            id="allowed"
            rows={3}
            value={allowed}
            onChange={(event) => setAllowed(event.target.value)}
            placeholder={"admin@game.example\n@game.example"}
            className="w-full rounded-xl border border-line bg-canvas px-3 py-2 font-mono text-sm placeholder:text-muted/70 focus:border-ink focus:outline-none"
          />
          <p className="mt-1 text-xs text-muted">
            One per line: an email, an @domain, or a subject ID. Anyone listed is a full admin.
            Emails only count if the provider marks them verified. If yours doesn't, use subject
            IDs.
          </p>
        </div>
        <div className="space-y-3 sm:col-span-2">
          <Line result={probe} />
          <Line result={result} />
          <div className="flex flex-wrap gap-2">
            <Button tone="primary" type="submit">
              Save
            </Button>
            <Button disabled={!issuer} onClick={() => void test()}>
              Test issuer
            </Button>
          </div>
        </div>
      </form>
    </Card>
  );
}

export function Settings() {
  const { session } = useSession();
  const settings = usePolled(() => api.get<SettingsData>("/settings"), 30000);
  const data = settings.data;

  return (
    <>
      <PageHeader title="Settings" />
      <div className="grid gap-4">
        {settings.error && (
          <Notice state="bad" title="Can't read settings">
            {settings.error}
          </Notice>
        )}
        {data && session && (
          <>
            <PublicUrl data={data} onSaved={() => void settings.refresh()} />
            <SignIn data={data} session={session} onSaved={() => void settings.refresh()} />
            <SingleSignOn data={data} onSaved={() => void settings.refresh()} />
            <Card className="p-5">
              <SectionTitle>This install</SectionTitle>
              <dl className="mt-3 grid gap-3 text-sm sm:grid-cols-2">
                <div>
                  <dt className="text-muted">Version</dt>
                  <dd>{data.version}</dd>
                </div>
                <div>
                  <dt className="text-muted">Settings are kept in</dt>
                  <dd className="break-all">{data.store.config}</dd>
                </div>
                <div>
                  <dt className="text-muted">Secrets are kept in</dt>
                  <dd className="break-all">{data.store.secrets}</dd>
                </div>
                <div>
                  <dt className="text-muted">Admin token for the CLI</dt>
                  <dd>{data.admin_token_set ? "Set" : "Not set"}</dd>
                </div>
              </dl>
            </Card>
          </>
        )}
      </div>
    </>
  );
}
