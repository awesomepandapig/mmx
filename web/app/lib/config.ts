export const PROD = process.env.NODE_ENV === "production";

const BASE_DOMAIN = PROD ? "skyblock.exchange" : "localhost";

export const API_VERSION = 'v1';

export const DOMAIN = import.meta.env.VITE_DOMAIN ?? (PROD
  ? `https://${BASE_DOMAIN}`
  : `http://${BASE_DOMAIN}:5173`);

export const API_URL = import.meta.env.VITE_API_URL ?? (PROD
  ? `https://api.${BASE_DOMAIN}/api`
  : `http://${BASE_DOMAIN}:8000/api`);

export const AUTH_URL = import.meta.env.VITE_AUTH_URL ?? (PROD
  ? `https://auth.${BASE_DOMAIN}/api`
  : `http://${BASE_DOMAIN}:8001/api`);

export const WS_URL = import.meta.env.VITE_WS_URL ?? (PROD
  ? `wss://advanced-trade-ws.${BASE_DOMAIN}`
  : `ws://${BASE_DOMAIN}:8080`);

export const DOCS_URL = import.meta.env.VITE_DOCS_URL ?? (PROD
  ? `https://docs.${BASE_DOMAIN}`
  : `http://${BASE_DOMAIN}:5174`);
