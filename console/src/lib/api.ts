// Every call the console makes. The CLI uses the same routes, so nothing here is console-only.

export const BASE = "/admin/v1";

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
    public body?: unknown,
  ) {
    super(message);
  }
}

let csrfToken = "";
let onSignedOut: () => void = () => {};

export function setCsrfToken(token: string | null) {
  csrfToken = token ?? "";
}

export function setSignedOutHandler(handler: () => void) {
  onSignedOut = handler;
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (method !== "GET") headers["x-csrf-token"] = csrfToken;

  let response: Response;
  try {
    response = await fetch(`${BASE}${path}`, {
      method,
      headers,
      credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, "unreachable", "Can't reach mmx serve.");
  }

  const text = await response.text();
  let parsed: unknown = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  if (response.ok) return parsed as T;

  const fields = (parsed ?? {}) as { code?: string; message?: string; gateways?: unknown };
  // Per-gateway answers still say what happened, so a partial failure carries them along.
  if (fields.gateways) return parsed as T;
  if (response.status === 401 && path !== "/login") onSignedOut();
  throw new ApiError(
    response.status,
    fields.code ?? "failed",
    fields.message ?? (typeof parsed === "string" && parsed ? parsed : `HTTP ${response.status}`),
    parsed,
  );
}

export const api = {
  get: <T>(path: string) => call<T>("GET", path),
  post: <T>(path: string, body?: unknown) => call<T>("POST", path, body),
  put: <T>(path: string, body: unknown) => call<T>("PUT", path, body),
  delete: <T>(path: string) => call<T>("DELETE", path),
};

export function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export interface Session {
  authenticated: boolean;
  user: string | null;
  method: "password" | "oidc" | null;
  csrf_token: string | null;
  must_set_password: boolean;
  password_login: boolean;
  oidc: { label: string } | null;
  version: string;
  store: "local" | "kubernetes";
}

export interface ServiceStatus {
  name: string;
  target: string;
  required: boolean;
  reachable: boolean;
  ready: boolean;
  version?: string;
  error?: string;
}

export interface Status {
  verdict: "ok" | "degraded" | "down";
  version: string;
  services: ServiceStatus[];
}

export interface GatewayView {
  gateway: string;
  reachable: boolean;
  in_sync: boolean;
  error?: string;
  market?: { halted: boolean };
}

export interface Market {
  symbol: string;
  tick_size: string;
  min_size?: string;
  max_size?: string;
  enabled: boolean;
  halted: boolean;
  in_file: boolean;
  in_sync: boolean;
  gateways: GatewayView[];
}

export interface Delivery {
  mode: "immediate" | "eventual";
  state: "in_sync" | "out_of_sync" | "reaching_gateways" | "stalled";
  behind: string[];
  waited_secs?: number;
  expected_secs?: number;
}

export interface MarketList {
  markets: Market[];
  delivery: Delivery;
}

export interface GatewayResult {
  gateway: string;
  ok: boolean;
  status?: number;
  error?: string;
}

export interface EditResult {
  written: boolean;
  applied: boolean;
  state: "applied" | "reaching_gateways" | "failed";
  revision?: string;
  expected_secs?: number;
  gateways: GatewayResult[];
}

export interface ActionResult {
  symbol: string;
  action: string;
  ok: boolean;
  gateways: GatewayResult[];
}

export interface CancelResult {
  symbol: string;
  complete: boolean;
  requested: number;
  canceled: number;
  failed: number;
  truncated: boolean;
  gateways: GatewayResult[];
}

export interface Problem {
  message: string;
  line?: number;
}

export interface MarketsFile {
  yaml: string;
  revision: string;
  valid: boolean;
  problem: Problem | null;
  backend: "local" | "kubernetes";
  location: string;
  delivery: "immediate" | "eventual";
}

export interface Validation {
  valid: boolean;
  problem?: Problem;
  changes?: { added: string[]; removed: string[]; changed: string[] };
  warnings?: string[];
}

export interface OrderRow {
  time: string;
  symbol: string;
  exec_type?: string;
  status?: string;
  side: string;
  reason?: string;
  player?: string;
  client_order_id?: string;
  exec_id: number;
  price?: string;
  size?: string;
  filled_size?: string;
}

export interface TradeRow {
  time: string;
  symbol: string;
  side: string;
  player?: string;
  client_order_id?: string;
  match_id?: number;
  exec_id: number;
  price?: string;
  size?: string;
}

export interface WebhookPosition {
  state: "not_configured" | "halted" | "verifying" | "failing" | "delivering" | "idle";
  backlog: number;
  delivered_position: number;
  durable_position: number;
  consecutive_failures: number;
  halted: string | null;
}

export interface WebhookConfig {
  url: string | null;
  secret_hint: string | null;
  state: "applied" | "restart_required" | "unknown" | "incomplete" | "from_environment";
  running_url: string | null;
  running_error: string | null;
  restart: string;
  secret?: string;
}

export interface OidcConfig {
  enabled: boolean;
  issuer: string;
  client_id: string;
  display_name: string | null;
  allowed: string[];
  client_secret_hint: string | null;
  redirect_uri: string | null;
}

export interface Settings {
  public_url: string | null;
  password_login: boolean;
  oidc: OidcConfig;
  store: { backend: string; config: string; secrets: string; delivery: string };
  admin_token_set: boolean;
  version: string;
}
