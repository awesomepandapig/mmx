import { THROUGHPUT } from "./data";
import { H2, SECTION } from "./layout";

export default function Throughput() {
  return (
    <section id="performance" className={SECTION}>
      <div className="grid items-center gap-8 rounded-3xl bg-mmx-blue p-6 lg:grid-cols-[1fr_1.25fr] lg:gap-12 lg:p-12">
        <div>
          <h2 className={H2}>Built for launch day.</h2>
          <p className="mt-4 max-w-[42ch] text-[19px] text-[#dfe2ff]">
            MMX is built for high availability. Lose a server at launch and your
            market keeps trading.
          </p>
        </div>
        <div className="rounded-2xl border border-white/20 bg-black/15 p-6 text-center lg:py-12">
          <b className="block font-display text-[clamp(2.6rem,7vw,6rem)] font-medium leading-none tracking-[-0.03em]">
            {THROUGHPUT.value}
          </b>
          <span className="mt-4 block text-sm font-medium uppercase tracking-[0.08em] text-[#dfe2ff]">
            {THROUGHPUT.unit}
          </span>
          <span className="mt-1 block text-sm text-[#dfe2ff]">
            {THROUGHPUT.note}
          </span>
        </div>
      </div>
    </section>
  );
}
