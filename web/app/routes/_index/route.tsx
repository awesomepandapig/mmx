import type { LinksFunction, MetaFunction } from "@remix-run/node";
import Api from "~/components/landing/Api";
import Architecture from "~/components/landing/Architecture";
import Deploy from "~/components/landing/Deploy";
import Footer from "~/components/landing/Footer";
import Hero from "~/components/landing/Hero";
import Lifecycle from "~/components/landing/Lifecycle";
import Navbar from "~/components/landing/Navbar";
import Throughput from "~/components/landing/ThroughputCard";

// Loaded here so the trading pages keep their own type.
export const links: LinksFunction = () => [
  {
    rel: "stylesheet",
    href: "https://fonts.googleapis.com/css2?family=Funnel+Display:wght@500;600&family=JetBrains+Mono:wght@400;500&family=Rethink+Sans:wght@400;500;600&display=swap",
  },
];

export const meta: MetaFunction = () => [
  { title: "MMX: don't build an exchange to ship a game" },
  {
    name: "description",
    content: "MMX is the trading engine behind your in-game market.",
  },
];

export default function Home() {
  return (
    <div className="min-h-screen overflow-x-clip bg-mmx-bg font-body text-[17px] leading-normal text-white antialiased">
      <Navbar />
      <main>
        <Hero />
        <Architecture />
        <Api />
        <Lifecycle />
        <Throughput />
        <Deploy />
      </main>
      <Footer />
    </div>
  );
}
