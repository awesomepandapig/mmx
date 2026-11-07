import { useEffect, useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { Navigate, useSearchParams } from "react-router-dom";
import { Wordmark } from "../components/Layout";
import { Button, Card, Field, Notice, buttonClass, cn } from "../components/ui";
import { BASE, api, messageOf, queryString } from "../lib/api";
import type { Session } from "../lib/api";
import { useSession } from "../lib/session";

export function AuthPage({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-dvh items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <Wordmark className="mb-5 block text-center" />
        <Card className="p-4">{children}</Card>
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
  const [usePassword, setUsePassword] = useState(false);
  const target = params.get("return") ?? "/";
  const start = `${BASE}/oidc/start${queryString({ return: target })}`;

  // With only a provider there's nothing to ask here, so a person never sees this page, unless
  // they just signed out or something went wrong.
  const straightThrough =
    !!session &&
    !session.authenticated &&
    !!session.oidc &&
    !session.password_login &&
    !failure &&
    !params.has("signed_out");
  useEffect(() => {
    if (straightThrough) window.location.replace(start);
  }, [straightThrough, start]);

  if (!session || straightThrough) return null;
  if (session.authenticated) return <Navigate to={target} replace />;

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

  const form = (
    <form onSubmit={(event) => void submit(event)} className="space-y-3">
      <Field
        label="Password"
        type="password"
        autoComplete="current-password"
        autoFocus
        required
        value={password}
        onChange={(event) => setPassword(event.target.value)}
        hint={session.first_run ? "The first password is in the mmx serve log." : undefined}
      />
      <Button
        tone={session.oidc ? "plain" : "primary"}
        type="submit"
        disabled={busy || !password}
        className="w-full"
      >
        {busy ? "Signing in" : "Sign in"}
      </Button>
    </form>
  );

  return (
    <AuthPage>
      <h1 className="text-[17px] font-semibold">Sign in</h1>
      <div className="mt-4 space-y-3">
        {failure && <Notice state="bad">{failure}</Notice>}
        {session.oidc && (
          // A full navigation, because the provider's page has to take over the tab.
          <a href={start} className={cn(buttonClass("primary"), "w-full")}>
            Continue with {session.oidc.label}
          </a>
        )}
        {session.password_login &&
          (session.oidc && !usePassword ? (
            <Button tone="quiet" className="w-full" onClick={() => setUsePassword(true)}>
              Use the admin password
            </Button>
          ) : (
            form
          ))}
        {!session.password_login && !session.oidc && (
          <Notice state="warn">
            No sign-in method is on. Run <code>mmx auth password-login on</code>.
          </Notice>
        )}
      </div>
    </AuthPage>
  );
}
