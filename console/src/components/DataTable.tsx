import * as Popover from "@radix-ui/react-popover";
import {
  flexRender,
  getCoreRowModel,
  getSortedRowModel,
  useReactTable,
} from "@tanstack/react-table";
import type {
  ColumnDef,
  ColumnOrderState,
  ColumnSizingState,
  OnChangeFn,
  RowData,
  SortingState,
  VisibilityState,
} from "@tanstack/react-table";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";
import { Button, EmptyState, ErrorState, Skeleton, cn } from "./ui";

declare module "@tanstack/react-table" {
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  interface ColumnMeta<TData extends RowData, TValue> {
    /** Names the column in the chooser. Defaults to the header. */
    label?: string;
    align?: "right";
    mono?: boolean;
    /** Off until someone turns it on in the chooser. */
    hidden?: boolean;
    /** Always shown, because a row means nothing without it. */
    required?: boolean;
  }
}

interface Saved {
  sizing?: ColumnSizingState;
  visibility?: VisibilityState;
  order?: ColumnOrderState;
  sorting?: SortingState;
}

const key = (id: string) => `mmx.table.${id}`;

function load(id: string): Saved {
  try {
    return JSON.parse(localStorage.getItem(key(id)) ?? "{}") as Saved;
  } catch {
    return {};
  }
}

const MIN_WIDTH = 48;
const MAX_FIT = 900;
/** Rows drawn beyond the visible ones, so a quick scroll doesn't show a gap. */
const OVERSCAN = 10;

function GearIcon() {
  return (
    <svg aria-hidden viewBox="0 0 16 16" className="h-3.5 w-3.5 fill-none stroke-current">
      <path d="M2 4h7M12 4h2M2 12h2M7 12h7" strokeWidth="1.5" strokeLinecap="round" />
      <circle cx="10.5" cy="4" r="1.5" strokeWidth="1.5" />
      <circle cx="5.5" cy="12" r="1.5" strokeWidth="1.5" />
    </svg>
  );
}

/**
 * The console's one table. Every value shows in full by default, and columns drag wider or narrower,
 * reorder, sort and hide, with the choices kept per table. Long lists draw only the rows in view,
 * and the caller bounds what it passes in: a live list is capped and anything larger is paged on
 * the server, with `onSorting` handing sorting to the server too.
 */
export function DataTable<T>({
  id,
  columns,
  data,
  rowId,
  title,
  toolbar,
  footer,
  sorting: controlled,
  onSorting,
  defaultSorting,
  virtual = false,
  onRowClick,
  selected,
  loading = false,
  error,
  onRetry,
  empty = "Nothing here",
  emptyDetail,
  className,
  rowHeight = 28,
}: {
  id: string;
  columns: ColumnDef<T, never>[];
  data: T[];
  rowId: (row: T) => string;
  title?: ReactNode;
  toolbar?: ReactNode;
  footer?: ReactNode;
  sorting?: SortingState;
  onSorting?: OnChangeFn<SortingState>;
  defaultSorting?: SortingState;
  virtual?: boolean;
  onRowClick?: (row: T) => void;
  selected?: string | null;
  loading?: boolean;
  error?: string | null;
  onRetry?: () => void;
  empty?: string;
  emptyDetail?: ReactNode;
  className?: string;
  rowHeight?: number;
}) {
  const saved = useRef(load(id));
  const [sizing, setSizing] = useState<ColumnSizingState>(saved.current.sizing ?? {});
  const [visibility, setVisibility] = useState<VisibilityState>(() => {
    const defaults: VisibilityState = {};
    for (const column of columns) {
      if (column.meta?.hidden && column.id) defaults[column.id] = false;
    }
    return { ...defaults, ...saved.current.visibility };
  });
  const [order, setOrder] = useState<ColumnOrderState>(saved.current.order ?? []);
  const [ownSorting, setOwnSorting] = useState<SortingState>(
    saved.current.sorting ?? defaultSorting ?? [],
  );
  const sorting = controlled ?? ownSorting;

  useEffect(() => {
    const value: Saved = { sizing, visibility, order, sorting: controlled ? undefined : sorting };
    try {
      localStorage.setItem(key(id), JSON.stringify(value));
    } catch {
      // Without storage the choices last until reload.
    }
  }, [id, sizing, visibility, order, sorting, controlled]);

  const table = useReactTable<T>({
    data,
    columns,
    getRowId: rowId,
    state: { columnSizing: sizing, columnVisibility: visibility, columnOrder: order, sorting },
    defaultColumn: { minSize: MIN_WIDTH, size: 120 },
    columnResizeMode: "onChange",
    enableSortingRemoval: false,
    manualSorting: !!onSorting,
    onColumnSizingChange: setSizing,
    onColumnVisibilityChange: setVisibility,
    onColumnOrderChange: setOrder,
    onSortingChange: onSorting ?? setOwnSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
  });

  const box = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ top: 0, height: 600 });
  const newest = useRef<string | null>(null);

  useLayoutEffect(() => {
    const element = box.current;
    if (!element) return;
    const measure = () => setView({ top: element.scrollTop, height: element.clientHeight || 600 });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const rows = table.getRowModel().rows;

  // Newest rows arrive at the top. Someone scrolled down keeps their place.
  useLayoutEffect(() => {
    if (!virtual) return;
    const element = box.current;
    const previous = newest.current;
    newest.current = rows[0]?.id ?? null;
    if (!element || !previous || element.scrollTop === 0) return;
    const added = rows.findIndex((row) => row.id === previous);
    if (added > 0) element.scrollTop += added * rowHeight;
  });

  const first = virtual ? Math.max(0, Math.floor(view.top / rowHeight) - OVERSCAN) : 0;
  const last = virtual
    ? Math.min(rows.length, Math.ceil((view.top + view.height) / rowHeight) + OVERSCAN)
    : rows.length;
  const shown = rows.slice(first, last);
  const columnsShown = table.getVisibleLeafColumns();
  const total = table.getTotalSize();

  function move(columnId: string, onto: string) {
    if (columnId === onto) return;
    const ids = table.getAllLeafColumns().map((column) => column.id);
    const next = ids.filter((name) => name !== columnId);
    next.splice(next.indexOf(onto), 0, columnId);
    setOrder(next);
  }

  /** Makes a column as wide as its widest row in view. */
  function fit(columnId: string) {
    const root = box.current;
    if (!root) return;
    let widest = 0;
    for (const cell of root.querySelectorAll<HTMLElement>(`[data-col="${columnId}"] > span`)) {
      widest = Math.max(widest, cell.scrollWidth);
    }
    setSizing({ ...sizing, [columnId]: Math.min(MAX_FIT, Math.max(MIN_WIDTH, widest + 26)) });
  }

  function onHandleKey(event: KeyboardEvent, columnId: string, size: number) {
    const step = event.shiftKey ? 40 : 10;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      event.preventDefault();
      const next = size + (event.key === "ArrowRight" ? step : -step);
      setSizing({ ...sizing, [columnId]: Math.max(MIN_WIDTH, next) });
    }
  }

  const headers = table.getHeaderGroups()[0]?.headers ?? [];
  const hasBar = title !== undefined || toolbar !== undefined;

  return (
    <section data-table={id} className={cn("flex min-h-0 min-w-0 flex-col bg-surface", className)}>
      {hasBar && (
        <div className="flex min-h-[36px] flex-none flex-wrap items-center gap-2 border-b border-hairline px-3 py-1">
          {title !== undefined && <h2 className="text-[12px] font-semibold">{title}</h2>}
          {toolbar}
          <div className="ml-auto flex items-center gap-2">
            {footer}
            <ColumnChooser
              columns={table.getAllLeafColumns().map((column) => ({
                id: column.id,
                label:
                  column.columnDef.meta?.label ??
                  (typeof column.columnDef.header === "string"
                    ? column.columnDef.header
                    : column.id),
                required: !!column.columnDef.meta?.required,
                visible: column.getIsVisible(),
                toggle: (on: boolean) => column.toggleVisibility(on),
              }))}
              onMove={(columnId, by) => {
                const ids = table.getAllLeafColumns().map((column) => column.id);
                const at = ids.indexOf(columnId);
                const to = at + by;
                if (to < 0 || to >= ids.length) return;
                ids.splice(at, 1);
                ids.splice(to, 0, columnId);
                setOrder(ids);
              }}
              onReset={() => {
                setSizing({});
                setOrder([]);
                const defaults: VisibilityState = {};
                for (const column of columns) {
                  if (column.meta?.hidden && column.id) defaults[column.id] = false;
                }
                setVisibility(defaults);
              }}
            />
          </div>
        </div>
      )}
      <div
        ref={box}
        onScroll={(event) => {
          const { scrollTop, clientHeight } = event.currentTarget;
          if (virtual) setView({ top: scrollTop, height: clientHeight });
        }}
        className="min-h-0 flex-1 overflow-auto"
      >
        <table
          className="border-separate border-spacing-0"
          style={{ tableLayout: "fixed", width: total, minWidth: "100%" }}
        >
          <colgroup>
            {columnsShown.map((column) => (
              <col key={column.id} style={{ width: column.getSize() }} />
            ))}
            <col />
          </colgroup>
          <thead className="sticky top-0 z-10">
            <tr>
              {headers.map((header) => {
                const meta = header.column.columnDef.meta;
                const sort = header.column.getIsSorted();
                const sortable = header.column.getCanSort();
                return (
                  <th
                    key={header.id}
                    scope="col"
                    aria-sort={
                      sort === "asc" ? "ascending" : sort === "desc" ? "descending" : undefined
                    }
                    onDragOver={(event) => event.preventDefault()}
                    onDrop={(event) => {
                      event.preventDefault();
                      move(event.dataTransfer.getData("text/mmx-column"), header.column.id);
                    }}
                    className="relative h-7 select-none border-b border-line bg-raised p-0 text-left text-[11px] font-semibold text-muted"
                  >
                    <button
                      type="button"
                      draggable
                      onDragStart={(event) =>
                        event.dataTransfer.setData("text/mmx-column", header.column.id)
                      }
                      disabled={!sortable}
                      onClick={header.column.getToggleSortingHandler()}
                      className={cn(
                        "flex h-full w-full items-center gap-1 overflow-hidden whitespace-nowrap px-2 hover:text-ink disabled:cursor-default disabled:hover:text-muted",
                        meta?.align === "right" && "justify-end",
                        sort && "text-ink",
                      )}
                    >
                      {flexRender(header.column.columnDef.header, header.getContext())}
                      {sort && <span aria-hidden>{sort === "asc" ? "↑" : "↓"}</span>}
                    </button>
                    {/* eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex */}
                    <div
                      role="separator"
                      aria-orientation="vertical"
                      aria-label={`Resize ${meta?.label ?? header.column.id}`}
                      aria-valuenow={header.getSize()}
                      tabIndex={0}
                      data-active={header.column.getIsResizing()}
                      onMouseDown={header.getResizeHandler()}
                      onTouchStart={header.getResizeHandler()}
                      onDoubleClick={() => fit(header.column.id)}
                      onKeyDown={(event) => onHandleKey(event, header.column.id, header.getSize())}
                      className="resize-grip"
                    />
                  </th>
                );
              })}
              <th aria-hidden className="border-b border-line bg-raised p-0" />
            </tr>
          </thead>
          <tbody>
            {first > 0 && (
              <tr aria-hidden style={{ height: first * rowHeight }}>
                <td colSpan={columnsShown.length + 1} />
              </tr>
            )}
            {shown.map((row) => (
              <tr
                key={row.id}
                style={{ height: rowHeight }}
                tabIndex={onRowClick ? 0 : undefined}
                aria-selected={selected === row.id || undefined}
                onClick={onRowClick ? () => onRowClick(row.original) : undefined}
                onKeyDown={
                  onRowClick
                    ? (event) => {
                        if (event.key === "Enter") onRowClick(row.original);
                      }
                    : undefined
                }
                className={cn(
                  onRowClick && "cursor-pointer hover:bg-raised",
                  selected === row.id && "bg-raised",
                )}
              >
                {row.getVisibleCells().map((cell) => {
                  const meta = cell.column.columnDef.meta;
                  const value = cell.getValue();
                  return (
                    <td
                      key={cell.id}
                      data-col={cell.column.id}
                      title={typeof value === "string" ? value : undefined}
                      className={cn(
                        "overflow-hidden text-ellipsis whitespace-nowrap border-b border-hairline px-2",
                        meta?.align === "right" && "tabular text-right",
                        meta?.mono && "tabular font-code text-[12px]",
                      )}
                    >
                      <span className="inline-block">
                        {flexRender(cell.column.columnDef.cell, cell.getContext())}
                      </span>
                    </td>
                  );
                })}
                <td className="border-b border-hairline" />
              </tr>
            ))}
            {last < rows.length && (
              <tr aria-hidden style={{ height: (rows.length - last) * rowHeight }}>
                <td colSpan={columnsShown.length + 1} />
              </tr>
            )}
          </tbody>
        </table>
        {error && rows.length === 0 ? (
          <ErrorState title="Can't read this" detail={error} onRetry={onRetry} />
        ) : loading ? (
          <div role="status" aria-label="Loading" className="divide-y divide-hairline">
            {Array.from({ length: 8 }, (_, index) => (
              <div key={index} className="flex h-7 items-center gap-6 px-2">
                <Skeleton className="h-3 w-24" />
                <Skeleton className="h-3 w-16" />
                <Skeleton className="h-3 w-40" />
              </div>
            ))}
          </div>
        ) : rows.length === 0 ? (
          <EmptyState title={empty}>{emptyDetail}</EmptyState>
        ) : null}
      </div>
    </section>
  );
}

interface ChooserColumn {
  id: string;
  label: string;
  required: boolean;
  visible: boolean;
  toggle: (on: boolean) => void;
}

function ColumnChooser({
  columns,
  onMove,
  onReset,
}: {
  columns: ChooserColumn[];
  onMove: (id: string, by: number) => void;
  onReset: () => void;
}) {
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <Button tone="quiet" className="h-[26px] px-1.5" aria-label="Columns">
          <GearIcon />
          <span className="text-[12px]">Columns</span>
        </Button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="end"
          sideOffset={4}
          className="z-50 max-h-[70dvh] w-56 overflow-y-auto border border-line bg-surface py-1 text-[13px] text-ink shadow-xl"
        >
          <ul>
            {columns.map((column, index) => (
              <li key={column.id} className="flex h-7 items-center gap-1 px-2 hover:bg-raised">
                <label className="flex min-w-0 flex-1 items-center gap-2">
                  <input
                    type="checkbox"
                    checked={column.visible}
                    disabled={column.required}
                    onChange={(event) => column.toggle(event.target.checked)}
                    className="accent-[rgb(var(--accent))]"
                  />
                  <span className="truncate">{column.label}</span>
                </label>
                <button
                  type="button"
                  aria-label={`Move ${column.label} earlier`}
                  disabled={index === 0}
                  onClick={() => onMove(column.id, -1)}
                  className="px-1 text-faint hover:text-ink disabled:opacity-30"
                >
                  ↑
                </button>
                <button
                  type="button"
                  aria-label={`Move ${column.label} later`}
                  disabled={index === columns.length - 1}
                  onClick={() => onMove(column.id, 1)}
                  className="px-1 text-faint hover:text-ink disabled:opacity-30"
                >
                  ↓
                </button>
              </li>
            ))}
          </ul>
          <div className="mt-1 border-t border-hairline px-2 pt-1">
            <button type="button" className="text-[12px] text-accent" onClick={onReset}>
              Reset widths, order and columns
            </button>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/** Typed helper, because the table holds columns of mixed value types. */
export function useColumns<T>(make: () => ColumnDef<T, never>[], deps: unknown[]) {
  // eslint-disable-next-line react-hooks/exhaustive-deps
  return useMemo(make, deps);
}
