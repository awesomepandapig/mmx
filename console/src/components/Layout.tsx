import { useEffect, useState } from "react";
import { NavLink, Outlet, useLocation, useNavigate, useSearchParams } from "react-router-dom";
import { api, messageOf } from "../lib/api";
import type { Session, Status as ExchangeStatus } from "../lib/api";
import { usePolled } from "../lib/hooks";
import { useCan, useSession } from "../lib/session";
import { MarketSearch } from "./MarketSearch";
import { Button, Notice, Status, Tag, cn } from "./ui";
import type { State } from "./ui";

export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cn("font-display text-[25px] font-semibold tracking-[-0.01em]", className)}>
      MMX
    </span>
  );
}

function useTheme() {
  const [dark, setDark] = useState(() => document.documentElement.classList.contains("dark"));
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark);
  }, [dark]);
  return {
    dark,
    toggle: () => {
      const next = !dark;
      setDark(next);
      try {
        localStorage.setItem("mmx-theme", next ? "dark" : "light");
      } catch {
        // Without storage the choice just lasts until reload.
      }
    },
  };
}

const verdicts: Record<ExchangeStatus["verdict"], { state: State; text: string }> = {
  ok: { state: "good", text: "Healthy" },
  degraded: { state: "warn", text: "Degraded" },
  down: { state: "bad", text: "Down" },
};

function Nav({ onPick }: { onPick?: () => void }) {
  const admin = useCan("admin");
  const pages = [
    { to: "/", label: "Overview", end: true },
    { to: "/markets", label: "Markets" },
    { to: "/activity", label: "Activity" },
    { to: "/webhooks", label: "Webhooks" },
    ...(admin ? [{ to: "/settings", label: "Settings" }] : []),
  ];
  return (
    <nav aria-label="Console" className="flex flex-col gap-0.5">
      {pages.map((page) => (
        <NavLink
          key={page.to}
          to={page.to}
          end={page.end}
          onClick={onPick}
          className={({ isActive }) =>
            cn(
              "rounded-lg px-3 py-1.5 text-sm font-medium transition-colors",
              isActive ? "bg-raised text-ink" : "text-muted hover:text-ink",
            )
          }
        >
          {page.label}
        </NavLink>
      ))}
    </nav>
  );
}

function Account({ session }: { session: Session | null }) {
  const { accept } = useSession();
  const { dark, toggle } = useTheme();
  const navigate = useNavigate();
  const [failure, setFailure] = useState<string | null>(null);

  async function signOut() {
    try {
      const next = await api.post<Session>("/logout");
      // Marked, or a login page with only a provider would sign straight back in.
      navigate("/login?signed_out=1", { replace: true });
      accept(next);
    } catch (error) {
      setFailure(messageOf(error));
    }
  }

  return (
    <div className="border-t border-line p-3">
      <div className="flex items-center gap-2 px-1">
        <p className="min-w-0 flex-1 truncate text-[13px] text-muted" title={session?.user ?? ""}>
          {session?.user}
        </p>
        {session?.role && <Tag className="capitalize">{session.role}</Tag>}
      </div>
      <div className="mt-2 flex gap-1">
        <Button tone="quiet" className="h-8 flex-1 px-2 text-[13px]" onClick={toggle}>
          {dark ? "Light" : "Dark"}
        </Button>
        <Button tone="quiet" className="h-8 flex-1 px-2 text-[13px]" onClick={() => void signOut()}>
          Sign out
        </Button>
      </div>
      {failure && <p className="mt-1 text-xs text-bad">{failure}</p>}
    </div>
  );
}

/** What came back from a trip to the provider to confirm a destructive action. */
function ReauthResult() {
  const [params, setParams] = useSearchParams();
  const error = params.get("reauth_error");
  const done = params.get("reauthed");
  const { load } = useSession();
  useEffect(() => {
    if (done) void load();
  }, [done, load]);
  if (!error && !done) return null;
  function dismiss() {
    params.delete("reauth_error");
    params.delete("reauthed");
    setParams(params, { replace: true });
  }
  const close = (
    <Button className="h-8 px-2.5 text-[13px]" onClick={dismiss}>
      Dismiss
    </Button>
  );
  return (
    <div className="px-4 pt-3 lg:px-6">
      {error ? (
        <Notice state="bad" title="Not confirmed" action={close}>
          {error}
        </Notice>
      ) : (
        <Notice
          state="good"
          title="Confirmed. Repeat the action within 5 minutes."
          action={close}
        />
      )}
    </div>
  );
}

export function Layout() {
  const { session } = useSession();
  const status = usePolled(() => api.get<ExchangeStatus>("/status"), 10000);
  const [menu, setMenu] = useState(false);
  const { pathname } = useLocation();
  useEffect(() => setMenu(false), [pathname]);

  const verdict = status.data ? verdicts[status.data.verdict] : null;

  return (
    <div className="flex h-dvh min-h-0">
      <aside className="hidden w-[208px] flex-none flex-col border-r border-line lg:flex">
        <div className="flex h-14 flex-none items-center px-4">
          <Wordmark />
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-2 pt-2">
          <Nav />
        </div>
        <Account session={session} />
      </aside>

      {menu && (
        <div className="fixed inset-x-0 bottom-0 top-14 z-30 flex flex-col border-t border-line bg-canvas lg:hidden">
          <div className="flex-1 overflow-y-auto p-2">
            <Nav onPick={() => setMenu(false)} />
          </div>
          <Account session={session} />
        </div>
      )}

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-14 flex-none items-center gap-3 border-b border-line px-4 lg:px-6">
          <Wordmark className="lg:hidden" />
          <MarketSearch className="min-w-0 max-w-md flex-1" />
          <div className="ml-auto flex items-center gap-3">
            {verdict ? (
              <NavLink to="/" aria-label="Exchange status" className="hidden sm:block">
                <Status state={verdict.state}>{verdict.text}</Status>
              </NavLink>
            ) : (
              status.error && (
                <Status state="bad" className="hidden sm:inline-flex">
                  Unreachable
                </Status>
              )
            )}
            <Button
              tone="quiet"
              className="lg:hidden"
              aria-expanded={menu}
              onClick={() => setMenu(!menu)}
            >
              Menu
            </Button>
          </div>
        </header>
        <ReauthResult />
        <main className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
