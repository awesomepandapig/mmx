import { useEffect, useState, useSyncExternalStore } from "react";
import GamePanel from "./GamePanel";
import TrafficPanel from "./TrafficPanel";
import { Simulator } from "./simulator";

export default function Demo() {
  const [sim] = useState(() => new Simulator());
  useSyncExternalStore(
    sim.subscribe,
    () => sim.version,
    () => 0,
  );

  useEffect(
    () =>
      sim.start({
        reducedMotion: matchMedia("(prefers-reduced-motion: reduce)").matches,
      }),
    [sim],
  );

  return (
    <div className="mb-12 grid gap-3.5 rounded-3xl bg-mmx-blue p-2.5 xl:grid-cols-2 xl:p-[18px]">
      <TrafficPanel sim={sim} />
      <GamePanel sim={sim} />
    </div>
  );
}
