import { useState } from "react";
import type { FormEvent } from "react";
import { Navigate } from "react-router-dom";
import { Button, Field, Notice } from "../components/ui";
import { api, messageOf } from "../lib/api";
import type { Session } from "../lib/api";
import { useSession } from "../lib/session";
import { AuthPage } from "./Login";

const MINIMUM = 12;

export function Setup() {
  const { session, accept } = useSession();
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  if (!session) return null;
  if (!session.authenticated) return <Navigate to="/login" replace />;
  if (!session.must_set_password) return <Navigate to="/" replace />;

  const mismatch = repeat !== "" && password !== repeat;

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      accept(await api.post<Session>("/password", { new: password }));
    } catch (error) {
      setFailure(messageOf(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthPage>
      <h1 className="text-[17px] font-semibold">Choose a password</h1>
      <form onSubmit={(event) => void submit(event)} className="mt-4 space-y-3">
        {failure && <Notice state="bad">{failure}</Notice>}
        <Field
          label="New password"
          type="password"
          autoComplete="new-password"
          autoFocus
          required
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          hint={`At least ${MINIMUM} characters.`}
        />
        <Field
          label="Repeat it"
          type="password"
          autoComplete="new-password"
          required
          value={repeat}
          onChange={(event) => setRepeat(event.target.value)}
          hint={mismatch ? "These don't match." : undefined}
        />
        <Button
          tone="primary"
          type="submit"
          className="w-full"
          disabled={busy || password.length < MINIMUM || password !== repeat}
        >
          {busy ? "Saving" : "Save password"}
        </Button>
      </form>
    </AuthPage>
  );
}
