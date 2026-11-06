import "@fontsource-variable/funnel-display";
import "@fontsource-variable/rethink-sans";
import "@fontsource-variable/jetbrains-mono";
import "./index.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { Layout } from "./components/Layout";
import { ErrorState } from "./components/ui";
import { SessionProvider, useSession } from "./lib/session";
import { Activity } from "./pages/Activity";
import { Login } from "./pages/Login";
import { MarketPage } from "./pages/Market";
import { Markets } from "./pages/Markets";
import { NotFound } from "./pages/NotFound";
import { Overview } from "./pages/Overview";
import { Settings } from "./pages/Settings";
import { Setup } from "./pages/Setup";
import { Webhooks } from "./pages/Webhooks";

function Guard() {
  const { session, error, load } = useSession();
  const { pathname, search } = useLocation();
  if (!session) {
    return error ? (
      <main className="mx-auto max-w-md p-8">
        <ErrorState title="Can't reach mmx serve" detail={error} onRetry={() => void load()} />
      </main>
    ) : null;
  }
  if (!session.authenticated) {
    const back = pathname === "/" ? "" : `?return=${encodeURIComponent(pathname + search)}`;
    return <Navigate to={`/login${back}`} replace />;
  }
  if (session.must_set_password) return <Navigate to="/setup" replace />;
  return <Outlet />;
}

/** Settings need the admin role. The server refuses them anyway. */
function AdminOnly() {
  const { session } = useSession();
  return session?.role === "admin" ? <Outlet /> : <Navigate to="/" replace />;
}

const root = document.getElementById("root");
if (!root) throw new Error("no root element");

createRoot(root).render(
  <StrictMode>
    <BrowserRouter>
      <SessionProvider>
        <Routes>
          <Route path="/login" element={<Login />} />
          <Route path="/setup" element={<Setup />} />
          <Route element={<Guard />}>
            <Route element={<Layout />}>
              <Route index element={<Overview />} />
              <Route path="markets" element={<Markets />} />
              <Route path="markets/:symbol" element={<MarketPage />} />
              <Route path="activity" element={<Activity />} />
              <Route path="webhooks" element={<Webhooks />} />
              <Route element={<AdminOnly />}>
                <Route path="settings" element={<Settings />} />
              </Route>
              <Route path="*" element={<NotFound />} />
            </Route>
          </Route>
        </Routes>
      </SessionProvider>
    </BrowserRouter>
  </StrictMode>,
);
