/* ── Notifications ─────────────────────────────────────────────────────────
 *
 * The same actionable cards the desktop popup draws, in the corner of the
 * panel.
 *
 * This replaces a filter the panel used to keep for itself: browser
 * notifications raised straight off the `/v1/events` stream for the two run
 * transitions a local `notifiable()` deemed worth interrupting someone for.
 * That was a second notification system with its own source and its own idea
 * of what mattered, and the two disagreed — a question from `ask_user` is not
 * a run transition, so it reached the popup and never reached here.
 *
 * Now both surfaces render one inbox, routed by one `[pilot.notifications]`.
 * The browser notification survives, because an in-page card cannot reach a
 * backgrounded tab — but it is raised from a card rather than from an event,
 * so what pops up and what the stack shows cannot drift apart.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { api, type NotificationAction, type WingmanNotification } from './api'
import { notificationsOn, useEvents } from './state'

/** How often the inbox is re-read. Cards arrive from processes that are not on
 *  the event stream — `ask_user` in a detached run, most of all — so there is
 *  nothing to subscribe to and this is a poll. */
const POLL_MS = 3000

export type Tone = 'failed' | 'asserted' | 'proven' | 'neutral'

/** Colour encodes epistemic status and nothing else — the rule `app.css`
 *  states. Shared verbatim with the popup's `wire.ts`. */
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
    // must not take the stack down with it.
  }
}

export function Notifications() {
  const [cards, setCards] = useState<WingmanNotification[]>([])
  const [busy, setBusy] = useState<string | null>(null)
  // Ids already announced, so a poll that re-sees a card does not re-ring.
  const announced = useRef<Set<string>>(new Set())
  const { tick } = useEvents()

  const refresh = useCallback(async () => {
    let next: WingmanNotification[]
    try {
      next = await api.notifications()
    } catch {
      // A daemon that went away is already reported by the shell's link
      // indicator; the stack just stops updating.
      return
    }
    for (const n of next) {
      if (!announced.current.has(n.id)) {
        announced.current.add(n.id)
        announce(n)
      }
    }
    setCards(next)
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
      try {
        await api.answerNotification(id, { action, text })
      } finally {
        setBusy(null)
        void refresh()
      }
    },
    [refresh],
  )

  if (cards.length === 0) return null

  return (
    <div className="notif-stack" role="region" aria-label="Notifications">
      {cards.map((n) => (
        <Card key={n.id} n={n} busy={busy === n.id} onAnswer={answer} />
      ))}
    </div>
  )
}

/**
 * One notification.
 *
 * Every card can be dismissed and the dismissal is recorded, because "I have
 * seen this" is the answer to a card with no buttons — and it is what stops it
 * coming back.
 */
function Card({
  n,
  busy,
  onAnswer,
}: {
  n: WingmanNotification
  busy: boolean
  onAnswer: (id: string, action: string | null, text: string | null) => void
}) {
  const [chosen, setChosen] = useState<string | null>(null)
  const [text, setText] = useState('')
  const actions = n.actions ?? []

  function answer(action: string | null, typed = text) {
    const { action: a, text: t } = replyPayload(action, typed)
    onAnswer(n.id, a, t)
  }

  return (
    <div className={`notif ${tone(n.severity)}`}>
      <div className="notif-head">
        <span className="notif-title">{n.title}</span>
        {n.project && <span className="notif-project">{n.project}</span>}
        <button
          className="button button-quiet button-sm notif-x"
          title="Dismiss"
          aria-label={`Dismiss: ${n.title}`}
          disabled={busy}
          onClick={() => onAnswer(n.id, null, null)}
        >
          ✕
        </button>
      </div>

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
            placeholder="Type an answer…"
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
