import { useState } from "react";
import type { FormEvent, ReactNode } from "react";
import { useLocation } from "react-router-dom";
import { ApiError, BASE, api, messageOf, queryString } from "../lib/api";
import type { Session } from "../lib/api";
import { useSession } from "../lib/session";
import { Button, Field, Modal, Notice, buttonClass } from "./ui";

function needsSignIn(session: Session | null) {
  return !session?.reauth_until || session.reauth_until * 1000 <= Date.now();
}

/**
 * Asks before doing something. With `stepUp`, the person must have signed in recently: a password
 * session types it here and the action runs straight away, and an OIDC session goes to the provider
 * and comes back to repeat it. `word` must be typed first, for what can't be undone.
 */
export function Confirm({
  title,
  body,
  action,
  tone = "danger",
  word,
  stepUp,
  run,
  onClose,
}: {
  title: string;
  body?: ReactNode;
  action: string;
  tone?: "primary" | "danger";
  word?: string;
  stepUp?: boolean;
  run: () => Promise<void>;
  onClose: () => void;
}) {
  const { session, accept } = useSession();
  const { pathname, search } = useLocation();
  const [typed, setTyped] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [refused, setRefused] = useState(false);

  const signIn = !!stepUp && (refused || needsSignIn(session));
  const viaProvider = session?.method === "oidc";
  const blocked = (word !== undefined && typed.toUpperCase() !== word) || (signIn && !password);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      if (signIn) {
        accept(await api.post<Session>("/reauth", { password }));
        setRefused(false);
      }
      await run();
    } catch (error) {
      if (error instanceof ApiError && error.code === "reauth_required") setRefused(true);
      else setFailure(messageOf(error));
      setPassword("");
      setBusy(false);
    }
  }

  const back = `${BASE}/oidc/start${queryString({ reauth: "1", return: pathname + search })}`;

  return (
    <Modal open title={title} description={body} onClose={onClose}>
      <form onSubmit={(event) => void submit(event)} className="space-y-4">
        {word !== undefined && (
          <Field
            label={`Type ${word} to confirm`}
            autoComplete="off"
            autoFocus
            value={typed}
            onChange={(event) => setTyped(event.target.value)}
          />
        )}
        {signIn && !viaProvider && (
          <Field
            label="Your password"
            type="password"
            autoComplete="current-password"
            autoFocus={word === undefined}
            value={password}
            onChange={(event) => setPassword(event.target.value)}
          />
        )}
        {signIn && viaProvider && (
          <Notice title={`Sign in again with ${session?.oidc?.label ?? "your provider"}`}>
            Then repeat this.
          </Notice>
        )}
        {failure && <Notice state="bad">{failure}</Notice>}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Cancel</Button>
          {signIn && viaProvider ? (
            <a href={back} className={buttonClass("primary")}>
              Continue
            </a>
          ) : (
            <Button tone={tone} type="submit" disabled={busy || blocked}>
              {busy ? "Working" : action}
            </Button>
          )}
        </div>
      </form>
    </Modal>
  );
}
