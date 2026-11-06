export function clock(time: string) {
  const parsed = new Date(time);
  if (Number.isNaN(parsed.getTime())) return time;
  const text = parsed.toLocaleTimeString("en-GB");
  const millis = String(parsed.getMilliseconds()).padStart(3, "0");
  return `${text}.${millis}`;
}

export function dateTime(time: string) {
  const parsed = new Date(time);
  return Number.isNaN(parsed.getTime()) ? time : parsed.toLocaleString("en-GB");
}

/** The end of an ID, because a UUIDv7 starts with its timestamp, so the starts all look alike. */
export function short(id?: string) {
  return id ? id.slice(-8) : "-";
}

export function count(value: number) {
  return value.toLocaleString("en-US");
}

export function rate(value: number) {
  if (value >= 1000) return `${(value / 1000).toFixed(1)}k`;
  return value >= 10 ? Math.round(value).toString() : value.toFixed(1).replace(/\.0$/, "");
}
