import type { ReactNode } from "react";

/** macOS-style terminal chrome wrapper. */
export function TerminalWindow({
  title = "wingman",
  children,
  className,
}: {
  title?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={`glow-ring overflow-hidden rounded-xl border border-[var(--border)] bg-[var(--color-carbon-200)] ${className ?? ""}`}
    >
      <div className="flex items-center gap-2 border-b border-[var(--border)] bg-[var(--color-carbon-300)] px-4 py-3">
        <span className="h-3 w-3 rounded-full bg-[var(--color-iron-500)]" />
        <span className="h-3 w-3 rounded-full bg-[var(--color-iron-400)]" />
        <span className="h-3 w-3 rounded-full bg-[var(--color-iron-300)]" />
        <span className="ml-2 font-mono text-xs text-[var(--text-dim)]">
          {title}
        </span>
      </div>
      <div className="px-4 py-4 font-mono text-[0.82rem] leading-relaxed sm:px-5 sm:py-5">
        {children}
      </div>
    </div>
  );
}
