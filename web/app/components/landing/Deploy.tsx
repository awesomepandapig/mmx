import { Lock } from "lucide-react";
import type { ReactNode } from "react";
import BrandLogo from "./BrandLogo";
import { CARD, H2, SECTION } from "./layout";
import { CHART_URL, REPO_URL } from "./links";

const MARK = "size-[72px] text-mmx-periwinkle";

const MARKETS_YAML = `markets:
  - symbol: IRON
    tick_size: "0.01"
    min_size: "1"`;

interface CardProps {
  visual: ReactNode;
  title: string;
  body: string;
  link?: { href: string; label: string };
}

function Card({ visual, title, body, link }: CardProps) {
  return (
    <article className={`flex flex-col ${CARD}`}>
      <div className="mb-6 flex h-[112px] items-center justify-center">
        {visual}
      </div>
      <h3 className="font-display text-[23px] font-medium leading-[1.15] tracking-[-0.01em]">
        {title}
      </h3>
      <p className="mt-1.5 text-mmx-mute">{body}</p>
      {link ? (
        <a
          href={link.href}
          target="_blank"
          rel="noreferrer"
          className="mt-3 font-medium text-mmx-periwinkle hover:text-white"
        >
          {link.label}
        </a>
      ) : null}
    </article>
  );
}

export default function Deploy() {
  return (
    <section id="deploy" className={SECTION}>
      <h2 className={H2}>Deploy it like the rest of your stack.</h2>
      <div className="mt-8 grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-4">
        <Card
          visual={<BrandLogo name="helm" className={MARK} />}
          title="Deploy with Helm"
          body="Install the chart in your cluster."
          link={{
            href: CHART_URL,
            label: "View the chart",
          }}
        />
        <Card
          visual={
            <pre className="w-full overflow-x-auto rounded-xl bg-mmx-deep p-4 font-code text-[13px] leading-[1.45] text-[#dfe1f2]">
              {MARKETS_YAML}
            </pre>
          }
          title="List your items"
          body="Every stackable item goes in one YAML file. MMX is for commodities, not auctions or unique items."
        />
        <Card
          visual={
            <Lock aria-hidden="true" strokeWidth={1.5} className={MARK} />
          }
          title="Your auth"
          body="Your game server vouches for each player, and players never reach MMX."
        />
        <Card
          visual={<BrandLogo name="osi" className={MARK} />}
          title="Open source"
          body="Licensed under MIT/Apache-2.0."
          link={{ href: REPO_URL, label: "View the source" }}
        />
      </div>
    </section>
  );
}
