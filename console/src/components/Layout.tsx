import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { useEffect, useState } from "react";
import { Link, NavLink, Outlet, useLocation, useNavigate, useSearchParams } from "react-router-dom";
import { api, messageOf } from "../lib/api";
import type { Session } from "../lib/api";
import { ExchangeProvider, useExchange } from "../lib/exchange";
import { ago, utcClock } from "../lib/format";
import { useCan, useSession } from "../lib/session";
import { Button, Notice, buttonClass, cn } from "./ui";

export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cn("font-display text-[19px] font-semibold tracking-[-0.01em]", className)}>
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

function Nav() {
  const admin = useCan("admin");
  const { pathname } = useLocation();
  const pages = [
    { to: "/", label: "Overview", end: true },
    { to: "/markets", label: "Markets", also: ["/all-markets", "/markets-file"] },
    { to: "/cluster", label: "Cluster" },
    { to: "/activity", label: "Activity" },
    { to: "/webhooks", label: "Webhooks" },
    ...(admin ? [{ to: "/settings", label: "Settings" }] : []),
  ];
  return (
    <nav aria-label="Console" className="flex h-full min-w-0 items-stretch overflow-x-auto">
      {pages.map((page) => (
        <NavLink
          key={page.to}
          to={page.to}
          end={page.end}
          className={({ isActive }) => {
            const active = isActive || page.also?.includes(pathname);
            return cn(
              "flex items-center whitespace-nowrap border-b-2 px-2.5 text-[15px] font-medium",
              active ? "border-accent text-ink" : "border-transparent text-muted hover:text-ink",
            );
          }}
        >
          {page.label}
        </NavLink>
      ))}
    </nav>
  );
}

function Clock() {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  return (
    <time className="tabular hidden whitespace-nowrap font-code text-[14px] text-muted sm:block">
      {utcClock(now)} UTC
    </time>
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

  const item = "cursor-pointer px-3 py-1 outline-none data-[highlighted]:bg-raised";
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <Button tone="quiet" aria-label="Account" className="max-w-[200px]">
          <span className={cn("truncate", failure && "text-bad")}>{session?.user}</span>
          <span className="capitalize text-faint">{session?.role}</span>
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          className="z-50 w-44 border border-line bg-surface py-1 text-[15px] text-ink shadow-xl"
        >
          <DropdownMenu.Item className={item} onSelect={toggle}>
            {dark ? "Light theme" : "Dark theme"}
          </DropdownMenu.Item>
          <DropdownMenu.Item className={item} onSelect={() => void signOut()}>
            Sign out
          </DropdownMenu.Item>
          {failure && <p className="px-3 py-1 text-[14px] text-bad">{failure}</p>}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
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
  const close = <Button onClick={dismiss}>Dismiss</Button>;
  return error ? (
    <Notice state="bad" title="Not confirmed" action={close} className="border-x-0 border-t-0">
      {error}
    </Notice>
  ) : (
    <Notice
      title="Confirmed. Repeat the action within 5 minutes."
      action={close}
      className="border-x-0 border-t-0"
    />
  );
}

/**
 * The one place the exchange's health is announced on every page. It says nothing while all is well,
 * so when it appears it means something.
 */
function ExchangeBanner() {
  const { status, error, asOf } = useExchange();
  const link = (
    <Link to="/cluster" className={buttonClass("plain")}>
      Open cluster
    </Link>
  );
  const bar = "border-x-0 border-t-0 px-3";
  if (error) {
    return (
      <Notice
        state="bad"
        title="Can't reach mmx serve. Everything here is out of date."
        className={bar}
      >
        {asOf ? `As of ${utcClock(asOf)} UTC, ${ago(asOf)}. ` : ""}
        {error}
      </Notice>
    );
  }
  if (!status || status.verdict === "ok") return null;
  const down = status.verdict === "down";
  const first = status.conditions[0];
  const more = status.conditions.length - 1;
  return (
    <Notice
      state={down ? "bad" : "warn"}
      title={down ? "Cannot trade. Gateways are refusing orders." : "Degraded"}
      action={link}
      className={bar}
    >
      {first && (
        <>
          {first.subject}: {first.check} failed since {utcClock(first.since)} UTC (
          {ago(first.since)}){more > 0 ? `, and ${more} more` : ""}.
        </>
      )}
    </Notice>
  );
}

export function Layout() {
  const { session } = useSession();
  const navigate = useNavigate();

  // Anywhere in the console, / or Cmd+K goes to the market picker.
  useEffect(() => {
    function keys(event: KeyboardEvent) {
      const target = event.target as HTMLElement;
      const typing = /INPUT|TEXTAREA|SELECT/.test(target.tagName);
      if (
        (event.key === "k" && (event.metaKey || event.ctrlKey)) ||
        (event.key === "/" && !typing)
      ) {
        event.preventDefault();
        if (!document.querySelector("[data-market-picker]")) {
          navigate("/markets?pick=1");
        } else {
          window.dispatchEvent(new Event("mmx-open-picker"));
        }
      }
    }
    window.addEventListener("keydown", keys);
    return () => window.removeEventListener("keydown", keys);
  }, [navigate]);

  return (
    <ExchangeProvider>
      <div className="flex h-dvh min-h-0 flex-col">
        <header className="flex h-10 flex-none items-center gap-3 border-b border-line bg-surface pl-3 pr-2">
          <Link to="/" aria-label="Overview" className="flex-none">
            <Wordmark />
          </Link>
          <Nav />
          <div className="ml-auto flex flex-none items-center gap-2">
            <Clock />
            <Account session={session} />
          </div>
        </header>
        <ExchangeBanner />
        <ReauthResult />
        <main className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          <Outlet />
        </main>
      </div>
    </ExchangeProvider>
  );
}
