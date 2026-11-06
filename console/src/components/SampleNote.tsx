import { rate } from "../lib/format";
import type { Link } from "../lib/hooks";
import type { Sample } from "../lib/tape";
import { Word } from "./ui";

/**
 * Says what the list is showing. It speaks about the data, not the browser's connection: an open
 * stream with nothing arriving says so, because that's what an outage looks like from here.
 */
export function SampleNote({ link, sample }: { link: Link; sample: Sample }) {
  const sampled = sample.seen > sample.sent * 1.05 && sample.seen > 0;
  const text = "tabular whitespace-nowrap text-[12px]";
  if (link === "connecting")
    return (
      <Word tone="muted" className={text}>
        Connecting
      </Word>
    );
  if (link === "paused")
    return (
      <Word tone="warn" className={text}>
        Paused
      </Word>
    );
  if (link === "lost")
    return (
      <Word tone="bad" className={text}>
        Disconnected, retrying
      </Word>
    );
  if (sample.seen === 0)
    return (
      <Word tone="muted" className={text}>
        No new events in 5s
      </Word>
    );
  return (
    <Word tone="muted" className={text}>
      {sampled
        ? `Sample: ${rate(sample.sent)} of ${rate(sample.seen)} events/s`
        : `All events, ${rate(sample.seen)}/s`}
    </Word>
  );
}
