const pad = (value: number, width = 2) => String(value).padStart(width, "0");

function parsed(time: string | number) {
  const date = new Date(time);
  return Number.isNaN(date.getTime()) ? null : date;
}

/** `2026-11-06 16:47:36.120`. Every time on screen is UTC, and a column or label says so. */
export function utc(time: string | number) {
  const date = parsed(time);
  if (!date) return String(time);
  return `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())} ${utcClock(date.getTime())}.${pad(date.getUTCMilliseconds(), 3)}`;
}

/** `16:47:36`. */
export function utcClock(time: string | number) {
  const date = parsed(time);
  if (!date) return String(time);
  return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}`;
}

/** `16:47`, for chart axes. */
export function utcMinute(time: number) {
  const date = new Date(time);
  return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}`;
}

export function duration(ms: number) {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ${minutes % 60}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

export function ago(time: string | number, now = Date.now()) {
  const date = parsed(time);
  return date ? `${duration(now - date.getTime())} ago` : "-";
}

export function count(value: number) {
  return value.toLocaleString("en-US");
}

export function rate(value: number) {
  if (value >= 1000) return `${(value / 1000).toFixed(1)}k`;
  return value >= 10 ? Math.round(value).toString() : value.toFixed(1).replace(/\.0$/, "");
}

export function bytes(value: number) {
  if (value < 1024) return `${value} B`;
  if (value < 1024 ** 2) return `${(value / 1024).toFixed(1)} KB`;
  if (value < 1024 ** 3) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  return `${(value / 1024 ** 3).toFixed(2)} GB`;
}

export function millis(value: number) {
  return value >= 100 ? `${Math.round(value)} ms` : `${Number(value.toFixed(1))} ms`;
}

/** Prices to the market's tick, so a column of them lines up. */
export function priced(price: string, tick: string) {
  const places = (tick.split(".")[1] ?? "").length;
  const [whole, fraction = ""] = price.split(".");
  return places ? `${whole}.${fraction.padEnd(places, "0")}` : price;
}
