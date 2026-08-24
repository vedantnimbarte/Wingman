import { useEffect, useState } from 'react'

/** `GET /v1/health` — the one route that needs no token. */
type Health = {
  ok: boolean
  version: string
  uptime_secs: number
}

type State =
  | { kind: 'connecting' }
  | { kind: 'connected'; health: Health }
  | { kind: 'unreachable'; detail: string }

/**
 * Phase 0 is the delivery pipeline, not a feature: this page exists to prove
 * that a React bundle built by Vite, embedded by `build.rs`, and served by
 * `wingman serve` can reach the API it was served from. Phase 1 replaces it
 * with the real shell.
 */
export function App() {
  const [state, setState] = useState<State>({ kind: 'connecting' })

  useEffect(() => {
    let live = true
    fetch('/v1/health')
      .then((r) => {
        if (!r.ok) throw new Error(`${r.status} ${r.statusText}`)
        return r.json() as Promise<Health>
      })
      .then((health) => live && setState({ kind: 'connected', health }))
      .catch((e: unknown) => {
        if (live) setState({ kind: 'unreachable', detail: String(e) })
      })
    return () => {
      live = false
    }
  }, [])

  return (
    <main className="boot">
      <h1>Wingman</h1>
      <p>Web control panel — phase 0, delivery pipeline.</p>
      {state.kind === 'connecting' && <p>Reaching the daemon…</p>}
      {state.kind === 'unreachable' && (
        <>
          <p className="status-failed">
            No answer from the daemon. Start it with <code>wingman serve</code>, then reload.
          </p>
          <p className="figure">{state.detail}</p>
        </>
      )}
      {state.kind === 'connected' && (
        <dl>
          <div>
            <dt>Daemon</dt>
            <dd className="status-proven">reachable</dd>
          </div>
          <div>
            <dt>Version</dt>
            <dd className="figure">{state.health.version}</dd>
          </div>
          <div>
            <dt>Uptime</dt>
            <dd className="figure">{formatUptime(state.health.uptime_secs)}</dd>
          </div>
        </dl>
      )}
    </main>
  )
}

/** Seconds to the coarsest unit that still reads honestly: 90 → "1m 30s". */
export function formatUptime(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return '—'
  const s = Math.floor(secs)
  const parts: string[] = []
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (h) parts.push(`${h}h`)
  if (h || m) parts.push(`${m}m`)
  parts.push(`${s % 60}s`)
  return parts.join(' ')
}
