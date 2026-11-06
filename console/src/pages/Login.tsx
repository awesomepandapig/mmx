import { useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { Navigate, useSearchParams } from "react-router-dom";
import { Wordmark } from "../components/Layout";
import { Button, Card, Field, Notice } from "../components/ui";
import { BASE, api, messageOf } from "../lib/api";
import type { Session } from "../lib/api";
import { useSession } from "../lib/session";

export function AuthPage({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-dvh items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <Wordmark className="mb-6 block text-center text-2xl" />
        <Card className="p-6">{children}</Card>
      </div>
    </main>
  );
}

export function Login() {
  const { session, accept } = useSession();
  const [params] = useSearchParams();
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(params.get("error"));

  if (!session) return null;
  if (session.authenticated) return <Navigate to="/" replace />;

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      accept(await api.post<Session>("/login", { password }));
    } catch (error) {
      setFailure(messageOf(error));
      setPassword("");
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthPage>
      <h1 className="text-2xl font-semibold tracking-heading">Sign in</h1>
      <div className="mt-4 space-y-4">
        {failure && <Notice state="bad">{failure}</Notice>}
        {session.password_login && (
          <form onSubmit={(event) => void submit(event)} className="space-y-4">
            <Field
              label="Password"
              type="password"
              autoComplete="current-password"
              autoFocus
              required
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              hint="On a new install, the first password is in the mmx serve log."
            />
            <Button tone="primary" type="submit" disabled={busy || !password} className="w-full">
              {busy ? "Signing in" : "Sign in"}
            </Button>
          </form>
        )}
        {session.oidc && (
          // A full navigation, because the provider's page has to take over the tab.
          <a
            href={`${BASE}/oidc/start`}
            className="inline-flex h-9 w-full items-center justify-center rounded-full border border-line bg-surface px-4 text-sm font-medium hover:bg-raised"
          >
            Continue with {session.oidc.label}
          </a>
        )}
        {!session.password_login && !session.oidc && (
          <Notice state="warn">
            No way to sign in is turned on. Run <code>mmx auth password-login on</code>.
          </Notice>
        )}
      </div>
    </AuthPage>
  );
}
