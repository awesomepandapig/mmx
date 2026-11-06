import * as Dialog from "@radix-ui/react-dialog";
import * as RadixSwitch from "@radix-ui/react-switch";
import clsx from "clsx";
import { useId, useState } from "react";
import type {
  ButtonHTMLAttributes,
  InputHTMLAttributes,
  ReactNode,
  SelectHTMLAttributes,
  TextareaHTMLAttributes,
} from "react";
import { twMerge } from "tailwind-merge";

export function cn(...parts: Parameters<typeof clsx>) {
  return twMerge(clsx(...parts));
}

type Tone = "primary" | "plain" | "danger" | "quiet";

const tones: Record<Tone, string> = {
  primary: "bg-solid text-on-solid hover:bg-solid-hover hover:text-on-solid",
  plain: "border border-line text-ink hover:border-accent",
  danger: "border border-bad/50 text-bad hover:border-bad hover:bg-bad/10",
  quiet: "text-muted hover:bg-raised hover:text-ink",
};

export const buttonClass = (tone: Tone = "plain") =>
  cn(
    "inline-flex h-9 shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-lg px-3.5 text-sm font-medium transition-colors disabled:pointer-events-none disabled:opacity-50",
    tones[tone],
  );

export function Button({
  tone = "plain",
  className,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { tone?: Tone }) {
  return <button type="button" {...props} className={cn(buttonClass(tone), className)} />;
}

export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <section className={cn("rounded-2xl border border-line bg-surface", className)}>
      {children}
    </section>
  );
}

export function Page({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <div className={cn("mx-auto w-full max-w-[1312px] p-4 lg:p-6", className)}>{children}</div>
  );
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
    <header className="mb-5 flex flex-wrap items-center justify-between gap-3">
      <div className="min-w-0">
        <h1 className="font-display text-[28px] font-medium leading-tight tracking-heading">
          {title}
        </h1>
        {detail && <p className="mt-0.5 text-sm text-muted">{detail}</p>}
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
      <h2 className="font-display text-lg font-medium tracking-heading">{children}</h2>
      {actions}
    </div>
  );
}

export type State = "good" | "warn" | "bad" | "idle";

const stateText: Record<State, string> = {
  good: "text-good",
  warn: "text-warn",
  bad: "text-bad",
  idle: "text-faint",
};

/** A status in words, with a dot. The word carries the meaning, so colour is never the only cue. */
export function Status({
  state,
  children,
  className,
}: {
  state: State;
  children: ReactNode;
  className?: string;
}) {
  return (
    <span
      className={cn("inline-flex items-center gap-1.5 font-medium", stateText[state], className)}
    >
      <span aria-hidden className="h-1.5 w-1.5 shrink-0 rounded-full bg-current" />
      <span className="text-[13px]">{children}</span>
    </span>
  );
}

export function Tag({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span
      className={cn(
        "inline-flex h-5 items-center rounded-md bg-raised px-1.5 text-xs font-medium text-muted",
        className,
      )}
    >
      {children}
    </span>
  );
}

const noticeTone: Record<State, string> = {
  good: "border-good/30",
  warn: "border-warn/40",
  bad: "border-bad/50",
  idle: "border-line",
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
        "flex flex-wrap items-center justify-between gap-3 rounded-xl border bg-surface px-4 py-2.5 text-sm",
        noticeTone[state],
        className,
      )}
    >
      <div className="min-w-0 break-words">
        {title && <p className={cn("font-medium", stateText[state])}>{title}</p>}
        {children && <div className={title ? "text-muted" : undefined}>{children}</div>}
      </div>
      {action}
    </div>
  );
}

export const inputClass =
  "h-9 w-full rounded-lg border border-line bg-canvas px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none disabled:opacity-60";

export function Field({
  label,
  hint,
  className,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: ReactNode }) {
  const id = useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="mb-1 block text-[13px] font-medium">
        {label}
      </label>
      <input id={id} {...props} className={inputClass} />
      {hint && <p className="mt-1 text-xs text-faint">{hint}</p>}
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
      <label htmlFor={id} className="mb-1 block text-[13px] font-medium">
        {label}
      </label>
      <textarea
        id={id}
        {...props}
        className={cn(inputClass, "h-auto py-2 font-code text-[13px] leading-5")}
      />
      {hint && <p className="mt-1 text-xs text-faint">{hint}</p>}
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
      className={cn(inputClass, "w-auto pr-7 font-medium", className)}
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
      className="relative h-[18px] w-[30px] shrink-0 rounded-full bg-line transition-colors data-[state=checked]:bg-good disabled:opacity-50"
    >
      <RadixSwitch.Thumb className="block h-3.5 w-3.5 translate-x-0.5 rounded-full bg-muted transition-transform data-[state=checked]:translate-x-[14px] data-[state=checked]:bg-on-solid" />
    </RadixSwitch.Root>
  );
}

export function Code({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <code
      className={cn(
        "break-all rounded-md bg-raised px-1.5 py-0.5 font-code text-[0.85em]",
        className,
      )}
    >
      {children}
    </code>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      className="h-8 px-2.5 text-[13px]"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        });
      }}
    >
      {copied ? "Copied" : label}
    </Button>
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
          className="fixed left-1/2 top-1/2 z-50 max-h-[90dvh] w-[calc(100vw-2rem)] max-w-lg -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-2xl border border-line bg-surface p-6 text-ink focus:outline-none"
        >
          <Dialog.Title className="font-display text-[22px] font-medium tracking-heading">
            {title}
          </Dialog.Title>
          {description && (
            <Dialog.Description asChild>
              <div className="mt-1 text-sm text-muted">{description}</div>
            </Dialog.Description>
          )}
          <div className="mt-4">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** A panel from the right edge, for the detail of one row. */
export function Drawer({
  open,
  title,
  onClose,
  children,
}: {
  open: boolean;
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={(next) => !next && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed bottom-0 right-0 top-0 z-50 flex w-full max-w-md flex-col border-l border-line bg-surface text-ink focus:outline-none"
        >
          <div className="flex h-14 flex-none items-center justify-between gap-3 border-b border-line px-5">
            <Dialog.Title className="min-w-0 truncate font-display text-lg font-medium tracking-heading">
              {title}
            </Dialog.Title>
            <Dialog.Close className={buttonClass("quiet")}>Close</Dialog.Close>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-5">{children}</div>
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
      className={cn("inline-flex gap-0.5 rounded-lg bg-raised p-0.5", className)}
    >
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          aria-pressed={value === option.value}
          onClick={() => onChange(option.value)}
          className={cn(
            "h-8 whitespace-nowrap rounded-md px-2.5 text-[13px] font-medium transition-colors",
            value === option.value ? "bg-line text-ink" : "text-muted hover:text-ink",
          )}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden className={cn("skeleton rounded-md bg-raised", className)} />;
}

/** Rows of a table that hasn't answered yet, so the page doesn't jump when it does. */
export function SkeletonRows({ rows = 6 }: { rows?: number }) {
  return (
    <div role="status" aria-label="Loading" className="divide-y divide-hairline">
      {Array.from({ length: rows }, (_, index) => (
        <div key={index} className="flex items-center gap-6 px-4 py-3">
          <Skeleton className="h-4 w-20" />
          <Skeleton className="h-4 w-16" />
          <Skeleton className="h-4 flex-1" />
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
    <div className="flex flex-col items-center px-6 py-14 text-center">
      <p className="font-display text-lg font-medium tracking-heading">{title}</p>
      {children && <p className="mt-1 max-w-md text-sm text-muted">{children}</p>}
      {action && <div className="mt-4">{action}</div>}
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
    <div role="alert" className="flex flex-col items-center px-6 py-14 text-center">
      <p className="font-display text-lg font-medium tracking-heading text-bad">{title}</p>
      {detail && <p className="mt-1 max-w-xl break-words text-sm text-muted">{detail}</p>}
      {onRetry && (
        <Button className="mt-4" onClick={onRetry}>
          Try again
        </Button>
      )}
    </div>
  );
}

/** Reads like a table on a wide screen, and scrolls sideways on a narrow one. */
export function TableFrame({ children }: { children: ReactNode }) {
  return <div className="relative overflow-x-auto">{children}</div>;
}

export const thClass = "h-9 px-4 text-[13px] font-medium text-faint";
export const tdClass = "h-11 px-4";

export function SortHeader({
  label,
  active,
  dir,
  onClick,
  className,
}: {
  label: string;
  active: boolean;
  dir: "asc" | "desc";
  onClick: () => void;
  className?: string;
}) {
  return (
    <th
      aria-sort={active ? (dir === "asc" ? "ascending" : "descending") : "none"}
      className={cn(thClass, className)}
    >
      <button
        type="button"
        onClick={onClick}
        className={cn("inline-flex items-center gap-1 hover:text-ink", active && "text-ink")}
      >
        {label}
        <svg aria-hidden viewBox="0 0 8 12" className="h-3 w-2 fill-current">
          <path d="M4 0 8 5H0z" opacity={active && dir === "asc" ? 1 : 0.3} />
          <path d="M4 12 0 7h8z" opacity={active && dir === "desc" ? 1 : 0.3} />
        </svg>
      </button>
    </th>
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
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-4 py-2 text-[13px] text-muted">
      <span className="tabular">
        {total === 0 ? "None" : `${(offset + 1).toLocaleString()} to ${last.toLocaleString()}`} of{" "}
        {total.toLocaleString()}
      </span>
      <div className="flex items-center gap-2">
        <Select
          label="Rows per page"
          className="h-8"
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
          className="h-8 px-2.5 text-[13px]"
          disabled={offset === 0}
          onClick={() => onOffset(Math.max(0, offset - limit))}
        >
          Previous
        </Button>
        <Button
          className="h-8 px-2.5 text-[13px]"
          disabled={last >= total}
          onClick={() => onOffset(offset + limit)}
        >
          Next
        </Button>
      </div>
    </div>
  );
}

export function SearchIcon({ className }: { className?: string }) {
  return (
    <svg aria-hidden viewBox="0 0 16 16" fill="none" className={cn("h-4 w-4", className)}>
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
      <SearchIcon className="pointer-events-none absolute left-2.5 top-2.5 text-faint" />
      <input aria-label={label} {...props} className={cn(inputClass, "pl-8")} />
    </div>
  );
}

/** Facts in two columns. */
export function Facts({ items, className }: { items: [string, ReactNode][]; className?: string }) {
  return (
    <dl className={cn("grid grid-cols-[auto_1fr] gap-x-6 gap-y-1.5 text-sm", className)}>
      {items.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-faint">{label}</dt>
          <dd className="min-w-0 break-words text-right">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
