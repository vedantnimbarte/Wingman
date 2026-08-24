import { useState } from 'react'
import { api, type Health, type Project } from './api'
import { Board } from './Board'
import { Runs } from './Runs'
import { Config } from './Config'
import { navigate, segments, useRoute } from './router'
import { EventsProvider, message, useEvents, useProjects, useSession } from './state'
import { Failed, Loading, NotYet } from './ui'

const SECTIONS = [
  { path: '/', label: 'Overview' },
  { path: '/board', label: 'Board' },
  { path: '/runs', label: 'Runs' },
  { path: '/sessions', label: 'Sessions' },
  { path: '/config', label: 'Config' },
  { path: '/insights', label: 'Insights' },
] as const

export function App() {
  const { session, probe } = useSession()

  switch (session.kind) {
    case 'loading':
      return <Loading what="the panel" />
    case 'unreachable':
      return (
        <Failed
          title="No answer from the daemon"
          detail={session.detail}
          action={{ label: 'Reconnect', onClick: () => void probe() }}
        />
      )
    case 'needs-token':
      return <SignIn onDone={() => void probe()} />
    case 'ready':
      return (
        <EventsProvider>
          <Shell health={session.health} />
        </EventsProvider>
      )
  }
}

/* ── Sign in ───────────────────────────────────────────────────────────── */

function SignIn({ onDone }: { onDone: () => void }) {
  const [token, setToken] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await api.signIn(token)
      onDone()
    } catch (err) {
      // The server refuses to say whether a wrong token was close, so neither
      // does this.
      setError(message(err))
      setBusy(false)
    }
  }

  return (
    <div className="signin">
      <form onSubmit={submit}>
        <h1>Wingman</h1>
        <p>This daemon requires a token. It is stored in a cookie this page cannot read.</p>

        <label htmlFor="token">API token</label>
        <input
          id="token"
          className="input"
          type="password"
          autoComplete="current-password"
          autoFocus
          value={token}
          onChange={(e) => setToken(e.target.value)}
          spellCheck={false}
        />

        {error && (
          <p className="is-failed dot figure" role="alert">
            {error}
          </p>
        )}

        <button type="submit" className="button" disabled={busy || token.trim() === ''}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>

        <p style={{ marginTop: '1rem' }}>
          Generate one with <code className="figure">wingman serve --init-token</code>. It is
          printed exactly once.
        </p>
      </form>
    </div>
  )
}

/* ── Shell ─────────────────────────────────────────────────────────────── */

function Shell({ health }: { health: Health }) {
  const path = useRoute()
  const { projects, error } = useProjects(true)
  const [selected, setSelected] = useState<string | null>(
    () => window.localStorage.getItem('wingman.project') ?? null,
  )

  function choose(id: string) {
    setSelected(id)
    window.localStorage.setItem('wingman.project', id)
  }

  const active = projects?.find((p) => p.id === selected) ?? projects?.[0] ?? null

  return (
    <div className="shell">
      <div className="shell-brand">wingman</div>

      <header className="shell-header">
        <ProjectPicker
          projects={projects}
          error={error}
          selected={active?.id ?? null}
          onChoose={choose}
        />
        <LinkState />
      </header>

      <nav className="shell-nav" aria-label="Sections">
        {SECTIONS.map((s) => (
          <button
            key={s.path}
            type="button"
            className="nav-item"
            // Compared on the section root so a nested route like
            // `/runs/{id}` still marks Runs as the current section.
            aria-current={`/${segments(path)[0] ?? ''}` === s.path ? 'page' : undefined}
            onClick={() => navigate(s.path)}
          >
            {s.label}
          </button>
        ))}
      </nav>

      <main className="shell-main">
        <Section path={path} health={health} project={active} />
      </main>
    </div>
  )
}

function ProjectPicker({
  projects,
  error,
  selected,
  onChoose,
}: {
  projects: Project[] | null
  error: string | null
  selected: string | null
  onChoose: (id: string) => void
}) {
  if (error) return <span className="is-failed dot figure">{error}</span>
  if (!projects) return <span className="muted figure">loading projects…</span>
  if (projects.length === 0) {
    return (
      <span className="muted figure">
        no projects — add one under [[serve.projects]] in the global config
      </span>
    )
  }

  return (
    <label>
      <span className="eyebrow" style={{ marginRight: '0.5rem' }}>
        Project
      </span>
      <select
        className="select"
        value={selected ?? ''}
        onChange={(e) => onChoose(e.target.value)}
      >
        {projects.map((p) => (
          <option key={p.id} value={p.id}>
            {p.id}
          </option>
        ))}
      </select>
    </label>
  )
}

/** Live-stream state. The one place the shell reports on itself. */
function LinkState() {
  const { link } = useEvents()
  const status =
    link === 'live'
      ? { cls: 'is-proven', text: 'live' }
      : link === 'connecting'
        ? { cls: 'is-asserted', text: 'connecting' }
        : { cls: 'is-failed', text: 'reconnecting' }

  return (
    <span className={`dot figure ${status.cls}`} title="Event stream from /v1/events">
      {status.text}
    </span>
  )
}

/* ── Sections ──────────────────────────────────────────────────────────── */

function Section({
  path,
  health,
  project,
}: {
  path: string
  health: Health
  project: Project | null
}) {
  // Matched on the first segment so nested routes like `/runs/{id}` reach
  // their section rather than falling through to "No such page".
  const [head, ...rest] = segments(path)

  switch (`/${head ?? ''}`) {
    case '/':
      return <Overview health={health} project={project} />
    case '/board':
      // The board's project ids come from its own registry, which is global.
      // They coincide with `serve`'s allowlist ids in the common case because
      // both derive from the directory name, but they are not the same
      // namespace — so an unmatched id shows the whole board rather than an
      // empty one.
      return <Board project={project?.id ?? null} />
    case '/runs':
      return <Runs project={project?.id ?? null} runId={rest[0] ?? null} />
    case '/sessions':
      return (
        <NotYet title="Sessions" phase="Phase 5">
          Transcripts and streaming turns. A session started here shows up in{' '}
          <code>wingman session list</code> like any other.
        </NotYet>
      )
    case '/config':
      return <Config />
    case '/insights':
      return (
        <NotYet title="Insights" phase="Phase 6">
          Token spend and what the same work would have cost on another model, the per-turn context
          tax, and <code>doctor</code>.
        </NotYet>
      )
    default:
      return (
        <div className="view">
          <Failed
            title="No such page"
            detail={path}
            action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
          />
        </div>
      )
  }
}

function Overview({ health, project }: { health: Health; project: Project | null }) {
  const { recent, link } = useEvents()

  return (
    <div className="view">
      <span className="eyebrow">Phase 1</span>
      <h1>Overview</h1>
      <p className="view-intro">
        The shell, the design system, and the live event stream. Board, runs, config, and sessions
        arrive in the phases named in the sections beside this one.
      </p>

      <div className="rows">
        <div className="row">
          <span className="muted">Daemon</span>
          <span className="figure is-proven dot">reachable</span>
        </div>
        <div className="row">
          <span className="muted">Version</span>
          <span className="figure">{health.version}</span>
        </div>
        <div className="row">
          <span className="muted">Uptime</span>
          <span className="figure">{formatUptime(health.uptime_secs)}</span>
        </div>
        <div className="row">
          <span className="muted">Authentication</span>
          <span className="figure">
            {health.auth_required ? 'token, held in an HttpOnly cookie' : 'off (loopback)'}
          </span>
        </div>
        <div className="row">
          <span className="muted">Project</span>
          <span className="figure">{project?.id ?? '—'}</span>
        </div>
        <div className="row">
          <span className="muted">Branch</span>
          <span className="figure">{project?.branch ?? '—'}</span>
        </div>
        <div className="row">
          <span className="muted">Semantic index</span>
          <span className={`figure dot ${project?.indexd_running ? 'is-proven' : 'is-asserted'}`}>
            {project?.indexd_running ? 'indexd running' : 'indexd not running'}
          </span>
        </div>
        <div className="row">
          <span className="muted">Event stream</span>
          <span className="figure">{link}</span>
        </div>
        <div className="row">
          {/* Capped at the last 50 — this is a liveness indicator, not a log,
              and a row labelled "seen" would be claiming a total it discards. */}
          <span className="muted">Recent run transitions</span>
          <span className="figure">{recent.length}</span>
        </div>
      </div>

      {project && (
        <p className="view-intro" style={{ marginTop: '1.5rem' }}>
          <span className="eyebrow">Root</span>
          <br />
          <span className="figure">{project.root}</span>
        </p>
      )}
    </div>
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
