import Link from "next/link";

/** Monogram + wordmark. The mark is an angular "arc" bracket. */
export function Logo({ href = "/" }: { href?: string }) {
  return (
    <Link
      href={href}
      className="group inline-flex items-center gap-2.5"
      aria-label="Wingman home"
    >
      <span className="relative grid h-8 w-8 place-items-center rounded-lg border border-[var(--border-strong)] bg-[var(--color-carbon-300)] transition-colors group-hover:border-[var(--color-french-500)]">
        <svg
          width="18"
          height="18"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.2"
          strokeLinecap="round"
          strokeLinejoin="round"
          className="text-[var(--color-french-600)] transition-colors group-hover:text-[var(--text-strong)]"
        >
          <path d="M8 6 3 12l5 6" />
          <path d="m13 18 3-12" />
        </svg>
      </span>
      <span className="text-[15px] font-extrabold tracking-tight text-[var(--text-strong)]">
        Arc<span className="text-[var(--text-dim)]">-</span>Code
      </span>
    </Link>
  );
}
