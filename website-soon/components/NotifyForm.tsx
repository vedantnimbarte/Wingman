"use client";

import { useState } from "react";
import { ArrowIcon } from "./icons";

type Status = "idle" | "error" | "done";

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/**
 * Visual-only waitlist capture. Validates the address and shows a success
 * state, but does NOT send or persist anything — wire a real provider
 * (Resend, ConvertKit, a Next.js route, …) into `onSubmit` when ready.
 */
export function NotifyForm() {
  const [email, setEmail] = useState("");
  const [status, setStatus] = useState<Status>("idle");

  function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!EMAIL_RE.test(email.trim())) {
      setStatus("error");
      return;
    }
    // Visual-only: no network call. Pretend it worked.
    setStatus("done");
  }

  if (status === "done") {
    return (
      <div
        className="flex items-center gap-3 rounded-xl border border-[var(--border-strong)] bg-[var(--surface)] px-5 py-4"
        aria-live="polite"
      >
        <span className="grid h-7 w-7 shrink-0 place-items-center rounded-full border border-[var(--border-strong)] bg-[var(--bg-soft)] text-[var(--text-strong)]">
          <svg
            viewBox="0 0 20 20"
            className="h-4 w-4"
            fill="none"
            stroke="currentColor"
            strokeWidth={2.2}
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden
          >
            <path d="M4 10.5l4 4 8-9.5" />
          </svg>
        </span>
        <div>
          <p className="text-sm font-semibold text-[var(--text-strong)]">
            You&rsquo;re on the list.
          </p>
          <p className="text-xs text-[var(--text-muted)]">
            We&rsquo;ll reach out at{" "}
            <span className="font-mono text-[var(--text)]">{email.trim()}</span>{" "}
            the moment Wingman ships.
          </p>
        </div>
      </div>
    );
  }

  return (
    <form onSubmit={onSubmit} noValidate className="w-full">
      <div className="flex flex-col gap-2.5 sm:flex-row">
        <div className="relative flex-1">
          <label htmlFor="notify-email" className="sr-only">
            Email address
          </label>
          <input
            id="notify-email"
            type="email"
            inputMode="email"
            autoComplete="email"
            placeholder="you@company.dev"
            value={email}
            onChange={(e) => {
              setEmail(e.target.value);
              if (status === "error") setStatus("idle");
            }}
            aria-invalid={status === "error"}
            className={`w-full rounded-xl border bg-[var(--surface)] px-4 py-3 font-mono text-sm text-[var(--text-strong)] placeholder:text-[var(--text-dim)] outline-none transition-colors ${
              status === "error"
                ? "border-[var(--color-french-400)]"
                : "border-[var(--border)] focus:border-[var(--border-strong)]"
            }`}
          />
        </div>
        <button
          type="submit"
          className="group inline-flex items-center justify-center gap-2 rounded-xl border border-[var(--text-strong)] bg-[var(--text-strong)] px-5 py-3 text-sm font-bold text-[var(--color-carbon-100)] transition-all hover:-translate-y-0.5 hover:shadow-[0_12px_30px_-12px_rgba(248,249,250,0.4)]"
        >
          Notify me
          <ArrowIcon className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
        </button>
      </div>
      <p
        className="mt-2.5 min-h-[1.1rem] text-xs"
        aria-live="polite"
      >
        {status === "error" ? (
          <span className="font-mono text-[var(--text-muted)]">
            Hmm, that doesn&rsquo;t look like an email. Mind checking it?
          </span>
        ) : (
          <span className="text-[var(--text-dim)]">
            One launch note. No spam, no sharing — unsubscribe anytime.
          </span>
        )}
      </p>
    </form>
  );
}
