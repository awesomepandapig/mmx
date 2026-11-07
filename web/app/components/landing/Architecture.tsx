import { H2, PANEL, SECTION } from "./layout";

const LINE = "#a2a5bb";

const LABEL =
  "Orders go from game clients through game servers to MMX, which runs the API, matching engine, database and outbox. Fills return through a webhook to the game servers, which update the game database.";

interface BoxProps {
  x: number;
  y: number;
  width?: number;
  height?: number;
  title: string;
  subtitle?: string;
  core?: boolean;
}

function Box({
  x,
  y,
  width = 176,
  height = 76,
  title,
  subtitle,
  core = false,
}: BoxProps) {
  const middle = y + height / 2;
  return (
    <g>
      <rect
        x={x}
        y={y}
        width={width}
        height={height}
        rx={12}
        fill={core ? "#2436f0" : "#171926"}
        stroke={core ? "#2436f0" : "#34374f"}
      />
      <text
        x={x + 16}
        y={subtitle ? middle - 3 : middle + 6}
        fill="#fff"
        className="font-body text-[17px] font-semibold"
      >
        {title}
      </text>
      {subtitle ? (
        <text
          x={x + 16}
          y={middle + 18}
          fill={core ? "#d3d7ff" : "#a2a5bb"}
          className="font-body text-sm"
        >
          {subtitle}
        </text>
      ) : null}
    </g>
  );
}

function Marker({ id }: { id: string }) {
  return (
    <defs>
      <marker
        id={id}
        viewBox="0 0 8 8"
        refX={7}
        refY={4}
        markerWidth={7}
        markerHeight={7}
        orient="auto"
      >
        <path d="M0 0L8 4L0 8z" fill={LINE} />
      </marker>
    </defs>
  );
}

function Line({ d, marker }: { d: string; marker: string }) {
  return (
    <path
      d={d}
      fill="none"
      stroke={LINE}
      strokeWidth={2}
      markerEnd={`url(#${marker})`}
    />
  );
}

function WideDiagram() {
  return (
    <svg
      viewBox="0 90 1264 300"
      role="img"
      aria-label={LABEL}
      className="hidden h-auto w-full md:block"
    >
      <Marker id="wide-arrow" />
      <rect
        x={440}
        y={92}
        width={822}
        height={158}
        rx={16}
        fill="rgba(36,54,240,.14)"
        stroke="rgba(143,154,255,.3)"
      />
      <text
        x={460}
        y={126}
        fill="#fff"
        className="font-display text-xl font-semibold"
      >
        MMX
      </text>
      <Box x={0} y={150} title="Game clients" />
      <Box x={218} y={150} title="Game servers" />
      <Box x={460} y={150} title="API" subtitle="REST, WS" core />
      <Box x={662} y={150} title="Matching engine" core />
      <Box x={864} y={150} title="Database" subtitle="Permanent record" core />
      <Box x={1066} y={150} title="Outbox" subtitle="Retries delivery" core />
      <Box x={1066} y={312} title="Fill webhook" />
      <Box x={0} y={312} title="Game database" />
      {[
        "M176 188H216",
        "M394 188H458",
        "M636 188H660",
        "M838 188H862",
        "M1040 188H1064",
        "M1154 226V310",
        "M1066 350H306V228",
        "M262 226V350H178",
      ].map((d) => (
        <Line key={d} d={d} marker="wide-arrow" />
      ))}
    </svg>
  );
}

const STACK: Omit<BoxProps, "x" | "y">[] = [
  { title: "Game clients" },
  { title: "Game servers" },
  { title: "API", subtitle: "REST, WS", core: true },
  { title: "Matching engine", core: true },
  { title: "Database", subtitle: "Permanent record", core: true },
  { title: "Outbox", subtitle: "Retries delivery", core: true },
  { title: "Fill webhook" },
  { title: "Game servers" },
  { title: "Game database" },
];

/** Boxes stack 92px apart, with room for the MMX group's label and edge. */
function tallY(index: number) {
  return index * 92 + (index >= 6 ? 48 : index >= 2 ? 32 : 0);
}

function TallDiagram() {
  return (
    <svg
      viewBox="0 0 360 852"
      role="img"
      aria-label={LABEL}
      className="mx-auto block h-auto w-full max-w-[360px] md:hidden"
    >
      <Marker id="tall-arrow" />
      <rect
        x={16}
        y={172}
        width={328}
        height={404}
        rx={16}
        fill="rgba(36,54,240,.14)"
        stroke="rgba(143,154,255,.3)"
      />
      <text
        x={40}
        y={202}
        fill="#fff"
        className="font-display text-xl font-semibold"
      >
        MMX
      </text>
      {STACK.map((box, i) => (
        <Box key={i} x={40} y={tallY(i)} width={280} height={68} {...box} />
      ))}
      {STACK.slice(1).map((_, i) => (
        <Line
          key={i}
          marker="tall-arrow"
          d={`M180 ${tallY(i) + 68}V${tallY(i + 1) - 2}`}
        />
      ))}
    </svg>
  );
}

export default function Architecture() {
  return (
    <section id="how-it-works" className={SECTION}>
      <h2 className={`max-w-[24em] ${H2}`}>
        How it works.{" "}
        <span className="text-mmx-mute">
          Players trade in your game. MMX sits between your game servers and
          your game database.
        </span>
      </h2>
      <div className={`mt-8 ${PANEL}`}>
        <WideDiagram />
        <TallDiagram />
      </div>
    </section>
  );
}
