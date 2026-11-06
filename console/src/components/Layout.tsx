import { useEffect, useState } from "react";
import { NavLink, Outlet } from "react-router-dom";
import { api, messageOf } from "../lib/api";
import type { Session } from "../lib/api";
import { useSession } from "../lib/session";
import { Button, cn } from "./ui";

const pages = [
  { to: "/", label: "Overview", end: true },
  { to: "/markets", label: "Markets" },
  { to: "/activity", label: "Activity" },
  { to: "/webhooks", label: "Webhooks" },
  { to: "/settings", label: "Settings" },
];

export function Wordmark({ className }: { className?: string }) {
  return <span className={cn("text-xl font-bold tracking-tight", className)}>MMX</span>;
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

export function Layout() {
  const { session, accept } = useSession();
  const { dark, toggle } = useTheme();
  const [failure, setFailure] = useState<string | null>(null);

  async function signOut() {
    try {
      accept(await api.post<Session>("/logout"));
    } catch (error) {
      setFailure(messageOf(error));
    }
  }

  return (
    <div className="mx-auto flex min-h-dvh max-w-[1320px] flex-col md:flex-row">
      <aside className="border-b border-line p-4 md:sticky md:top-0 md:flex md:h-dvh md:w-56 md:shrink-0 md:flex-col md:border-b-0 md:border-r md:p-5">
        <Wordmark className="block px-3 pb-3 md:pb-6" />
        <nav
          aria-label="Console"
          className="-mx-1 flex gap-1 overflow-x-auto px-1 md:mx-0 md:flex-col md:overflow-visible md:px-0"
        >
          {pages.map((page) => (
            <NavLink
              key={page.to}
              to={page.to}
              end={page.end}
              className={({ isActive }) =>
                cn(
                  "shrink-0 rounded-full px-3 py-1.5 text-sm font-medium transition",
                  isActive ? "bg-ink text-canvas" : "text-muted hover:bg-raised hover:text-ink",
                )
              }
            >
              {page.label}
            </NavLink>
          ))}
        </nav>
        <div className="mt-3 flex items-center gap-2 px-3 md:mt-auto md:block md:space-y-2 md:px-0">
          <p
            className="min-w-0 flex-1 truncate text-xs text-muted md:px-3"
            title={session?.user ?? ""}
          >
            {session?.user}
          </p>
          <div className="flex gap-2">
            <Button className="h-8 px-3 text-xs" onClick={toggle}>
              {dark ? "Light" : "Dark"}
            </Button>
            <Button className="h-8 px-3 text-xs" onClick={() => void signOut()}>
              Sign out
            </Button>
          </div>
          {failure && <p className="text-xs text-bad">{failure}</p>}
        </div>
      </aside>
      <main className="min-w-0 flex-1 p-4 md:p-8">
        <Outlet />
      </main>
    </div>
  );
}
