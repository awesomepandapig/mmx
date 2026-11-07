import { ChevronDown, Terminal } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "~/components/ui/dropdown-menu";
import BrandLogo from "./BrandLogo";
import { AGENTS, SETUP_PROMPT, type Agent } from "./data";

/** The clipboard API only exists on secure origins, so plain http falls back to a hidden field. */
async function writeClipboard(text: string) {
  if (navigator.clipboard) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const field = document.createElement("textarea");
  field.value = text;
  field.setAttribute("readonly", "");
  field.style.position = "fixed";
  field.style.opacity = "0";
  document.body.append(field);
  field.select();
  const copied = document.execCommand("copy");
  field.remove();
  if (!copied) throw new Error("Copy was refused");
}

function AgentLogo({ agent }: { agent: Agent }) {
  if (!agent.logo)
    return <Terminal aria-hidden="true" className="size-[18px] shrink-0" />;
  return <BrandLogo name={agent.logo} className="size-[18px] shrink-0" />;
}

export default function AgentField() {
  const [agent, setAgent] = useState(AGENTS[0]);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(timer);
  }, [copied]);

  const text = agent.copyText ?? SETUP_PROMPT;

  async function copy(event: FormEvent) {
    event.preventDefault();
    try {
      await writeClipboard(text);
      setCopied(true);
    } catch {
      // The command stays visible in the field, so there's nothing more to show.
    }
  }

  return (
    <form
      onSubmit={copy}
      className="flex w-full max-w-[820px] flex-wrap items-center gap-1 rounded-xl border border-mmx-line bg-mmx-surface p-[5px] md:flex-nowrap"
    >
      <code className="order-first w-full min-w-0 truncate px-2.5 py-2 font-code text-sm text-mmx-mute md:order-none md:w-auto md:flex-1 md:py-0">
        {agent.command}
      </code>
      <DropdownMenu>
        <DropdownMenuTrigger
          aria-label={`Install with ${agent.name}`}
          className="flex h-11 shrink-0 items-center gap-2 rounded-lg border border-mmx-line bg-mmx-raised pl-3 pr-2.5 text-[15px] font-medium md:order-first"
        >
          <AgentLogo agent={agent} />
          {agent.name}
          <ChevronDown aria-hidden="true" className="size-4 text-mmx-mute" />
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="start"
          className="min-w-[200px] rounded-xl border-mmx-line bg-mmx-raised p-1 font-body text-white"
        >
          {AGENTS.map((option) => (
            <DropdownMenuItem
              key={option.name}
              onSelect={() => setAgent(option)}
              className="cursor-pointer gap-2.5 rounded-lg px-2.5 py-2 text-[15px] font-medium focus:bg-mmx-line focus:text-white"
            >
              <AgentLogo agent={option} />
              {option.name}
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
      <button
        type="submit"
        className="h-11 min-w-[126px] flex-1 whitespace-nowrap rounded-lg bg-mmx-pink px-5 font-semibold text-mmx-ink transition-colors hover:bg-[#ff7ac8] md:flex-none"
      >
        {copied ? "Copied" : agent.copyText ? "Copy" : "Copy prompt"}
      </button>
    </form>
  );
}
