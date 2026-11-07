import { useState } from "react";
import { SNIPPETS, type SnippetKey } from "./data";
import { tokenize, type TokenKind } from "./highlight";
import { H2, LEAD, SECTION } from "./layout";

// The syntax palette from the original API card.
const COLORS: Record<TokenKind, string> = {
  plain: "#6f8399",
  comment: "#6f8399",
  string: "#47ebb4",
  number: "#0fbf85",
  keyword: "#c586c0",
  function: "#47ebb4",
  property: "#7eb6f6",
  flag: "#47ebb4",
  command: "#7f9ab8",
};

const TABS = Object.keys(SNIPPETS) as SnippetKey[];

export default function Api() {
  const [active, setActive] = useState<SnippetKey>("unity");

  return (
    <section
      id="api"
      className={`${SECTION} grid items-center gap-8 lg:grid-cols-[1fr_1.35fr] lg:gap-12`}
    >
      <div>
        <h2 className={H2}>Call it from your game server.</h2>
        <p className={LEAD}>
          Send orders over REST or WebSocket, and apply fills on one webhook.
        </p>
      </div>

      <div className="min-w-0 overflow-hidden rounded-2xl border border-mmx-line bg-mmx-surface">
        <div
          role="tablist"
          aria-label="Code sample language"
          className="flex overflow-x-auto border-b border-[#20223a]"
        >
          {TABS.map((key) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={active === key}
              onClick={() => setActive(key)}
              className={`whitespace-nowrap border-r border-r-[#20223a] px-2.5 py-3 text-xs sm:px-6 font-medium transition-colors ${
                active === key
                  ? "text-[#47ebb4]"
                  : "text-gray-400 hover:text-gray-300"
              }`}
            >
              {SNIPPETS[key].label}
            </button>
          ))}
        </div>
        <div className="grid">
          {TABS.map((key) => (
            <pre
              key={key}
              role="tabpanel"
              aria-hidden={active !== key}
              tabIndex={active === key ? 0 : undefined}
              className={`col-start-1 row-start-1 min-w-0 overflow-x-auto p-6 font-code text-[13px] leading-relaxed md:text-[14.5px] ${active === key ? "" : "invisible"}`}
            >
              <code>
                {tokenize(SNIPPETS[key].code).map(({ kind, text }, index) => (
                  <span
                    key={index}
                    style={{
                      color: COLORS[kind],
                      fontStyle: kind === "comment" ? "italic" : undefined,
                    }}
                  >
                    {text}
                  </span>
                ))}
              </code>
            </pre>
          ))}
        </div>
      </div>
    </section>
  );
}
