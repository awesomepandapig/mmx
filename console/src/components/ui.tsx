import * as Dialog from "@radix-ui/react-dialog";
import * as RadixSwitch from "@radix-ui/react-switch";
import clsx from "clsx";
import { useId, useState } from "react";
import type { ButtonHTMLAttributes, InputHTMLAttributes, ReactNode } from "react";
import { twMerge } from "tailwind-merge";

export function cn(...parts: Parameters<typeof clsx>) {
  return twMerge(clsx(...parts));
}

type Tone = "primary" | "plain" | "danger";

const tones: Record<Tone, string> = {
  primary: "bg-accent text-on-accent hover:brightness-95",
  plain: "border border-line bg-surface text-ink hover:bg-raised",
  danger: "border border-bad/40 bg-transparent text-bad hover:bg-bad/10",
};

export function Button({
  tone = "plain",
  className,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { tone?: Tone }) {
  return (
    <button
      type="button"
      {...props}
      className={cn(
        "inline-flex h-9 shrink-0 items-center justify-center whitespace-nowrap rounded-full px-4 text-sm font-medium transition disabled:cursor-not-allowed disabled:opacity-50",
        tones[tone],
        className,
      )}
    />
  );
}

export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <section className={cn("rounded-card border border-line bg-surface", className)}>
      {children}
    </section>
  );
}

export function PageHeader({
  title,
  detail,
  actions,
}: {
  title: string;
  detail?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div className="min-w-0">
        <h1 className="text-3xl font-semibold tracking-heading">{title}</h1>
        {detail && <p className="mt-1 max-w-2xl text-muted">{detail}</p>}
      </div>
      {actions && <div className="flex flex-wrap gap-2">{actions}</div>}
    </header>
  );
}

export function SectionTitle({ children }: { children: ReactNode }) {
  return <h2 className="text-lg font-semibold tracking-heading">{children}</h2>;
}

export type State = "good" | "warn" | "bad" | "idle";

const stateText: Record<State, string> = {
  good: "text-good",
  warn: "text-warn",
  bad: "text-bad",
  idle: "text-muted",
};

/** A status in words, with a dot. The word carries the meaning, so colour is never the only cue. */
export function Status({ state, children }: { state: State; children: ReactNode }) {
  return (
    <span className={cn("inline-flex items-center gap-2 text-sm font-medium", stateText[state])}>
      <span aria-hidden className="h-2 w-2 shrink-0 rounded-full bg-current" />
      {children}
    </span>
  );
}

const noticeTone: Record<State, string> = {
  good: "border-good/30 bg-good/10",
  warn: "border-warn/40 bg-warn/10",
  bad: "border-bad/40 bg-bad/10",
  idle: "border-line bg-raised",
};

export function Notice({
  state = "idle",
  title,
  children,
  action,
}: {
  state?: State;
  title?: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div
      role={state === "bad" ? "alert" : "status"}
      className={cn(
        "flex flex-wrap items-center justify-between gap-3 rounded-2xl border px-4 py-3 text-sm",
        noticeTone[state],
      )}
    >
      <div className="min-w-0 break-words">
        {title && <p className="font-semibold">{title}</p>}
        {children && <div className={title ? "text-muted" : undefined}>{children}</div>}
      </div>
      {action}
    </div>
  );
}

const fieldClass =
  "h-10 w-full rounded-xl border border-line bg-canvas px-3 text-sm text-ink placeholder:text-muted/70 focus:border-ink focus:outline-none disabled:opacity-60";

export function Field({
  label,
  hint,
  className,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: ReactNode }) {
  const id = useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="mb-1 block text-sm font-medium">
        {label}
      </label>
      <input id={id} {...props} className={fieldClass} />
      {hint && <p className="mt-1 text-xs text-muted">{hint}</p>}
    </div>
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
      className="relative h-6 w-10 shrink-0 rounded-full border border-line bg-raised transition data-[state=checked]:border-transparent data-[state=checked]:bg-accent disabled:opacity-50"
    >
      <RadixSwitch.Thumb className="block h-[18px] w-[18px] translate-x-[2px] rounded-full bg-ink transition data-[state=checked]:translate-x-[18px] data-[state=checked]:bg-on-accent" />
    </RadixSwitch.Root>
  );
}

export function Code({ children }: { children: ReactNode }) {
  return (
    <code className="break-all rounded-md bg-raised px-1.5 py-0.5 font-mono text-[0.85em]">
      {children}
    </code>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
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
          className="fixed left-1/2 top-1/2 z-50 max-h-[90dvh] w-[calc(100vw-2rem)] max-w-lg -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-card border border-line bg-surface p-6 focus:outline-none"
        >
          <Dialog.Title className="text-xl font-semibold tracking-heading">{title}</Dialog.Title>
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

/** Reads like a table on a wide screen, and scrolls sideways on a narrow one. */
export function TableFrame({ children }: { children: ReactNode }) {
  return <div className="relative overflow-x-auto">{children}</div>;
}
