import AgentField from "./AgentField";
import BrandLogo from "./BrandLogo";
import OrderDemo from "./OrderDemo";
import { ENGINES } from "./data";
import { CARD, SECTION } from "./layout";
import { DEMO_URL } from "./links";

export default function Hero() {
  return (
    <div className={SECTION}>
      <header>
        <h1 className="font-display text-[clamp(2.9rem,4.9vw,4.2rem)] font-medium leading-[1.04] tracking-[-0.025em]">
          Don&apos;t build an exchange to ship a game.
        </h1>
        <p className="mt-4 text-xl text-mmx-mute">
          MMX is the trading engine behind your in-game market.
        </p>
        <div className="mt-8 flex flex-wrap items-center gap-2">
          <AgentField />
          <a
            href={DEMO_URL}
            className="inline-flex h-[56px] items-center rounded-xl border border-mmx-line px-6 font-medium transition-colors hover:border-mmx-periwinkle"
          >
            Demo
          </a>
        </div>
      </header>
      <div className="mt-8">
        <OrderDemo />
      </div>
      <div
        className={`${CARD} mt-8 flex flex-wrap items-center justify-center gap-x-12 gap-y-4 lg:justify-between`}
      >
        <p className="w-full text-center font-medium lg:w-auto">
          Works with your game server
        </p>
        {ENGINES.map(({ name, logo }) => (
          <span
            key={name}
            className="inline-flex items-center gap-2 font-medium"
          >
            <BrandLogo name={logo} className="size-8" />
            {name}
          </span>
        ))}
      </div>
    </div>
  );
}
