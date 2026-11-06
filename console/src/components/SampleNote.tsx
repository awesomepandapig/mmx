import { rate } from "../lib/format";
import type { Link } from "../lib/hooks";
import type { Sample } from "../lib/tape";
import { Status } from "./ui";
import type { State } from "./ui";

const words: Record<Link, { state: State; text: string }> = {
  connecting: { state: "idle", text: "Connecting" },
  live: { state: "good", text: "Live" },
  lost: { state: "bad", text: "Reconnecting" },
  paused: { state: "warn", text: "Paused" },
};

/** Says what the list is showing, so a sample never reads as everything. */
export function SampleNote({ link, sample }: { link: Link; sample: Sample }) {
  const sampled = sample.seen > sample.sent * 1.05 && sample.seen > 0;
  return (
    <span className="tabular inline-flex flex-wrap items-center gap-x-3 gap-y-1 text-[13px] text-muted">
      <Status state={words[link].state}>{words[link].text}</Status>
      {link === "live" &&
        (sampled ? (
          <span>
            Sample: {rate(sample.sent)} of {rate(sample.seen)} reports/s (
            {Math.max(0.1, (sample.sent / sample.seen) * 100).toFixed(1)}%)
          </span>
        ) : (
          <span>All reports, {rate(sample.seen)}/s</span>
        ))}
    </span>
  );
}
