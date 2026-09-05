/*
 * The shape of what the backend sends, and the small decisions that follow
 * from it. Pure: everything here is testable without a window.
 */

export type Action = {
  id: string
  label: string
  /** Present when this button drives a run's control channel rather than a
   *  reply. The backend looks it up by id — it never travels back through
   *  here — so it exists on this type only to say whether the button is one. */
  control?: unknown
}

export type Notification = {
  id: string
  severity: string
  title: string
  body: string
  project: string | null
  run_dir: string | null
  created_at: number
  expires_at: number | null
  actions: Action[]
  free_text: boolean
}

/*
 * Colour encodes epistemic status and nothing else — the panel's rule, and the
 * reason there is no brand hue here either. A failure is `--failed`, something
 * waiting on you is `--asserted` (running, unproven), a finished run is
 * `--proven`, and plain news gets no colour at all.
 */
export type Tone = 'failed' | 'asserted' | 'proven' | 'neutral'

export function tone(severity: string): Tone {
  switch (severity) {
    case 'escalation':
      return 'failed'
    case 'decision':
      return 'asserted'
    case 'progress':
      return 'proven'
    default:
      return 'neutral'
  }
}

/** Seconds since the epoch, matching the backend's clock. */
export function nowSecs(): number {
  return Math.floor(Date.now() / 1000)
}

/**
 * Whether the process that asked has stopped listening. An expired card is
 * dropped rather than answered: a reply would go nowhere, and recording one
 * would claim the user answered when they did not.
 */
export function expired(n: Notification, now: number): boolean {
  return n.expires_at !== null && n.expires_at <= now
}

/**
 * Whether a card may disappear on its own.
 *
 * Only when there is nothing to answer *and* nothing has gone wrong. A card
 * that a human owes a reply to must never expire off the screen on a timer —
 * that is the one failure mode that would make this worse than no popup at
 * all — and a failure stays until someone has actually seen it.
 */
export function autoDismisses(n: Notification): boolean {
  if (n.actions.length > 0 || n.free_text) return false
  return n.severity === 'info' || n.severity === 'progress'
}

export const AUTO_DISMISS_MS = 6000

/** How many cards are on screen before the rest collapse into a count. */
export const VISIBLE = 3

export type StackView = {
  /** Render order, oldest first — so the newest sits nearest the corner. */
  shown: Notification[]
  /** How many are folded away above them. */
  hidden: number
}

/**
 * What to draw, given everything open.
 *
 * Keeps the newest few and folds the rest into one row rather than growing a
 * scroll region: a scrollbar inside a frameless window with no chrome is an
 * affordance nobody finds.
 */
export function view(cards: Notification[], expanded: boolean): StackView {
  if (expanded || cards.length <= VISIBLE) return { shown: cards, hidden: 0 }
  return { shown: cards.slice(-VISIBLE), hidden: cards.length - VISIBLE }
}

/**
 * Normalise what the user did into what the backend stores.
 *
 * Whitespace-only text is not an answer — it falls back to whichever button
 * was pressed, which is how someone who clicks a suggestion without typing
 * still gets their choice recorded.
 */
export function replyPayload(
  action: string | null,
  text: string,
): { action: string | null; text: string | null } {
  const trimmed = text.trim()
  return { action, text: trimmed === '' ? null : trimmed }
}
