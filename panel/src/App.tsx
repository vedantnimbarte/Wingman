import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, type BoardData, type Health, type Project } from './api'
import { Board, money } from './Board'
import { navigate, segments, useRoute } from './router'
import { Runs } from './Runs'
import { Config } from './Config'
import { Sessions } from './Sessions'
import { Insights } from './Insights'
import { EventsProvider, message, useEvents, useProjects, useSession } from './state'
import { nextTheme, useTheme, type Theme } from './theme'
import { Empty, Failed, Icon, Loading, PageHead, Pill, type IconName } from './ui'

const SECTIONS = [
  { path: '/', label: 'Overview', icon: 'overview' },
  { path: '/board', label: 'Board', icon: 'board' },
  { path: '/runs', label: 'Runs', icon: 'runs' },
  { path: '/sessions', label: 'Sessions', icon: 'sessions' },
  { path: '/insights', label: 'Insights', icon: 'insights' },
] as const satisfies readonly { path: string; label: string; icon: IconName }[]

const SETTINGS = { path: '/config', label: 'Config', icon: 'config' } as const

export function App() {
  const { session, probe } = useSession()

  switch (session.kind) {
    case 'loading':
      return <Loading what="the panel" />
    case 'unreachable':
      return (
        <div className="view">
          <Failed
            title="No answer from the daemon"
            detail={session.detail}
            action={{ label: 'Reconnect', onClick: () => void probe() }}
          />
        </div>
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

/** The wing. One mark, used on the rail and on the sign-in card. */
function Mark() {
  return (
    <span className="brand-mark" aria-hidden="true">
      <svg width="14" height="14" viewBox="0 0 20 20" fill="currentColor">
        <path d="M10 2.5 17.5 17 10 13.2 2.5 17Z" />
      </svg>
    </span>
  )
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
        <div className="signin-brand">
          <Mark />
          wingman
        </div>
        <h1>Sign in to the panel</h1>
        <p className="signin-sub">
          This daemon requires a token. It is stored in a cookie this page cannot read.
        </p>

        <div>
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
        </div>

        {error && (
          <p className="is-failed dot figure" role="alert">
            {error}
          </p>
        )}

        <button
          type="submit"
          className="button button-primary"
          disabled={busy || token.trim() === ''}
        >
          {busy ? 'Signing in…' : 'Sign in'}
        </button>

        <p className="signin-foot">
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
  const { theme, setTheme } = useTheme()
  const [palette, setPalette] = useState(false)
  const [tight, setTight] = useState(() => window.localStorage.getItem('wingman.rail') === 'tight')
  const [selected, setSelected] = useState<string | null>(
    () => window.localStorage.getItem('wingman.project') ?? null,
  )

  const choose = useCallback((id: string) => {
    setSelected(id)
    window.localStorage.setItem('wingman.project', id)
  }, [])

  function collapse(next: boolean) {
    setTight(next)
    window.localStorage.setItem('wingman.rail', next ? 'tight' : 'wide')
  }

  // ⌘K / Ctrl-K from anywhere, including inside a text field — a palette you
  // have to click out of a form to reach is a palette nobody uses.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPalette((p) => !p)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const active = projects?.find((p) => p.id === selected) ?? projects?.[0] ?? null
  const current = `/${segments(path)[0] ?? ''}`

  return (
    <div className={`shell${tight ? ' shell-tight' : ''}`}>
      <div className="shell-brand">
        <Mark />
        {!tight && 'wingman'}
      </div>

      <header className="shell-header">
        <button type="button" className="omni" onClick={() => setPalette(true)}>
          <Icon name="search" size={14} />
          <span className="omni-label">Search sections and projects</span>
          <kbd className="kbd">⌘K</kbd>
        </button>

        <div className="header-end">
          <ProjectPicker
            projects={projects}
            error={error}
            selected={active?.id ?? null}
            onChoose={choose}
          />
          <LinkState />
          <ThemeToggle theme={theme} onChange={setTheme} />
        </div>
      </header>

      <nav className="shell-nav" aria-label="Sections">
        {SECTIONS.map((s) => (
          <NavItem key={s.path} item={s} current={current} tight={tight} />
        ))}

        <span className="nav-spacer" />

        <div className="nav-foot">
          <NavItem item={SETTINGS} current={current} tight={tight} />
          <button
            type="button"
            className="nav-item"
            onClick={() => collapse(!tight)}
            title={tight ? 'Expand the sidebar' : 'Collapse the sidebar'}
          >
            <Icon name={tight ? 'expand' : 'collapse'} />
            <span className="nav-label">Collapse</span>
          </button>
        </div>
      </nav>

      <main className="shell-main">
        <Section path={path} health={health} project={active} />
      </main>

      {palette && (
        <Palette
          projects={projects}
          theme={theme}
          authRequired={health.auth_required}
          onProject={choose}
          onTheme={setTheme}
          onClose={() => setPalette(false)}
        />
      )}
    </div>
  )
}

function NavItem({
  item,
  current,
  tight,
}: {
  item: { path: string; label: string; icon: IconName }
  current: string
  tight: boolean
}) {
  return (
    <button
      type="button"
      className="nav-item"
      // Compared on the section root so a nested route like `/runs/{id}` still
      // marks Runs as the current section.
      aria-current={current === item.path ? 'page' : undefined}
      onClick={() => navigate(item.path)}
      title={tight ? item.label : undefined}
    >
      <Icon name={item.icon} />
      <span className="nav-label">{item.label}</span>
    </button>
  )
}

function ThemeToggle({ theme, onChange }: { theme: Theme; onChange: (t: Theme) => void }) {
  const next = nextTheme(theme)
  return (
    <button
      type="button"
      className="button button-quiet button-icon"
      onClick={() => onChange(next)}
      title={`Theme: ${theme}. Switch to ${next}.`}
      aria-label={`Theme: ${theme}. Switch to ${next}.`}
    >
      <Icon name={theme === 'dark' ? 'moon' : 'sun'} />
    </button>
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
  if (!projects) return <span className="faint figure">loading projects…</span>
  if (projects.length === 0) {
    return (
      <span className="faint figure" title="Add one under [[serve.projects]] in the global config">
        no projects
      </span>
    )
  }

  return (
    <select
      className="select"
      aria-label="Project"
      value={selected ?? ''}
      onChange={(e) => onChoose(e.target.value)}
    >
      {projects.map((p) => (
        <option key={p.id} value={p.id}>
          {p.id}
        </option>
      ))}
    </select>
  )
}

/** Live-stream state. The one place the shell reports on itself. */
function LinkState() {
  const { link } = useEvents()
  const [cls, glyph, text] =
    link === 'live'
      ? ['is-proven', '●', 'live']
      : link === 'connecting'
        ? ['is-asserted', '◐', 'connecting']
        : ['is-failed', '✕', 'reconnecting']

  return (
    <span title="Event stream from /v1/events">
      <Pill status={cls} glyph={glyph}>
        {text}
      </Pill>
    </span>
  )
}

/* ── Command palette ───────────────────────────────────────────────────── */

type Command = { id: string; label: string; hint: string; run: () => void }

/**
 * Everything the shell can do, in one list you can type at.
 *
 * Deliberately only the shell's own verbs — navigation, the project scope, the
 * theme, signing out. A palette that also dispatched cards and approved plans
 * would be a second place those decisions are made, and the screens that own
 * them show what they are acting on. This one never acts on something you
 * cannot see.
 */
function Palette({
  projects,
  theme,
  authRequired,
  onProject,
  onTheme,
  onClose,
}: {
  projects: Project[] | null
  theme: Theme
  authRequired: boolean
  onProject: (id: string) => void
  onTheme: (t: Theme) => void
  onClose: () => void
}) {
  const [query, setQuery] = useState('')
  const [cursor, setCursor] = useState(0)
  const listRef = useRef<HTMLUListElement | null>(null)

  const commands = useMemo<Command[]>(() => {
    const go = [...SECTIONS, SETTINGS].map((s) => ({
      id: `go:${s.path}`,
      label: `Go to ${s.label}`,
      hint: s.path,
      run: () => navigate(s.path),
    }))
    const scope = (projects ?? []).map((p) => ({
      id: `project:${p.id}`,
      label: `Switch to ${p.id}`,
      hint: 'project',
      run: () => onProject(p.id),
    }))
    const themes: Command[] = (['light', 'dark', 'system'] as const)
      .filter((t) => t !== theme)
      .map((t) => ({
        id: `theme:${t}`,
        label: `Use the ${t} theme`,
        hint: 'appearance',
        run: () => onTheme(t),
      }))
    const account: Command[] = authRequired
      ? [
          {
            id: 'signout',
            label: 'Sign out of the panel',
            hint: 'clears the cookie',
            run: () => void api.signOut().then(() => window.location.reload()),
          },
        ]
      : []
    return [...go, ...scope, ...themes, ...account]
  }, [projects, theme, authRequired, onProject, onTheme])

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return commands
    return commands.filter((c) => `${c.label} ${c.hint}`.toLowerCase().includes(q))
  }, [commands, query])

  // A filtered list whose cursor still points at index 7 runs the wrong
  // command on Enter.
  const at = Math.min(cursor, Math.max(0, matches.length - 1))

  useEffect(() => {
    listRef.current?.children[at]?.scrollIntoView({ block: 'nearest' })
  }, [at])

  function onKey(e: React.KeyboardEvent) {
    if (e.key === 'Escape') return onClose()
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      return setCursor((c) => Math.min(c + 1, matches.length - 1))
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault()
      return setCursor((c) => Math.max(c - 1, 0))
    }
    if (e.key === 'Enter') {
      e.preventDefault()
      const chosen = matches[at]
      if (chosen) {
        chosen.run()
        onClose()
      }
    }
  }

  return (
    <div
      className="palette-backdrop"
      role="presentation"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div className="palette" role="dialog" aria-label="Command palette">
        <input
          className="input palette-input"
          autoFocus
          spellCheck={false}
          placeholder="Where to?"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value)
            setCursor(0)
          }}
          onKeyDown={onKey}
        />
        {matches.length === 0 ? (
          <p className="palette-empty">Nothing matches “{query.trim()}”.</p>
        ) : (
          <ul className="palette-list" ref={listRef} role="listbox">
            {matches.map((c, i) => (
              <li key={c.id} role="none">
                <button
                  type="button"
                  role="option"
                  aria-selected={i === at}
                  className="palette-item"
                  onMouseEnter={() => setCursor(i)}
                  onClick={() => {
                    c.run()
                    onClose()
                  }}
                >
                  {c.label}
                  <span className="figure">{c.hint}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
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
      return <Sessions project={project?.id ?? null} id={rest[0] ?? null} />
    case '/config':
      return <Config />
    case '/insights':
      return <Insights project={project?.id ?? null} />
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

/* ── Overview ──────────────────────────────────────────────────────────── */

function Overview({ health, project }: { health: Health; project: Project | null }) {
  const { link } = useEvents()
  const [board, setBoard] = useState<BoardData | null>(null)

  // The board is what makes this a landing page rather than a health check.
  // It is furniture, though — a failure here leaves the tiles reading "—"
  // rather than replacing the page with an error about a screen you are not on.
  useEffect(() => {
    let alive = true
    api
      .board()
      .then((b) => alive && setBoard(b))
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [])

  const mine = board?.cards.filter((c) => !project || c.project === project.id) ?? []
  const open = mine.filter((c) => c.column !== 'done')
  const spend = mine.reduce((sum, c) => sum + (c.rollup?.usd ?? 0), 0)
  const running = mine.filter((c) => c.rollup?.status === 'running').length

  return (
    <div className="view">
      <PageHead
        eyebrow="Overview"
        title={project ? project.id : 'Wingman'}
        intro={
          project
            ? 'What this repo has running, what it has spent, and whether the daemon behind it is answering.'
            : 'No project is selected. Add one under [[serve.projects]] in the global config, then pick it in the header.'
        }
        actions={
          <>
            <button type="button" className="button" onClick={() => navigate('/runs')}>
              View runs
            </button>
            <button
              type="button"
              className="button button-primary"
              onClick={() => navigate('/board')}
            >
              Open board
            </button>
          </>
        }
      />

      <div className="tiles">
        <div className="tile">
          <span className="eyebrow">Open cards</span>
          <span className="tile-value">{board ? open.length : '—'}</span>
          <span className="tile-note">
            {board ? `${mine.length} in total` : 'reading the board…'}
          </span>
        </div>
        <div className="tile">
          <span className="eyebrow">Spend</span>
          <span className="tile-value">{board ? money(spend) : '—'}</span>
          <span className="tile-note">across every run filed to a card</span>
        </div>
        <div className="tile">
          <span className="eyebrow">Running now</span>
          <span className="tile-value">{board ? running : '—'}</span>
          <span className="tile-note">
            {running === 1 ? '1 card has a live run' : 'cards with a live run'}
          </span>
        </div>
        <div className="tile">
          <span className="eyebrow">Semantic index</span>
          <span className="tile-value">{project?.indexd_running ? 'On' : 'Off'}</span>
          <span className="tile-note">
            {project?.indexd_running ? 'indexd is serving queries' : 'indexd is not running'}
          </span>
        </div>
      </div>

      <h2 className="section-head">Daemon</h2>
      <div className="rows">
        <Row label="Status">
          <Pill status="is-proven" glyph="●">
            reachable
          </Pill>
        </Row>
        <Row label="Version">
          <span className="figure">{health.version}</span>
        </Row>
        <Row label="Uptime">
          <span className="figure">{formatUptime(health.uptime_secs)}</span>
        </Row>
        <Row label="Authentication" wrap>
          <span className="figure">
            {health.auth_required ? 'token, in an HttpOnly cookie' : 'off (loopback)'}
          </span>
        </Row>
        <Row label="Event stream">
          <span className="figure">{link}</span>
        </Row>
      </div>

      {project ? (
        <>
          <h2 className="section-head">Repository</h2>
          <div className="rows">
            <Row label="Branch" wrap>
              <span className="figure">{project.branch ?? '—'}</span>
            </Row>
            <Row label="Root" wrap>
              <span className="figure">{project.root}</span>
            </Row>
          </div>
        </>
      ) : (
        <>
          <h2 className="section-head">Repository</h2>
          <Empty title="No repository is in scope">
            Every screen except Config and the board is per-repo. Add one under{' '}
            <code>[[serve.projects]]</code> in the global config and restart the daemon.
          </Empty>
        </>
      )}
    </div>
  )
}

/** `wrap` is for a value with no natural width — a filesystem path, an id.
    It stays on the ledger axis but is allowed a second line. */
function Row({
  label,
  wrap,
  children,
}: {
  label: string
  wrap?: boolean
  children: React.ReactNode
}) {
  return (
    <div className={`row${wrap ? ' row-wrap' : ''}`}>
      <span className="muted">{label}</span>
      {children}
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
