import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { api, messageOf, setCsrfToken, setSignedOutHandler } from "./api";
import type { Session } from "./api";

interface SessionState {
  session: Session | null;
  error: string | null;
  load: () => Promise<void>;
  accept: (session: Session) => void;
}

const Context = createContext<SessionState | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState<string | null>(null);

  const accept = useCallback((next: Session) => {
    setCsrfToken(next.csrf_token);
    setSession(next);
    setError(null);
  }, []);

  const load = useCallback(async () => {
    try {
      accept(await api.get<Session>("/session"));
    } catch (failure) {
      setError(messageOf(failure));
    }
  }, [accept]);

  useEffect(() => {
    setSignedOutHandler(() => void load());
    void load();
  }, [load]);

  const value = useMemo(() => ({ session, error, load, accept }), [session, error, load, accept]);
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useSession(): SessionState {
  const state = useContext(Context);
  if (!state) throw new Error("useSession needs a SessionProvider");
  return state;
}
