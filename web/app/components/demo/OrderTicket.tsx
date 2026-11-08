import { type FormEvent, useState } from "react";
import {
  type OrderType,
  type Side,
  type Simulator,
  money,
  whole,
} from "./simulator";

const QUANTITIES = [10, 50, 100];

interface SegmentProps<T extends string> {
  label: string;
  options: { value: T; label: string }[];
  value: T;
  onChange: (value: T) => void;
}

function Segment<T extends string>({
  label,
  options,
  value,
  onChange,
}: SegmentProps<T>) {
  return (
    <div
      role="group"
      aria-label={label}
      className="grid h-9 grid-cols-2 rounded-lg bg-[#f1f1f5] p-0.5 text-[13px] font-medium"
    >
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
          className={`rounded-md ${value === option.value ? "bg-white text-mmx-ink shadow-[0_1px_2px_rgba(8,9,15,0.1)]" : "text-[#6c6e7c] hover:text-mmx-ink"}`}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

// Every control in the ticket is 36px tall with an 8px radius.
const quickButton =
  "h-9 min-w-0 whitespace-nowrap rounded-lg bg-[#f1f1f5] text-[13px] font-medium tabular-nums text-mmx-ink hover:bg-[#ececf1] disabled:pointer-events-none disabled:text-[#6c6e7c] disabled:opacity-50";
const field =
  "flex h-9 min-w-0 items-center gap-2 rounded-lg border border-[#ececf1] px-3 text-[13px] text-[#6c6e7c] focus-within:border-mmx-ink";
const input =
  "w-full min-w-0 flex-1 bg-transparent text-right text-[13px] font-medium tabular-nums text-mmx-ink outline-none placeholder:font-normal placeholder:text-[#6c6e7c] disabled:text-[#6c6e7c]";

interface OrderTicketProps {
  sim: Simulator;
  type: OrderType;
  onTypeChange: (type: OrderType) => void;
  /** What the player typed or picked. While null, the price follows the market. */
  priceDraft: string | null;
  onPriceChange: (price: string | null) => void;
}

export default function OrderTicket({
  sim,
  type,
  onTypeChange,
  priceDraft,
  onPriceChange,
}: OrderTicketProps) {
  const [side, setSide] = useState<Side>("buy");
  const [quantity, setQuantity] = useState("100");

  const market = sim.market();
  const name = market.name.toLowerCase();
  const item = sim.items[market.sym];
  const buying = side === "buy";
  const limit = type === "limit";
  const bid = market.bids[0].price;
  const ask = market.asks[0].price;

  const shownPrice = limit ? (priceDraft ?? money(buying ? ask : bid)) : "";
  const qty = Number.parseInt(quantity, 10) || 0;
  const price = Math.round(Number.parseFloat(shownPrice) * 100) || 0;
  const total = limit
    ? qty * price
    : sim.quote(market.sym, side, qty, null).cost;
  const totalLabel = buying
    ? "Gold held in escrow"
    : limit
      ? "You receive"
      : "Estimated gold";

  const pickPrice = (cents: number) =>
    onPriceChange(money(Math.round(cents / market.tick) * market.tick));
  const percent = (amount: number) => {
    const direction = buying ? -1 : 1;
    pickPrice((price || market.last) * (1 + (direction * amount) / 100));
  };
  const arrow = buying ? "↓" : "↑";
  const priceButtons = [
    { label: "Mid", onClick: () => pickPrice((bid + ask) / 2) },
    {
      label: buying ? "Bid" : "Ask",
      onClick: () => pickPrice(buying ? bid : ask),
    },
    { label: `1%${arrow}`, onClick: () => percent(1) },
    { label: `5%${arrow}`, onClick: () => percent(5) },
  ];

  const submit = (event: FormEvent) => {
    event.preventDefault();
    sim.placeOrder({
      sym: market.sym,
      side,
      type,
      qty,
      price: limit ? price : null,
    });
  };

  return (
    <form onSubmit={submit} className="grid flex-none grid-cols-2 gap-2">
      <Segment
        label="Side"
        value={side}
        onChange={(value) => {
          setSide(value);
          onPriceChange(null);
        }}
        options={[
          { value: "buy", label: "Buy" },
          { value: "sell", label: "Sell" },
        ]}
      />
      <Segment
        label="Order type"
        value={type}
        onChange={onTypeChange}
        options={[
          { value: "limit", label: "Limit" },
          { value: "market", label: "Market" },
        ]}
      />
      <label className={field}>
        Quantity
        <input
          value={quantity}
          onChange={(event) =>
            setQuantity(event.target.value.replace(/\D/g, ""))
          }
          inputMode="numeric"
          className={input}
        />
      </label>
      <label className={field}>
        Price
        <input
          value={shownPrice}
          onChange={(event) => {
            if (/^\d*\.?\d{0,2}$/.test(event.target.value))
              onPriceChange(event.target.value);
          }}
          disabled={!limit}
          placeholder={limit ? "" : "Best available"}
          inputMode="decimal"
          className={input}
        />
      </label>
      <div className="grid grid-cols-3 gap-1">
        {QUANTITIES.map((value) => (
          <button
            key={value}
            type="button"
            onClick={() => setQuantity(String(value))}
            className={quickButton}
          >
            {value}
          </button>
        ))}
      </div>
      <div className="grid grid-cols-4 gap-1">
        {priceButtons.map(({ label, onClick }) => (
          <button
            key={label}
            type="button"
            onClick={onClick}
            disabled={!limit}
            className={quickButton}
          >
            {label}
          </button>
        ))}
      </div>
      <dl className="col-span-2 grid gap-1 py-1 text-[13px] leading-5 tabular-nums text-[#6c6e7c]">
        <div className="flex flex-wrap justify-between gap-x-4">
          <dt>Available</dt>
          <dd className="font-medium text-mmx-ink">
            {whole(sim.gold)} gold
            {sim.heldGold ? (
              <span className="font-normal text-[#6c6e7c]">
                {" "}
                ({whole(sim.heldGold)} held)
              </span>
            ) : null}
            {` · ${item.free} ${name}`}
            {item.held ? (
              <span className="font-normal text-[#6c6e7c]">
                {" "}
                ({item.held} held)
              </span>
            ) : null}
          </dd>
        </div>
        <div className="flex justify-between gap-4">
          <dt>{totalLabel}</dt>
          <dd className="font-medium text-mmx-ink">{money(total)} gold</dd>
        </div>
      </dl>
      <button
        type="submit"
        className="col-span-2 h-9 rounded-lg bg-mmx-pink text-[13px] font-semibold text-mmx-ink transition-colors hover:bg-[#ff7ac8]"
      >
        {buying ? "Buy" : "Sell"} {qty || ""} {name}
      </button>
    </form>
  );
}
