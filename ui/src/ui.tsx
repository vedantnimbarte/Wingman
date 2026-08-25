/**
 * The shared furniture: page heads, status pills, icons, and the loading,
 * failure and empty states.
 *
 * Defined once, here, on purpose: five feature phases will otherwise each
 * invent their own, and a panel where every section fails differently — or
 * starts its heading at a different height — is a panel nobody trusts.
 *
 * The voice: say what happened and what to do about it. No apologies, no
 * vagueness about which thing broke, and an empty screen is an invitation to
 * act rather than a shrug.
 */

/* ── Icons ─────────────────────────────────────────────────────────────────
 *
 * Drawn here rather than pulled from an icon package. Nine glyphs at one
 * weight is a few hundred bytes of path data; the smallest icon library is a
 * dependency, a build step and a bundle for the same nine shapes.
 */

const PATHS = {
  overview: 'M3 8.5 10 3l7 5.5V16a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1Z',
  board: 'M3.5 3.5h4v13h-4zM12.5 3.5h4v8h-4z',
  runs: 'M4 3.5v13M4 5h8.5l-1.5 2.5L12.5 10H4',
  sessions: 'M3.5 5a1.5 1.5 0 0 1 1.5-1.5h10A1.5 1.5 0 0 1 16.5 5v7a1.5 1.5 0 0 1-1.5 1.5H8l-4 3v-3H5A1.5 1.5 0 0 1 3.5 12Z',
  insights: 'M3.5 16.5h13M6 16.5v-5M10 16.5V5M14 16.5v-8',
  config: 'M4 6h12M4 14h12M8 3.5v5M13 11.5v5',
  search: 'M8.75 14a5.25 5.25 0 1 0 0-10.5 5.25 5.25 0 0 0 0 10.5ZM12.6 12.6 16.5 16.5',
  sun: 'M10 13.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7ZM10 2v1.5M10 16.5V18M18 10h-1.5M3.5 10H2M15.7 4.3l-1 1M5.3 14.7l-1 1M15.7 15.7l-1-1M5.3 5.3l-1-1',
  moon: 'M16 11.7A6.6 6.6 0 0 1 8.3 4a6.6 6.6 0 1 0 7.7 7.7Z',
  collapse: 'M12 5.5 7.5 10l4.5 4.5',
  expand: 'M8 5.5 12.5 10 8 14.5',
  close: 'M5.5 5.5l9 9M14.5 5.5l-9 9',
} as const

export type IconName = keyof typeof PATHS

export function Icon({ name, size = 16 }: { name: IconName; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={PATHS[name]} />
    </svg>
  )
}

/* ── Page head ─────────────────────────────────────────────────────────── */

/**
 * The opening block of every view: eyebrow, title, optional sentence, and the
 * screen's actions on the right. Shared so the six sections start their
 * content at the same height, which is the difference between a product and
 * six pages that happen to share a stylesheet.
 */
export function PageHead({
  eyebrow,
  title,
  intro,
  actions,
}: {
  eyebrow: React.ReactNode
  title: React.ReactNode
  intro?: React.ReactNode
  actions?: React.ReactNode
}) {
  return (
    <header className="page-head">
      <div className="page-title">
        <span className="eyebrow">{eyebrow}</span>
        <h1>{title}</h1>
        {intro && <p className="page-intro">{intro}</p>}
      </div>
      {actions && <div className="actions">{actions}</div>}
    </header>
  )
}

/* ── Status ────────────────────────────────────────────────────────────── */

/**
 * A status as a value: glyph, label, and a wash mixed from the status hue.
 *
 * The glyph is not decoration — it is the second channel, so the three states
 * are still distinguishable to a reader who cannot separate the three hues.
 */
export function Pill({
  status,
  glyph,
  children,
}: {
  status: string
  glyph: string
  children: React.ReactNode
}) {
  return (
    <span className={`pill ${status}`}>
      <span className="pill-glyph" aria-hidden="true">
        {glyph}
      </span>
      {children}
    </span>
  )
}

/* ── States ────────────────────────────────────────────────────────────── */

export function Loading({ what }: { what: string }) {
  return (
    <div className="state state-center" role="status">
      <span className="spinner" aria-hidden="true" />
      <p className="muted">Loading {what}…</p>
    </div>
  )
}

/**
 * `action` names what the button does, because "Try again" on a page that was
 * never going to exist is a lie about what the click will accomplish. A
 * control says exactly what happens when it is used.
 */
export function Failed({
  title,
  detail,
  action,
}: {
  title: string
  detail?: string
  action?: { label: string; onClick: () => void }
}) {
  return (
    <div className="state state-plain">
      <h2 className="is-failed dot">{title}</h2>
      {detail && <p className="figure">{detail}</p>}
      {action && (
        <button type="button" className="button" onClick={action.onClick}>
          {action.label}
        </button>
      )}
    </div>
  )
}

/** An empty screen with the one thing to do next inside it. */
export function Empty({
  title,
  children,
  action,
}: {
  title: string
  children: React.ReactNode
  action?: { label: string; onClick: () => void }
}) {
  return (
    <div className="state">
      <h2>{title}</h2>
      <p>{children}</p>
      {action && (
        <button type="button" className="button button-primary" onClick={action.onClick}>
          {action.label}
        </button>
      )}
    </div>
  )
}

/**
 * An inline result: a save that landed, a control that was recorded, a delete
 * that half-worked. `tone` is one of the three status classes, or omitted when
 * the note is neither true nor false yet — a plain fact about what just
 * happened.
 */
export function Note({
  tone,
  role = 'status',
  children,
}: {
  tone?: 'is-proven' | 'is-asserted' | 'is-failed'
  role?: 'status' | 'alert'
  children: React.ReactNode
}) {
  return (
    <p className={`note ${tone ?? ''}`} role={role}>
      {tone && <span className={`dot ${tone}`} aria-hidden="true" />}
      <span>{children}</span>
    </p>
  )
}
