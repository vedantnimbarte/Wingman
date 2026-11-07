/* ── Notifications ─────────────────────────────────────────────────────────
 *
 * The same actionable cards the desktop popup draws: approvals, questions from
 * agents, failures. One inbox, polled once, read by three places — the header
 * bell, the Overview's "Needs you" list, and a single toast for a card that
 * arrives while the page is open.
 *
 * This replaces a filter the panel used to keep for itself: browser
 * notifications raised straight off the `/v1/events` stream for the two run
 * transitions a local `notifiable()` deemed worth interrupting someone for.
 * That was a second notification system with its own source and its own idea
 * of what mattered, and the two disagreed — a question from `ask_user` is not
 * a run transition, so it reached the popup and never reached here.
 *
 * The browser notification survives, because an in-page card cannot reach a
 * backgrounded tab — but it is raised from a card rather than from an event,
 * so what pops up and what the inbox shows cannot drift apart.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react'
import { api, type NotificationAction, type WingmanNotification } from './api'
import { ago } from './Sessions'
import { notificationsAvailable, notificationsOn, toggleNotifications, useEvents } from './state'
import { Icon } from './ui'

/** How often the inbox is re-read. Cards arrive from processes that are not on
 *  the event stream — `ask_user` in a detached run, most of all — so there is
 *  nothing to subscribe to and this is a poll. */
const POLL_MS = 3000

export type Tone = 'failed' | 'asserted' | 'proven' | 'neutral'

/** Colour encodes status and nothing else — the rule `app.css` states.
 *  Shared verbatim with the popup's `wire.ts`. */
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

/** Empty text is no text: a blank box must not read as an answer of "". */
export function replyPayload(
  action: string | null,
  text: string,
): { action: string | null; text: string | null } {
  const trimmed = text.trim()
  return { action, text: trimmed === '' ? null : trimmed }
}

/** Whether a button commits immediately rather than filling the box. */
export function commits(a: NotificationAction): boolean {
  return a.control !== undefined && a.control !== null
}

/** Cards worth a browser notification: the ones that are waiting on a person.
 *
 *  A card with no buttons and no box is something that already happened, and
 *  interrupting someone for it is what made the old filter feel noisy. */
export function interrupts(n: WingmanNotification): boolean {
  return (n.actions?.length ?? 0) > 0 || n.free_text === true
}

function announce(n: WingmanNotification) {
  if (!notificationsOn() || !interrupts(n)) return
  try {
    const note = new Notification(`Wingman — ${n.title}`, {
      body: [n.project, n.body].filter(Boolean).join(' · ') || undefined,
      // One per card, so a re-poll cannot stack duplicates.
      tag: n.id,
    })
    note.onclick = () => {
      window.focus()
      note.close()
    }
  } catch {
    // Some browsers throw outside a service worker. A missing notification
    // must not take the inbox down with it.
  }
}

/* ── The inbox ─────────────────────────────────────────────────────────── */

type Inbox = {
  cards: WingmanNotification[]
  busy: string | null
  answer: (id: string, action: string | null, text: string | null) => void
}

const InboxContext = createContext<Inbox>({ cards: [], busy: null, answer: () => {} })

export const useInbox = () => useContext(InboxContext)

export function NotificationsProvider({ children }: { children: React.ReactNode }) {
  const [cards, setCards] = useState<WingmanNotification[]>([])
  const [busy, setBusy] = useState<string | null>(null)
  // The newest card that arrived while the page was open and waits on a
  // person. The backlog on first load is not news, so it never toasts.
  const [fresh, setFresh] = useState<WingmanNotification | null>(null)
  const announced = useRef<Set<string>>(new Set())
  const loaded = useRef(false)
  const { tick } = useEvents()

  const refresh = useCallback(async () => {
    let next: WingmanNotification[]
    try {
      next = await api.notifications()
    } catch {
      // A daemon that went away is already reported by the shell's link
      // indicator; the inbox just stops updating.
      return
    }
    for (const n of next) {
      if (!announced.current.has(n.id)) {
        announced.current.add(n.id)
        announce(n)
        if (loaded.current && interrupts(n)) setFresh(n)
      }
    }
    loaded.current = true
    setCards(next)
    // A toast for a card answered somewhere else is stale.
    setFresh((f) => (f && next.some((n) => n.id === f.id) ? f : null))
  }, [])

  useEffect(() => {
    void refresh()
    const t = window.setInterval(() => void refresh(), POLL_MS)
    return () => window.clearInterval(t)
  }, [refresh])

  // Run transitions often raise a card, so react to them rather than waiting
  // out the poll.
  useEffect(() => {
    void refresh()
  }, [tick, refresh])

  const answer = useCallback(
    async (id: string, action: string | null, text: string | null) => {
      setBusy(id)
      // Optimistic: the card goes now. A failed write puts it back on the next
      // poll, which is a better outcome than a card that sits there looking
      // unclicked after a click that worked.
      setCards((prev) => prev.filter((c) => c.id !== id))
      setFresh((f) => (f?.id === id ? null : f))
      try {
        await api.answerNotification(id, { action, text })
      } finally {
        setBusy(null)
        void refresh()
      }
    },
    [refresh],
  )

  const value = useMemo(() => ({ cards, busy, answer }), [cards, busy, answer])

  return (
    <InboxContext.Provider value={value}>
      {children}
      {fresh && (
        <div className="notif-toast" role="region" aria-label="New notification">
          <NotificationCard
            n={fresh}
            busy={busy === fresh.id}
            onAnswer={answer}
            onHide={() => setFresh(null)}
          />
        </div>
      )}
    </InboxContext.Provider>
  )
}

/** The header bell: how much is waiting, and the inbox behind it. */
export function InboxButton() {
  const { cards, busy, answer } = useInbox()
  const [open, setOpen] = useState(false)
  const [alerts, setAlerts] = useState(notificationsOn)
  const ref = useRef<HTMLDivElement | null>(null)
  const waiting = cards.filter(interrupts).length

  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('mousedown', onDown)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('mousedown', onDown)
      window.removeEventListener('keydown', onKey)
    }
  }, [open])

  const label = cards.length
    ? `Inbox: ${cards.length} item${cards.length === 1 ? '' : 's'}${
        waiting ? `, ${waiting} waiting on you` : ''
      }`
    : 'Inbox: nothing new'

  return (
    <div className="inbox" ref={ref}>
      <button
        type="button"
        className="button button-quiet button-icon inbox-trigger"
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={label}
        title={label}
        onClick={() => setOpen((o) => !o)}
      >
        <Icon name="bell" />
        {cards.length > 0 && (
          <span className={`icon-badge${waiting ? '' : ' is-quiet'}`} aria-hidden="true">
            {cards.length}
          </span>
        )}
      </button>

      {open && (
        <div className="inbox-panel" role="dialog" aria-label="Inbox">
          <div className="inbox-head">
            <h2>Inbox</h2>
            {(notificationsAvailable() || alerts) && (
              <label
                className="check"
                title="Show a desktop alert when something needs you, even while this tab is in the background"
              >
                <input
                  type="checkbox"
                  checked={alerts}
                  onChange={() => void toggleNotifications().then(setAlerts)}
                />
                Desktop alerts
              </label>
            )}
          </div>
          {cards.length === 0 ? (
            <div className="inbox-empty">
              <span className="inbox-empty-icon" aria-hidden="true">
                <Icon name="check" />
              </span>
              <p>You're all caught up.</p>
              <p className="faint">Approvals, questions from agents and failures show up here.</p>
            </div>
          ) : (
            <div className="inbox-list">
              {cards.map((n) => (
                <NotificationCard key={n.id} n={n} busy={busy === n.id} onAnswer={answer} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

/**
 * One notification.
 *
 * Every card can be dismissed and the dismissal is recorded, because "I have
 * seen this" is the answer to a card with no buttons — and it is what stops it
 * coming back. On the arrival toast the close button only hides the toast;
 * the card stays in the inbox until it is answered.
 */
export function NotificationCard({
  n,
  busy,
  onAnswer,
  onHide,
}: {
  n: WingmanNotification
  busy: boolean
  onAnswer: (id: string, action: string | null, text: string | null) => void
  onHide?: () => void
}) {
  const [chosen, setChosen] = useState<string | null>(null)
  const [text, setText] = useState('')
  const actions = n.actions ?? []
  const t = tone(n.severity)

  function answer(action: string | null, typed = text) {
    const { action: a, text: tx } = replyPayload(action, typed)
    onAnswer(n.id, a, tx)
  }

  return (
    <div className={`notif ${t}`}>
      <div className="notif-head">
        <span className={`lamp${t === 'neutral' ? '' : ` is-${t}`}`} aria-hidden="true" />
        <span className="notif-title">{n.title}</span>
        <button
          className="button button-quiet button-sm button-icon notif-x"
          title={onHide ? 'Hide — it stays in the inbox' : 'Dismiss'}
          aria-label={onHide ? `Hide: ${n.title}` : `Dismiss: ${n.title}`}
          disabled={busy}
          onClick={() => (onHide ? onHide() : onAnswer(n.id, null, null))}
        >
          <Icon name="close" size={14} />
        </button>
      </div>

      {(n.project || n.created_at > 0) && (
        <div className="notif-meta">
          {n.project && <span>{n.project}</span>}
          {n.created_at > 0 && <span>{ago(n.created_at)}</span>}
        </div>
      )}

      {n.body && <div className="notif-body">{n.body}</div>}

      {actions.length > 0 && (
        <div className="notif-row">
          {actions.map((a, i) =>
            // A control button is a decision, not a draft: Approve must not
            // wait on anything else on the card. A suggestion on a question is
            // the opposite — it fills the box so it can still be edited.
            commits(a) ? (
              <button
                key={a.id}
                className={`button button-sm${i === 0 ? ' button-primary' : ''}`}
                disabled={busy}
                onClick={() => answer(a.id, '')}
              >
                {a.label}
              </button>
            ) : (
              <button
                key={a.id}
                className="button button-sm notif-chip"
                aria-pressed={chosen === a.id}
                disabled={busy}
                onClick={() => {
                  setChosen(a.id)
                  if (n.free_text) setText(a.label)
                  else answer(a.id, '')
                }}
              >
                {a.label}
              </button>
            ),
          )}
        </div>
      )}

      {n.free_text && (
        <form
          className="notif-row"
          onSubmit={(e) => {
            e.preventDefault()
            answer(chosen)
          }}
        >
          <input
            type="text"
            className="input"
            placeholder="Type your answer…"
            aria-label={`Answer: ${n.title}`}
            value={text}
            disabled={busy}
            onChange={(e) => setText(e.target.value)}
          />
          <button className="button button-primary button-sm" type="submit" disabled={busy}>
            Send
          </button>
        </form>
      )}
    </div>
  )
}
