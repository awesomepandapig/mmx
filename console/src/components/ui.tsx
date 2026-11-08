import * as Dialog from "@radix-ui/react-dialog";
import * as RadixSwitch from "@radix-ui/react-switch";
import clsx from "clsx";
import { forwardRef, useEffect, useId, useRef, useState } from "react";
import type {
  ButtonHTMLAttributes,
  InputHTMLAttributes,
  ReactNode,
  SelectHTMLAttributes,
  TextareaHTMLAttributes,
} from "react";
import { twMerge } from "tailwind-merge";
import type { Tone as TextTone } from "../lib/exchange";
import { copyText } from "../lib/clipboard";

export function cn(...parts: Parameters<typeof clsx>) {
  return twMerge(clsx(...parts));
}

/**
 * How much an action weighs. `primary` is the one thing a view is for, `plain` is ordinary, and
 * `quiet` stays out of the way. `danger` is solid red and stops trading or destroys something, so a
 * view shows at most one. `caution` is the same warning at the size of a row.
 */
type Tone = "primary" | "plain" | "quiet" | "danger" | "caution";

const tones: Record<Tone, string> = {
  primary: "bg-solid text-on-solid hover:bg-solid-hover",
  plain: "border border-line text-ink hover:border-faint",
  quiet: "text-muted hover:bg-raised hover:text-ink",
  danger: "bg-danger font-semibold text-white hover:bg-danger-hover",
  caution: "border border-bad/60 text-bad hover:border-bad hover:bg-bad-wash",
};

export const buttonClass = (tone: Tone = "plain") =>
  cn(
    "inline-flex h-7 shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-[3px] px-2.5 text-[15px] font-medium transition-colors disabled:pointer-events-none disabled:opacity-40",
    tones[tone],
  );

export const Button = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & { tone?: Tone }
>(function Button({ tone = "plain", className, ...props }, ref) {
  return <button ref={ref} type="button" {...props} className={cn(buttonClass(tone), className)} />;
});

/** A panel with a border, for forms. Dashboards use `Panel` inside a `Tiles`. */
export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return <section className={cn("border border-line bg-surface", className)}>{children}</section>;
}

/** Panels that share their edges, with 1px rules between them and no gaps. */
export function Tiles({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={cn("grid gap-px bg-line", className)}>{children}</div>;
}

export function Panel({
  title,
  aside,
  className,
  bodyClassName,
  children,
}: {
  title: ReactNode;
  aside?: ReactNode;
  className?: string;
  bodyClassName?: string;
  children: ReactNode;
}) {
  return (
    <section className={cn("flex min-h-0 min-w-0 flex-col bg-surface", className)}>
      <header className="flex h-8 flex-none items-center justify-between gap-3 border-b border-hairline px-3">
        <h2 className="truncate text-[14px] font-semibold">{title}</h2>
        {aside && <div className="flex items-center gap-2 text-[14px] text-faint">{aside}</div>}
      </header>
      <div className={cn("min-h-0 flex-1", bodyClassName)}>{children}</div>
    </section>
  );
}

export function Page({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={cn("w-full p-3", className)}>{children}</div>;
}

export function PageHeader({
  title,
  detail,
  actions,
}: {
  title: ReactNode;
  detail?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="mb-3 flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 items-baseline gap-3">
        <h1 className="text-[17px] font-semibold">{title}</h1>
        {detail && <div className="text-[15px] text-muted">{detail}</div>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </header>
  );
}

export function SectionTitle({
  children,
  actions,
  className,
}: {
  children: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex items-center justify-between gap-3", className)}>
      <h2 className="text-[15px] font-semibold">{children}</h2>
      {actions}
    </div>
  );
}

const wordTone: Record<TextTone, string> = {
  plain: "text-ink",
  muted: "text-muted",
  warn: "font-medium text-warn",
  bad: "font-semibold text-bad",
};

/** A state, in words. Normal reads as plain text, and only trouble gets colour. */
export function Word({
  tone = "plain",
  className,
  children,
}: {
  tone?: TextTone;
  className?: string;
  children: ReactNode;
}) {
  return <span className={cn(wordTone[tone], className)}>{children}</span>;
}

/** One figure in a strip: a label over a number, with a line of context under it. */
export function Metric({
  label,
  value,
  note,
  tone = "plain",
  className,
}: {
  label: string;
  value: ReactNode;
  note?: ReactNode;
  tone?: TextTone;
  className?: string;
}) {
  return (
    <div className={cn("min-w-0 bg-surface px-3 py-2", className)}>
      <p className="label truncate">{label}</p>
      <div className={cn("tabular truncate text-[20px] font-medium leading-7", wordTone[tone])}>
        {value ?? <Skeleton className="my-1 h-5 w-16" />}
      </div>
      <p className="tabular truncate text-[14px] leading-4 text-muted">{note ?? " "}</p>
    </div>
  );
}

export type State = "good" | "warn" | "bad" | "idle";

const noticeTone: Record<State, string> = {
  good: "border-line bg-surface",
  warn: "border-warn/50 bg-warn-wash",
  bad: "border-bad/60 bg-bad-wash",
  idle: "border-line bg-surface",
};

const noticeTitle: Record<State, string> = {
  good: "text-ink",
  warn: "text-warn",
  bad: "text-bad",
  idle: "text-ink",
};

export function Notice({
  state = "idle",
  title,
  children,
  action,
  className,
}: {
  state?: State;
  title?: string;
  children?: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  return (
    <div
      role={state === "bad" ? "alert" : "status"}
      className={cn(
        "flex flex-wrap items-center justify-between gap-3 border px-3 py-1.5",
        noticeTone[state],
        className,
      )}
    >
      <div className="min-w-0 break-words">
        {title && <p className={cn("font-semibold", noticeTitle[state])}>{title}</p>}
        {children && <div className={title ? "text-muted" : undefined}>{children}</div>}
      </div>
      {action}
    </div>
  );
}

export const inputClass =
  "h-7 w-full rounded-[3px] border border-line bg-canvas px-2 text-[15px] text-ink placeholder:text-faint focus:border-accent focus:outline-none disabled:opacity-60";

export function Field({
  label,
  hint,
  className,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: ReactNode }) {
  const id = useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="label mb-0.5 block">
        {label}
      </label>
      <input id={id} {...props} className={inputClass} />
      {hint && <p className="mt-0.5 text-[14px] text-faint">{hint}</p>}
    </div>
  );
}

export function TextArea({
  label,
  hint,
  className,
  ...props
}: TextareaHTMLAttributes<HTMLTextAreaElement> & { label: string; hint?: ReactNode }) {
  const id = useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="label mb-0.5 block">
        {label}
      </label>
      <textarea
        id={id}
        {...props}
        className={cn(inputClass, "h-auto py-1 font-code text-[14px] leading-5")}
      />
      {hint && <p className="mt-0.5 text-[14px] text-faint">{hint}</p>}
    </div>
  );
}

export function Select({
  label,
  className,
  children,
  ...props
}: SelectHTMLAttributes<HTMLSelectElement> & { label: string }) {
  return (
    <select
      aria-label={label}
      {...props}
      className={cn(inputClass, "select w-auto appearance-none pr-7", className)}
    >
      {children}
    </select>
  );
}

export function Switch({
  label,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <RadixSwitch.Root
      aria-label={label}
      checked={checked}
      disabled={disabled}
      onCheckedChange={onChange}
      className="relative h-[18px] w-[30px] shrink-0 rounded-full bg-line transition-colors disabled:opacity-50 data-[state=checked]:bg-accent"
    >
      <RadixSwitch.Thumb className="block h-3.5 w-3.5 translate-x-0.5 rounded-full bg-muted transition-transform data-[state=checked]:translate-x-[14px] data-[state=checked]:bg-on-solid" />
    </RadixSwitch.Root>
  );
}

export function Code({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <code
      className={cn(
        "break-all rounded-[3px] bg-raised px-1 py-0.5 font-code text-[14px]",
        className,
      )}
    >
      {children}
    </code>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const [failed, setFailed] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => () => clearTimeout(timer.current), []);
  return (
    <span className="inline-flex flex-wrap items-center gap-2">
      <Button
        onClick={() => {
          setCopied(false);
          setFailed(false);
          clearTimeout(timer.current);
          void copyText(text).then(
            () => {
              setCopied(true);
              timer.current = setTimeout(() => setCopied(false), 1500);
            },
            () => setFailed(true),
          );
        }}
      >
        {copied ? "Copied" : label}
      </Button>
      <span role="status" className="text-[14px] text-bad">
        {failed ? "Copy failed. Select the text and copy it manually." : copied ? "Copied." : ""}
      </span>
    </span>
  );
}

export function Modal({
  open,
  title,
  description,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  description?: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={(next) => !next && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/60" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed left-1/2 top-1/2 z-50 max-h-[90dvh] w-[calc(100vw-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 overflow-y-auto border border-line bg-surface p-4 text-ink focus:outline-none"
        >
          <Dialog.Title className="text-[17px] font-semibold">{title}</Dialog.Title>
          {description && (
            <Dialog.Description asChild>
              <div className="mt-1 text-muted">{description}</div>
            </Dialog.Description>
          )}
          <div className="mt-3">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** One of a few choices, always on show. */
export function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
  className,
}: {
  label: string;
  value: T;
  options: { value: T; label: ReactNode }[];
  onChange: (value: T) => void;
  className?: string;
}) {
  return (
    <div
      role="group"
      aria-label={label}
      className={cn("inline-flex rounded-[3px] border border-line", className)}
    >
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
          className={cn(
            "tabular h-[26px] whitespace-nowrap border-l border-line px-2 text-[14px] font-medium first:border-l-0",
            value === option.value ? "bg-raised text-ink" : "text-muted hover:text-ink",
          )}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden className={cn("skeleton rounded-[2px] bg-raised", className)} />;
}

/** Rows of a table that hasn't answered yet, so the page doesn't jump when it does. */
export function SkeletonRows({ rows = 6 }: { rows?: number }) {
  return (
    <div role="status" aria-label="Loading" className="divide-y divide-hairline">
      {Array.from({ length: rows }, (_, index) => (
        <div key={index} className="flex h-7 items-center gap-6 px-3">
          <Skeleton className="h-3 w-20" />
          <Skeleton className="h-3 w-16" />
          <Skeleton className="h-3 flex-1" />
        </div>
      ))}
    </div>
  );
}

export function EmptyState({
  title,
  children,
  action,
}: {
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center px-4 py-8 text-center">
      <p className="font-medium text-muted">{title}</p>
      {children && <p className="mt-0.5 max-w-md text-[14px] text-faint">{children}</p>}
      {action && <div className="mt-3">{action}</div>}
    </div>
  );
}

export function ErrorState({
  title,
  detail,
  onRetry,
}: {
  title: string;
  detail?: string | null;
  onRetry?: () => void;
}) {
  return (
    <div role="alert" className="flex flex-col items-center px-4 py-8 text-center">
      <p className="font-semibold text-bad">{title}</p>
      {detail && <p className="mt-0.5 max-w-xl break-words text-[14px] text-muted">{detail}</p>}
      {onRetry && (
        <Button className="mt-3" onClick={onRetry}>
          Try again
        </Button>
      )}
    </div>
  );
}

export function Pager({
  offset,
  limit,
  total,
  onOffset,
  onLimit,
}: {
  offset: number;
  limit: number;
  total: number;
  onOffset: (offset: number) => void;
  onLimit: (limit: number) => void;
}) {
  const last = Math.min(offset + limit, total);
  return (
    <div className="flex items-center gap-2 text-[14px] text-muted">
      <span className="tabular whitespace-nowrap">
        {total === 0 ? "0" : `${(offset + 1).toLocaleString()}-${last.toLocaleString()}`} of{" "}
        {total.toLocaleString()}
      </span>
      <Select
        label="Rows per page"
        className="h-[26px] text-[14px]"
        value={limit}
        onChange={(event) => onLimit(Number(event.target.value))}
      >
        {[25, 50, 100, 200].map((size) => (
          <option key={size} value={size}>
            {size} rows
          </option>
        ))}
      </Select>
      <Button
        className="h-[26px] px-2"
        aria-label="Previous page"
        disabled={offset === 0}
        onClick={() => onOffset(Math.max(0, offset - limit))}
      >
        ‹
      </Button>
      <Button
        className="h-[26px] px-2"
        aria-label="Next page"
        disabled={last >= total}
        onClick={() => onOffset(offset + limit)}
      >
        ›
      </Button>
    </div>
  );
}

export function SearchIcon({ className }: { className?: string }) {
  return (
    <svg aria-hidden viewBox="0 0 16 16" fill="none" className={cn("h-3.5 w-3.5", className)}>
      <circle cx="7" cy="7" r="4.5" stroke="currentColor" strokeWidth="1.5" />
      <path d="m10.5 10.5 3 3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  );
}

export function SearchField({
  label,
  className,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & { label: string }) {
  return (
    <div className={cn("relative", className)}>
      <SearchIcon className="pointer-events-none absolute left-2 top-[7px] text-faint" />
      <input aria-label={label} {...props} className={cn(inputClass, "pl-7")} />
    </div>
  );
}

/** Facts in two columns. */
export function Facts({ items, className }: { items: [string, ReactNode][]; className?: string }) {
  return (
    <dl className={cn("grid grid-cols-[auto_1fr] gap-x-4 gap-y-1", className)}>
      {items.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-faint">{label}</dt>
          <dd className="tabular min-w-0 break-words text-right">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
