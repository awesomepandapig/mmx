import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "./index.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Outlet, Route, Routes } from "react-router-dom";
import { Layout } from "./components/Layout";
import { Notice } from "./components/ui";
import { SessionProvider, useSession } from "./lib/session";
import { Activity } from "./pages/Activity";
import { Login } from "./pages/Login";
import { Markets } from "./pages/Markets";
import { Overview } from "./pages/Overview";
import { Settings } from "./pages/Settings";
import { Setup } from "./pages/Setup";
import { Webhooks } from "./pages/Webhooks";

function Guard() {
  const { session, error } = useSession();
  if (!session) {
    return error ? (
      <div className="mx-auto max-w-md p-8">
        <Notice state="bad" title="Can't load the console">
          {error}
        </Notice>
      </div>
    ) : null;
  }
  if (!session.authenticated) return <Navigate to="/login" replace />;
  if (session.must_set_password) return <Navigate to="/setup" replace />;
  return <Outlet />;
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
              <Route path="activity" element={<Activity />} />
              <Route path="webhooks" element={<Webhooks />} />
              <Route path="settings" element={<Settings />} />
            </Route>
          </Route>
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </SessionProvider>
    </BrowserRouter>
  </StrictMode>,
);
