const PROD = process.env.NODE_ENV === "production";

export const DOCS_URL =
  import.meta.env.VITE_DOCS_URL ||
  (PROD ? "https://docs.mmx.exchange" : "http://localhost:5174");
