import {
  type KeyboardEvent,
  type PointerEvent,
  type ReactNode,
  useRef,
  useState,
} from "react";
import { tokenize } from "~/components/landing/highlight";
import type {
  LogEntry,
  Payload,
  PayloadTab,
  Service,
  Simulator,
} from "./simulator";

const SERVICE_COLORS: Record<Service, string> = {
  api: "#ff7ac8",
  auth: "#ffc53d",
  engine: "#a3acff",
  outbox: "#a3acff",
  webhook: "#3be3a6",
  game: "#3be3a6",
};

const TABS: { id: PayloadTab; label: string; empty: string }[] = [
  {
    id: "request",
    label: "Request",
    empty: "Place an order to see the request your game server sends.",
  },
  {
    id: "response",
    label: "Response",
    empty: "Place an order to see what MMX returns.",
  },
  {
    id: "webhook",
    label: "Webhook",
    empty: "Fills arrive here once an order matches.",
  },
  {
    id: "ws",
    label: "WebSocket",
    empty: "Book updates for your order appear here.",
  },
];

const DEFAULT_PANEL = 262;
const MIN_PANEL = 41;
const MIN_LOG = 60;

function LogRow({ entry }: { entry: LogEntry }) {
  const resultColor =
    entry.tone === "bad"
      ? "text-[#ff7ac8]"
      : entry.tone === "good"
        ? "text-mmx-mint"
        : "text-[#8f92ad]";
  return (
    <div
      className={`grid min-h-7 flex-none grid-cols-[58px_1fr_auto] items-center gap-2.5 border-t border-[#20223a] text-[#dfe1f2] sm:grid-cols-[88px_66px_1fr_auto] ${
        entry.mine
          ? "bg-gradient-to-r from-mmx-raised to-transparent duration-200 animate-in fade-in slide-in-from-bottom-1 motion-reduce:animate-none"
          : "opacity-[.42]"
      }`}
    >
      <time
        dateTime={new Date(entry.at).toISOString()}
        className="hidden text-[11px] text-[#6f7290] sm:block"
      >
        {entry.at}
      </time>
      <b
        className="font-medium"
        style={{ color: SERVICE_COLORS[entry.service] }}
      >
        {entry.service}
      </b>
      <span className="min-w-0 truncate">{entry.message}</span>
      <em className={`not-italic ${resultColor}`}>{entry.result}</em>
    </div>
  );
}

function Json({ value }: { value: unknown }) {
  const tokens = tokenize(JSON.stringify(value, null, 2));
  return tokens.map(({ kind, text }, i) => {
    const isKey = kind === "string" && tokens[i + 1]?.text.startsWith(":");
    const color = isKey
      ? "text-mmx-periwinkle"
      : kind === "string"
        ? "text-mmx-mint"
        : kind === "number"
          ? "text-mmx-gold"
          : "";
    return (
      <span key={i} className={color}>
        {text}
      </span>
    );
  });
}

function PayloadView({ payload }: { payload: Payload }) {
  const lines: ReactNode[] = [];
  if (payload.method) {
    lines.push(
      <span key="start">
        <span className="text-[#ff7ac8]">{payload.method}</span> {payload.path}
        {payload.attempt ? (
          <span className="text-[#8f92ad]">{`  try ${payload.attempt}`}</span>
        ) : null}
        {"\n"}
      </span>,
    );
  }
  if (payload.status)
    lines.push(
      <span
        key="status"
        className="text-[#ff7ac8]"
      >{`${payload.status}\n`}</span>,
    );
  for (const [name, value] of Object.entries(payload.headers ?? {})) {
    lines.push(
      <span key={name}>
        <span className="text-mmx-periwinkle">{name}</span>:{" "}
        <span className="text-[#8f92ad]">{value}</span>
        {"\n"}
      </span>,
    );
  }
  return (
    <>
      {lines}
      {lines.length && payload.body !== undefined ? "\n" : null}
      {payload.body !== undefined ? <Json value={payload.body} /> : null}
    </>
  );
}

export default function TrafficPanel({ sim }: { sim: Simulator }) {
  const [onlyMine, setOnlyMine] = useState(false);
  const [tab, setTab] = useState<PayloadTab>("request");

  const [panelHeight, setPanelHeight] = useState(DEFAULT_PANEL);
  const [dragging, setDragging] = useState(false);
  const log = useRef<HTMLDivElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const drag = useRef<{ y: number; height: number } | null>(null);

  const resize = (height: number) => {
    const logHeight = log.current?.offsetHeight ?? 0;
    const current = panel.current?.offsetHeight ?? DEFAULT_PANEL;
    setPanelHeight(
      Math.round(
        Math.max(MIN_PANEL, Math.min(current + logHeight - MIN_LOG, height)),
      ),
    );
  };

  const startDrag = (event: PointerEvent<HTMLDivElement>) => {
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = {
      y: event.clientY,
      height: panel.current?.offsetHeight ?? DEFAULT_PANEL,
    };
    setDragging(true);
  };
  const moveDrag = (event: PointerEvent) => {
    if (drag.current)
      resize(drag.current.height + drag.current.y - event.clientY);
  };
  const endDrag = () => {
    drag.current = null;
    setDragging(false);
  };
  const nudge = (event: KeyboardEvent) => {
    const step = { ArrowUp: 24, ArrowDown: -24 }[event.key];
    if (step) {
      event.preventDefault();
      resize(panelHeight + step);
    }
  };

  const rows = onlyMine ? sim.log.filter((entry) => entry.mine) : sim.log;
  const active = TABS.find(({ id }) => id === tab) ?? TABS[0];
  const payload = sim.payloads[tab];

  return (
    <section
      aria-label="MMX server traffic"
      className="flex min-w-0 flex-col overflow-hidden rounded-[14px] bg-mmx-deep xl:h-[792px]"
    >
      <div className="flex min-h-[46px] flex-none items-center justify-between gap-3 px-4 py-3 text-[13px] font-medium">
        <span>Your infrastructure</span>
        <span className="inline-flex gap-0.5 rounded-lg bg-mmx-raised p-0.5">
          {[
            { label: "All traffic", mine: false },
            { label: "Your orders", mine: true },
          ].map(({ label, mine }) => (
            <button
              key={label}
              type="button"
              aria-pressed={onlyMine === mine}
              onClick={() => setOnlyMine(mine)}
              className={`rounded-md px-[9px] py-[3px] text-xs ${onlyMine === mine ? "bg-mmx-line text-white" : "text-mmx-mute"}`}
            >
              {label}
            </button>
          ))}
        </span>
        <span className="hidden font-code text-xs font-normal text-[#8f92ad] sm:block">
          Simulated in your browser
        </span>
      </div>

      <div
        ref={log}
        role="log"
        aria-label="Traffic log"
        className="flex h-[300px] min-h-0 flex-none flex-col justify-end overflow-hidden px-4 font-code text-[13px] leading-[1.3] xl:h-auto xl:flex-1"
      >
        {rows.map((entry) => (
          <LogRow key={entry.id} entry={entry} />
        ))}
      </div>

      {/* A focusable separator is an interactive widget in ARIA; the lint rules treat all separators as static. */}
      {/* eslint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        role="separator"
        aria-orientation="horizontal"
        aria-label="Resize request panel"
        aria-valuenow={panelHeight}
        // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
        onPointerDown={startDrag}
        onPointerMove={moveDrag}
        onPointerUp={endDrag}
        onDoubleClick={() => resize(DEFAULT_PANEL)}
        onKeyDown={nudge}
        className="group relative z-10 -my-[5px] hidden h-[11px] flex-none cursor-row-resize touch-none outline-none xl:block"
      >
        <span
          className={`absolute inset-x-0 top-[5px] h-px group-hover:bg-mmx-periwinkle group-focus-visible:bg-mmx-periwinkle ${dragging ? "bg-mmx-periwinkle" : "bg-mmx-line"}`}
        />
        <span
          className={`absolute left-1/2 top-[3px] -ml-[18px] h-[5px] w-9 rounded-[3px] group-hover:bg-mmx-periwinkle group-focus-visible:bg-mmx-periwinkle ${dragging ? "bg-mmx-periwinkle" : "bg-mmx-line"}`}
        />
      </div>

      <div
        ref={panel}
        style={{ height: panelHeight }}
        className="flex min-h-0 flex-none flex-col overflow-hidden border-t border-mmx-line bg-[#090a12] xl:border-t-0"
      >
        <div
          role="tablist"
          aria-label="Last request"
          className="grid flex-none grid-cols-4 gap-1 border-b border-[#20223a] px-3 text-[11px] font-medium min-[360px]:text-[13px]"
        >
          {TABS.map(({ id, label }) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={tab === id}
              onClick={() => setTab(id)}
              className={`border-b-2 pb-2 pt-2.5 ${tab === id ? "border-mmx-pink text-white" : "border-transparent text-mmx-mute"}`}
            >
              {label}
            </button>
          ))}
        </div>
        <pre className="min-h-0 flex-1 overflow-auto px-4 py-3 font-code text-[12.5px] leading-normal text-[#dfe1f2]">
          {payload ? (
            <PayloadView payload={payload} />
          ) : (
            <span className="whitespace-normal text-[#8f92ad]">
              {active.empty}
            </span>
          )}
        </pre>
      </div>

      <div className="flex flex-none items-center gap-3.5 border-t border-mmx-line px-4 py-[9px] text-[13px]">
        <button
          type="button"
          role="switch"
          aria-checked={sim.gameOnline}
          onClick={() => sim.setGameOnline(!sim.gameOnline)}
          className="inline-flex items-center gap-2 font-medium"
        >
          <i
            className={`relative h-[18px] w-[30px] rounded-[9px] transition-colors ${sim.gameOnline ? "bg-mmx-mint" : "bg-mmx-line"}`}
          >
            <i
              className={`absolute top-0.5 h-3.5 w-3.5 rounded-full transition-[left] ${sim.gameOnline ? "left-3.5 bg-mmx-ink" : "left-0.5 bg-mmx-mute"}`}
            />
          </i>
          Game server {sim.gameOnline ? "online" : "offline"}
        </button>
        <span
          className={`ml-auto font-code text-xs ${sim.outbox ? "text-mmx-gold" : "text-[#8f92ad]"}`}
        >
          {sim.outbox ? `outbox ${sim.outbox} pending` : "outbox empty"}
        </span>
      </div>
    </section>
  );
}
