import { NavLink } from "react-router-dom";
import { lastMarket } from "./MarketPicker";
import { cn } from "./ui";

/** Market view first. The list and the file are for when one market isn't the question. */
export function MarketsNav() {
  const last = lastMarket();
  const tabs = [
    { to: last ? `/markets/${last}` : "/markets", label: "Market view" },
    { to: "/all-markets", label: "All markets" },
    { to: "/markets-file", label: "Markets file" },
  ];
  return (
    <div className="flex h-7 flex-none items-stretch gap-1 border-b border-line bg-surface px-1.5">
      {tabs.map((tab) => (
        <NavLink
          key={tab.label}
          to={tab.to}
          className={({ isActive }) =>
            cn(
              "flex items-center border-b-2 px-1.5 text-[14px] font-medium",
              isActive ? "border-accent text-ink" : "border-transparent text-muted hover:text-ink",
            )
          }
        >
          {tab.label}
        </NavLink>
      ))}
    </div>
  );
}
