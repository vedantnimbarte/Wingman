import { createContext, useCallback, useContext, useEffect, useState } from 'react'
import { ApiError, api, type Health, type Project } from './api'

/* ── Session ──────────────────────────────────────────────────────────────
 *
 * `GET /v1/health` answers both questions the shell needs before it can render
 * anything: is the daemon there, and does it want a credential. Whether we
 * *have* one is not knowable directly — the cookie is `HttpOnly` — so it is
 * probed by asking for something gated and seeing whether it comes back 401.
 */

export type Session =
  | { kind: 'loading' }
  | { kind: 'unreachable'; detail: string }
  | { kind: 'needs-token'; health: Health }
  | { kind: 'ready'; health: Health }

export function useSession() {
  const [session, setSession] = useState<Session>({ kind: 'loading' })

  const probe = useCallback(async () => {
    setSession({ kind: 'loading' })
    let health: Health
    try {
      health = await api.health()
    } catch (e) {
      return setSession({ kind: 'unreachable', detail: message(e) })
    }

    if (!health.auth_required) return setSession({ kind: 'ready', health })

    // The cookie cannot be read from script, so the only honest test is to use
    // it. `/v1/projects` is the cheapest gated route.
    try {
      await api.projects()
      setSession({ kind: 'ready', health })
    } catch (e) {
      if (e instanceof ApiError && e.isUnauthorized) {
        setSession({ kind: 'needs-token', health })
      } else {
        setSession({ kind: 'unreachable', detail: message(e) })
      }
    }
  }, [])

  useEffect(() => {
    void probe()
  }, [probe])

  return { session, probe }
}

export function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/* ── Projects ─────────────────────────────────────────────────────────── */

export function useProjects(enabled: boolean) {
  const [projects, setProjects] = useState<Project[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!enabled) return
    let live = true
    api
      .projects()
      .then((p) => live && setProjects(p))
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [enabled])

  return { projects, error }
}

/* ── Events ───────────────────────────────────────────────────────────────
 *
 * One `EventSource` on `/v1/events` for the whole app. It is the same detector
 * outbound push uses, so what the panel shows and what lands in a webhook
 * cannot disagree — and every later view subscribes to this rather than
 * growing a poll of its own.
 *
 * `EventSource` cannot set request headers, so a bearer token would have had
 * to travel in the query string and into every access log. The `HttpOnly`
 * cookie rides along on its own.
 */

export type Link = 'connecting' | 'live' | 'down'

export type RunEvent = {
  event: string
  project?: string
  run_id?: string
  [k: string]: unknown
}

type EventsValue = {
  link: Link
  /** Most recent first, capped — this is a live indicator, not a log store. */
  recent: RunEvent[]
  /**
   * Events seen since the tab opened. Monotonic, and the thing views should
   * put in a dependency array.
   *
   * `recent.length` was the obvious choice and was wrong: it saturates at
   * `KEEP` and then never changes again, so every screen that refetched on it
   * silently stopped refetching after fifty events — the panel kept reporting
   * "live" while showing a frozen board. A counter cannot saturate.
   */
  tick: number
}

const EventsContext = createContext<EventsValue>({ link: 'connecting', recent: [], tick: 0 })

const KEEP = 50

/* ── Desktop notifications ────────────────────────────────────────────────
 *
 * The plan gate is the panel's reason to exist: a run stops, waits for a
 * human, and until now did so silently in a background tab. The event that
 * says so is already on the wire — `[serve.push]` sends the same one to a
 * webhook — so this is a renderer for a signal the daemon already emits.
 *
 * Off unless asked for. A page that asks for notification permission on load
 * is a page people deny once and never re-enable.
 */

const NOTIFY_KEY = 'wingman.notify'

export function notificationsOn(): boolean {
  return (
    typeof Notification !== 'undefined' &&
    Notification.permission === 'granted' &&
    window.localStorage.getItem(NOTIFY_KEY) === 'on'
  )
}

/** Whether the browser will even let us ask. */
export function notificationsAvailable(): boolean {
  return typeof Notification !== 'undefined' && Notification.permission !== 'denied'
}

/** Returns whether they are on afterwards, so a caller can re-render on it. */
export async function toggleNotifications(): Promise<boolean> {
  if (notificationsOn()) {
    window.localStorage.removeItem(NOTIFY_KEY)
    return false
  }
  if (typeof Notification === 'undefined') return false
  const granted =
    Notification.permission === 'granted' ? 'granted' : await Notification.requestPermission()
  if (granted !== 'granted') return false
  window.localStorage.setItem(NOTIFY_KEY, 'on')
  return true
}

/**
 * Which events are worth interrupting someone for.
 *
 * Only the two that mean the machine has stopped and cannot continue without
 * you. `run.started` is something you did; a notification for it is a
 * notification for your own click.
 */
function notifiable(e: RunEvent): string | null {
  if (e.event === 'run.awaiting_approval') return 'Waiting for plan approval'
  if (e.event === 'run.finished') {
    const status = typeof e.status === 'string' ? e.status : null
    return status && status !== 'done' ? `Run ${status}` : null
  }
  return null
}

function announce(e: RunEvent) {
  const title = notifiable(e)
  if (!title || !notificationsOn()) return
  const where = [e.project, e.run_id].filter(Boolean).join(' · ')
  try {
    const n = new Notification(`Wingman — ${title}`, {
      body: where || undefined,
      // One notification per run, so a flapping stream cannot stack twenty.
      tag: typeof e.run_id === 'string' ? e.run_id : title,
    })
    n.onclick = () => {
      window.focus()
      n.close()
    }
  } catch {
    // Some browsers throw when constructing one outside a service worker.
    // A missing notification must not take the event stream down with it.
  }
}

export function EventsProvider({ children }: { children: React.ReactNode }) {
  const [link, setLink] = useState<Link>('connecting')
  const [recent, setRecent] = useState<RunEvent[]>([])
  const [tick, setTick] = useState(0)

  useEffect(() => {
    const src = new EventSource('/v1/events')

    src.onopen = () => setLink('live')
    // EventSource reconnects on its own; this reports the gap rather than
    // trying to manage it.
    src.onerror = () => setLink('down')
    src.onmessage = (m: MessageEvent<string>) => {
      const e = parse(m)
      announce(e)
      // The functional update keeps `recent` out of this effect's dependencies,
      // so the stream is not torn down and rebuilt on every event.
      setRecent((prev) => [e, ...prev].slice(0, KEEP))
      setTick((n) => n + 1)
    }

    return () => src.close()
  }, [])

  return (
    <EventsContext.Provider value={{ link, recent, tick }}>{children}</EventsContext.Provider>
  )
}

function parse(m: MessageEvent<string>): RunEvent {
  try {
    return { event: m.type, ...(JSON.parse(m.data) as object) }
  } catch {
    return { event: m.type, raw: m.data }
  }
}

export function useEvents() {
  return useContext(EventsContext)
}
