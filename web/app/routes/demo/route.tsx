import type { LinksFunction, MetaFunction } from "@remix-run/node";
import Demo from "~/components/demo/Demo";
import Footer from "~/components/landing/Footer";
import Navbar from "~/components/landing/Navbar";
import { pageMeta } from "~/lib/seo";

export const links: LinksFunction = () => [
  {
    rel: "stylesheet",
    href: "https://fonts.googleapis.com/css2?family=Funnel+Display:wght@500;600&family=JetBrains+Mono:wght@400;500&family=Rethink+Sans:wght@400;500;600&display=swap",
  },
];

export const meta: MetaFunction = () =>
  pageMeta(
    "MMX - Trading Demo",
    "Explore a simulated game market: place orders and follow matching, market data, and fill webhooks in the browser.",
    "/demo",
  );

export default function DemoRoute() {
  return (
    <div className="min-h-screen overflow-x-clip bg-mmx-bg font-body text-[17px] leading-normal text-white antialiased">
      <Navbar />
      <main className="mx-auto max-w-[1312px] px-6">
        <header className="pb-[26px] pt-[34px]">
          <h1 className="font-display text-[clamp(2.2rem,3.6vw,3.2rem)] font-medium leading-[1.04] tracking-[-0.025em]">
            Place an order. Watch your server run it.
          </h1>
          <p className="mt-2.5 text-xl text-mmx-mute">
            The game on the right is a sample client. The log on the left is
            MMX.
          </p>
        </header>
        <Demo />
      </main>
      <Footer showCta={false} />
    </div>
  );
}
