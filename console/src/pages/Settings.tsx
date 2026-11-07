import { useEffect, useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { Confirm } from "../components/Confirm";
import {
  Button,
  Card,
  Code,
  CopyButton,
  ErrorState,
  Facts,
  Field,
  Notice,
  Page,
  PageHeader,
  SectionTitle,
  Skeleton,
  Switch,
  TextArea,
} from "../components/ui";
import { ApiError, api, messageOf } from "../lib/api";
import type { OidcConfig, Session, Settings as SettingsData } from "../lib/api";
import { usePolled, useTitle } from "../lib/hooks";
import { useSession } from "../lib/session";

type Result = { state: "good" | "bad"; text: string } | null;

function Line({ result }: { result: Result }) {
  return result ? <Notice state={result.state}>{result.text}</Notice> : null;
}

function Row({ title, children }: { title: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 border-t border-hairline py-3 first:border-t-0">
      <div className="min-w-0 text-sm font-medium">{title}</div>
      {children}
    </div>
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
      setResult({ state: "good", text: "Password changed. Everyone else was signed out." });
    } catch (error) {
      setResult({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <form
      onSubmit={(event) => void save(event)}
      className="grid gap-3 border-t border-hairline py-3 sm:grid-cols-2"
    >
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
      <div className="space-y-2 sm:col-span-2">
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
  const [confirm, setConfirm] = useState<"off" | "on" | "revoke" | null>(null);

  async function setPasswordLogin(on: boolean) {
    await api.put("/settings", { password_login: on });
    setConfirm(null);
    onSaved();
  }

  async function revoke() {
    accept(await api.post<Session>("/sessions/revoke"));
    setConfirm(null);
    setResult({ state: "good", text: "Everyone else was signed out." });
  }

  return (
    <Card className="p-3">
      <SectionTitle>Sign-in</SectionTitle>
      <div className="mt-2">
        <Row title="Password login">
          <Switch
            label="Password login"
            checked={data.password_login}
            onChange={(on) => setConfirm(on ? "on" : "off")}
          />
        </Row>
        {data.password_login && <Password session={session} />}
        <Row title="Sessions">
          <Button onClick={() => setConfirm("revoke")}>Sign out everyone else</Button>
        </Row>
        <Line result={result} />
      </div>
      {confirm === "off" && (
        <Confirm
          title="Turn off password login?"
          body={
            <>
              <Code>mmx auth password-login on</Code> turns it back on.
            </>
          }
          action="Turn off"
          stepUp
          run={() => setPasswordLogin(false)}
          onClose={() => setConfirm(null)}
        />
      )}
      {confirm === "on" && (
        <Confirm
          title="Turn on password login?"
          action="Turn on"
          tone="primary"
          stepUp
          run={() => setPasswordLogin(true)}
          onClose={() => setConfirm(null)}
        />
      )}
      {confirm === "revoke" && (
        <Confirm
          title="Sign out everyone else?"
          action="Sign them out"
          stepUp
          run={revoke}
          onClose={() => setConfirm(null)}
        />
      )}
    </Card>
  );
}

const toLines = (groups: string[]) => groups.join("\n");
const toList = (text: string) => text.split(/[\n,]/);

function SingleSignOn({ data, onSaved }: { data: SettingsData; onSaved: () => void }) {
  const saved: OidcConfig = data.oidc;
  const managed = saved.managed;
  const [enabled, setEnabled] = useState(saved.enabled);
  const [issuer, setIssuer] = useState(saved.issuer);
  const [clientId, setClientId] = useState(saved.client_id);
  const [secret, setSecret] = useState("");
  const [label, setLabel] = useState(saved.display_name ?? "");
  const [claim, setClaim] = useState(saved.groups_claim);
  const [admin, setAdmin] = useState(toLines(saved.roles.admin));
  const [operator, setOperator] = useState(toLines(saved.roles.operator));
  const [viewer, setViewer] = useState(toLines(saved.roles.viewer));
  const [result, setResult] = useState<Result>(null);
  const [probe, setProbe] = useState<Result>(null);
  const [confirming, setConfirming] = useState(false);

  useEffect(() => setEnabled(saved.enabled), [saved.enabled]);

  async function save() {
    await api.put("/settings/oidc", {
      enabled,
      issuer,
      client_id: clientId,
      client_secret: secret || undefined,
      display_name: label || undefined,
      groups_claim: claim,
      roles: { admin: toList(admin), operator: toList(operator), viewer: toList(viewer) },
    });
    setSecret("");
    setConfirming(false);
    setResult({ state: "good", text: "Saved. Everyone else was signed out." });
    onSaved();
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
          ? { state: "good", text: `Found ${reply.issuer}` }
          : { state: "bad", text: reply.error ?? "No answer." },
      );
    } catch (error) {
      setProbe({ state: "bad", text: messageOf(error) });
    }
  }

  return (
    <Card className="p-3">
      <SectionTitle
        actions={
          managed ? (
            <span className="text-[12px] text-muted">Set by the deployment</span>
          ) : (
            <Switch label="Single sign-on" checked={enabled} onChange={setEnabled} />
          )
        }
      >
        Single sign-on
      </SectionTitle>

      {saved.redirect_uri && (
        <div className="mt-3">
          <p className="text-[13px] font-medium">Redirect URI to register with your provider</p>
          <div className="mt-1 flex flex-wrap items-center gap-2">
            <Code>{saved.redirect_uri}</Code>
            <CopyButton text={saved.redirect_uri} />
          </div>
        </div>
      )}

      <form
        onSubmit={(event) => {
          event.preventDefault();
          setResult(null);
          setConfirming(true);
        }}
        className="mt-4 grid gap-3 sm:grid-cols-2"
      >
        <Field
          label="Issuer"
          className="sm:col-span-2"
          placeholder="https://id.game.example"
          readOnly={managed}
          value={issuer}
          onChange={(event) => setIssuer(event.target.value)}
        />
        <Field
          label="Client ID"
          autoComplete="off"
          readOnly={managed}
          value={clientId}
          onChange={(event) => setClientId(event.target.value)}
        />
        <Field
          label="Client secret"
          type="password"
          autoComplete="new-password"
          readOnly={managed}
          value={secret}
          onChange={(event) => setSecret(event.target.value)}
          placeholder={saved.client_secret_hint ?? ""}
        />
        <Field
          label="Button label"
          placeholder="single sign-on"
          readOnly={managed}
          value={label}
          onChange={(event) => setLabel(event.target.value)}
        />
        <Field
          label="Groups claim"
          readOnly={managed}
          value={claim}
          onChange={(event) => setClaim(event.target.value)}
        />
        <TextArea
          label="Admin groups"
          rows={3}
          readOnly={managed}
          value={admin}
          onChange={(event) => setAdmin(event.target.value)}
        />
        <TextArea
          label="Operator groups"
          rows={3}
          readOnly={managed}
          value={operator}
          onChange={(event) => setOperator(event.target.value)}
        />
        <TextArea
          label="Viewer groups"
          rows={3}
          readOnly={managed}
          value={viewer}
          onChange={(event) => setViewer(event.target.value)}
        />
        <p className="self-end text-[13px] text-faint">
          One group per line, by exact name. Anyone else is refused.
        </p>
        <div className="space-y-2 sm:col-span-2">
          <Line result={probe} />
          <Line result={result} />
          <div className="flex flex-wrap gap-2">
            {!managed && (
              <Button tone="primary" type="submit">
                Save
              </Button>
            )}
            <Button disabled={!issuer} onClick={() => void test()}>
              Test issuer
            </Button>
          </div>
        </div>
      </form>

      {confirming && (
        <Confirm
          title="Save single sign-on?"
          body="Everyone else has to sign in again."
          action="Save"
          tone="primary"
          stepUp
          run={save}
          onClose={() => setConfirming(false)}
        />
      )}
    </Card>
  );
}

export function Settings() {
  useTitle("Settings");
  const { session } = useSession();
  const settings = usePolled(() => api.get<SettingsData>("/settings"), 30000);
  const data = settings.data;
  const denied = settings.failure instanceof ApiError && settings.failure.status === 403;

  return (
    <Page className="max-w-[1320px]">
      <PageHeader title="Settings" />
      {settings.error && !data && (
        <Card>
          <ErrorState
            title={denied ? "Admins only" : "Can't read settings"}
            detail={settings.error}
          />
        </Card>
      )}
      {!data && !settings.error && <Skeleton className="h-64 w-full" />}
      {data && session && (
        <div className="grid items-start gap-2 lg:grid-cols-[3fr_2fr]">
          <SingleSignOn data={data} onSaved={() => void settings.refresh()} />
          <div className="grid gap-2">
            <SignIn data={data} session={session} onSaved={() => void settings.refresh()} />
            <Card className="p-3">
              <SectionTitle>This install</SectionTitle>
              <Facts
                className="mt-2"
                items={[
                  ["Version", data.version],
                  [
                    "Settings",
                    <span key="c" className="font-code text-[13px]">
                      {data.store.config}
                    </span>,
                  ],
                  [
                    "Secrets",
                    <span key="s" className="font-code text-[13px]">
                      {data.store.secrets}
                    </span>,
                  ],
                  ["Admin token", data.admin_token_set ? "Set" : "Not set"],
                ]}
              />
            </Card>
          </div>
        </div>
      )}
    </Page>
  );
}
