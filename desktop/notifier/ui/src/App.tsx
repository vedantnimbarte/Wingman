import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { Card } from './Card'
import {
  AUTO_DISMISS_MS,
  VISIBLE,
  autoDismisses,
  expired,
  nowSecs,
  view,
  type Notification,
} from './wire'

/** How often the stack is swept for cards that should go on their own. */
const SWEEP_MS = 500

/**
 * A card plus when it appeared.
 *
 * The arrival time is what makes auto-dismissal a property of the card rather
 * than a `setTimeout` per card: one sweep handles both "this expired" and "this
 * has been read", and a new notification arriving cannot reset another card's
 * countdown the way a re-registered timer would.
 */
type Shown = Notification & { shownAt: number }

export function App() {
  const [cards, setCards] = useState<Shown[]>([])
  const [expanded, setExpanded] = useState(false)
  const root = useRef<HTMLDivElement>(null)

  /** The user answered, dismissed, or let a plain card time out. */
  const answer = useCallback((id: string, action: string | null, text: string | null) => {
    setCards((prev) => prev.filter((n) => n.id !== id))
    void invoke('reply', { id, action, text }).catch(() => {
      // The backend has already forgotten this card, or the run directory
      // failed validation. Either way it is off the screen and the backend
      // owns the diagnosis; there is nothing useful to say here.
    })
  }, [])

  const add = useCallback((n: Notification) => {
    setCards((prev) =>
      // Unanswered cards are replayed at startup and could in principle arrive
      // twice. Keep one.
      prev.some((c) => c.id === n.id) ? prev : [...prev, { ...n, shownAt: Date.now() }],
    )
  }, [])

  useEffect(() => {
    // `npm run dev` in a plain browser: there is no Tauri to listen to, so seed
    // the stack instead. Guarded by `import.meta.env.DEV`, so the import and
    // the branch are both gone from a production build.
    if (import.meta.env.DEV && !('__TAURI_INTERNALS__' in window)) {
      void import('./demo').then((m) => m.demoCards(nowSecs()).forEach(add))
      return
    }
    // Pull what is already open *before* trusting the stream. Emitting is
    // fire-and-forget on the Rust side, so anything raised while this page was
    // still loading — which is exactly when the startup replay runs — reached
    // no listener. `add` dedupes by id, so a card that arrives both ways is
    // shown once.
    void invoke<Notification[]>('open')
      .then((cards) => cards.forEach(add))
      .catch(() => {})
    const stop = listen<Notification>('notification', (e) => add(e.payload))
    return () => {
      void stop.then((off) => off())
    }
  }, [add])

  useEffect(() => {
    const timer = setInterval(() => {
      const now = nowSecs()
      const cutoff = Date.now() - AUTO_DISMISS_MS

      setCards((prev) => {
        // Expiry leaves no reply: the process that asked has stopped
        // listening, and recording an answer now would claim the user gave one.
        const gone = prev.filter((n) => expired(n, now))
        // Plain news goes on its own once it has been on screen long enough.
        // Anything with a question in it never does — that rule lives in
        // `autoDismisses`, and it is the one that keeps this feature honest.
        const read = prev.filter(
          (n) => !expired(n, now) && autoDismisses(n) && n.shownAt <= cutoff,
        )
        if (gone.length === 0 && read.length === 0) return prev

        for (const n of gone) void invoke('forget', { id: n.id }).catch(() => {})
        for (const n of read) {
          void invoke('reply', { id: n.id, action: null, text: null }).catch(() => {})
        }
        const dropped = new Set([...gone, ...read].map((n) => n.id))
        return prev.filter((n) => !dropped.has(n.id))
      })
    }, SWEEP_MS)
    return () => clearInterval(timer)
  }, [])

  // Collapse once the overflow is gone, or one look leaves it expanded forever.
  useEffect(() => {
    if (expanded && cards.length <= VISIBLE) setExpanded(false)
  }, [cards.length, expanded])

  const { shown, hidden } = view(cards, expanded)

  // The window is sized to its content, and an empty stack hides it — which is
  // what makes it click-through without any per-platform cursor trickery.
  useLayoutEffect(() => {
    const el = root.current
    if (!el) return
    const send = () => {
      void invoke('resize', {
        height: cards.length === 0 ? 0 : Math.ceil(el.scrollHeight),
      }).catch(() => {})
    }
    send()
    const observer = new ResizeObserver(send)
    observer.observe(el)
    return () => observer.disconnect()
  }, [cards.length, shown.length, hidden])

  return (
    <div ref={root} id="stack">
      {hidden > 0 && (
        <button className="more" onClick={() => setExpanded(true)}>
          +{hidden} more
        </button>
      )}
      {shown.map((n) => (
        <Card key={n.id} n={n} onAnswer={answer} />
      ))}
    </div>
  )
}
