import type { TapeRow } from "./api";

/** Orders and trades are separate views; every execution report has its own ID. */
export function rowKey(row: TapeRow) {
  return `${row.kind}-${row.exec_id}`;
}

/** Reconnects replay the tail. Keep each execution once, with bounded retained history. */
export function mergeTapeRows(fresh: TapeRow[], current: TapeRow[], keep: number): TapeRow[] {
  const unique = new Map<string, TapeRow>();
  for (const row of [...fresh, ...current]) {
    const key = rowKey(row);
    if (!unique.has(key)) unique.set(key, row);
  }
  return [...unique.values()]
    .sort((left, right) =>
      left.time < right.time
        ? 1
        : left.time > right.time
          ? -1
          : right.exec_id - left.exec_id || left.kind.localeCompare(right.kind),
    )
    .slice(0, keep);
}
