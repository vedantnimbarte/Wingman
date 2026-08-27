import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useDialog, useHotkey, useLeader } from './a11y'
import { api, type BoardData, type Health, type Project } from './api'
import { Board, money, scope } from './Board'
import { Changes } from './Changes'
import { navigate, segments, useRoute } from './router'
import { Runs } from './Runs'
import { Config } from './Config'
import { ago, Sessions } from './Sessions'
import { Insights } from './Insights'
import { Output } from './output'
import {
  EventsProvider,
  message,
  notificationsAvailable,
  notificationsOn,
  toggleNotifications,
  useEvents,
  useProjects,
  useSession,
} from './state'
import { nextTheme, useTheme, type Theme } from './theme'
import { Empty, Failed, Icon, Loading, Note, PageHead, Pill, type IconName } from './ui'

const SECTIONS = [
  { path: '/', label: 'Overview', icon: 'overview', key: 'o' },
  { path: '/board', label: 'Board', icon: 'board', key: 'b' },
  { path: '/runs', label: 'Runs', icon: 'runs', key: 'r' },
  { path: '/sessions', label: 'Sessions', icon: 'sessions', key: 's' },
  { path: '/changes', label: 'Changes', icon: 'changes', key: 'd' },
  { path: '/insights', label: 'Insights', icon: 'insights', key: 'i' },
] as const satisfies readonly { path: string; label: string; icon: IconName; key: string }[]

const SETTINGS = { path: '/config', label: 'Config', icon: 'config', key: 'c' } as const

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
  const { link } = useEvents()
  const [palette, setPalette] = useState(false)
  const [keys, setKeys] = useState(false)
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

  // `g` then a section letter, and `/` for the palette. Both are what a
  // terminal-native reader tries before reaching for the mouse, and both are
  // suppressed while typing — see `a11y.typing`.
  useLeader(
    useMemo(
      () =>
        Object.fromEntries(
          [...SECTIONS, SETTINGS].map((s) => [s.key, () => navigate(s.path)] as const),
        ),
      [],
    ),
  )
  // Neither opens on top of the other. Both are `role="dialog"` with a focus
  // trap, and two of those stacked means the Escape that closes one leaves the
  // other holding focus behind a backdrop that is no longer above anything.
  useHotkey('/', () => !keys && setPalette(true))
  useHotkey('?', () => !palette && setKeys(true))

  const active = projects?.find((p) => p.id === selected) ?? projects?.[0] ?? null
  const current = `/${segments(path)[0] ?? ''}`
  const section = [...SECTIONS, SETTINGS].find((s) => s.path === current)

  // Three tabs open on three runs all said "Wingman". The section, the repo,
  // and — for a nested route — the id, because that is what tells two run tabs
  // apart in a tab strip that shows twelve characters.
  useEffect(() => {
    const rest = segments(path)[1]
    document.title = [rest, section?.label, active?.id, 'Wingman'].filter(Boolean).join(' · ')
  }, [path, section, active])

  return (
    <div className={`shell${tight ? ' shell-tight' : ''}`}>
      {/* First tabbable thing on the page, visible only once focused: without
          it a keyboard user walks the whole rail on every navigation. */}
      <a className="skip" href="#main">
        Skip to content
      </a>

      <div className="shell-brand">
        <Mark />
        {!tight && 'wingman'}
      </div>

      <header className="shell-header">
        <button type="button" className="omni" onClick={() => setPalette(true)}>
          <Icon name="search" size={14} />
          <span className="omni-label">Search sections, runs, cards and sessions</span>
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
          <NotifyToggle />
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

      <main className="shell-main" id="main" tabIndex={-1}>
        {/* The stream is the only thing every screen depends on and the only
            thing that can fail without any screen saying so. A pill in the
            header reports it; this says what it means for what is on screen. */}
        {link === 'down' && (
          <div className="banner" role="status">
            <span className="dot is-failed" aria-hidden="true" />
            <span>
              The event stream dropped. Everything below is as it was when the connection went —
              the browser is retrying.
            </span>
            <button
              type="button"
              className="button button-sm"
              onClick={() => window.location.reload()}
            >
              Reload
            </button>
          </div>
        )}

        {/* One polite region for the whole app: route changes are silent
            otherwise, because nothing about a client-side navigation is a page
            load as far as a screen reader is concerned. */}
        <p className="sr-only" aria-live="polite">
          {section ? `${section.label}${active ? `, ${active.id}` : ''}` : 'Page not found'}
        </p>

        <Section path={path} health={health} project={active} />
      </main>

      {palette && (
        <Palette
          projects={projects}
          project={active}
          theme={theme}
          authRequired={health.auth_required}
          onProject={choose}
          onTheme={setTheme}
          onClose={() => setPalette(false)}
        />
      )}

      {keys && <Shortcuts onClose={() => setKeys(false)} />}
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

/**
 * Desktop notifications for a run that has stopped and is waiting.
 *
 * Hidden entirely where the browser has already refused permission — a control
 * whose only outcome is "no" is worse than no control.
 */
function NotifyToggle() {
  const [on, setOn] = useState(notificationsOn)
  if (!notificationsAvailable() && !on) return null

  return (
    <button
      type="button"
      className="button button-quiet button-icon"
      aria-pressed={on}
      onClick={() => void toggleNotifications().then(setOn)}
      title={
        on
          ? 'Notifications on: a run waiting for approval will interrupt you. Click to stop.'
          : 'Notify me when a run stops for plan approval or fails'
      }
      aria-label={on ? 'Turn notifications off' : 'Turn notifications on'}
    >
      <Icon name={on ? 'bell' : 'bell-off'} />
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

/* ── Keyboard reference ────────────────────────────────────────────────── */

const KEYS: [string, string][] = [
  ['⌘K / Ctrl-K', 'Open the palette — sections, runs, cards, sessions'],
  ['/', 'Open the palette'],
  ['g then o b r s d i c', 'Go to Overview, Board, Runs, Sessions, Changes, Insights, Config'],
  ['↑ ↓ Enter', 'Move and choose in the palette'],
  ['Esc', 'Close the palette, a drawer, or this'],
  ['Enter / Shift+Enter', 'Send a turn / newline, in a conversation'],
  ['?', 'This list'],
]

function Shortcuts({ onClose }: { onClose: () => void }) {
  const ref = useDialog<HTMLDivElement>(onClose)
  return (
    <div className="palette-backdrop" role="presentation" onMouseDown={onClose}>
      <div
        className="palette keys"
        ref={ref}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="Keyboard shortcuts"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 className="section-head">Keyboard</h2>
        <div className="rows">
          {KEYS.map(([k, what]) => (
            <div key={k} className="row">
              <span className="muted">{what}</span>
              <kbd className="kbd">{k}</kbd>
            </div>
          ))}
        </div>
        <div className="actions">
          <button type="button" className="button button-quiet" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  )
}

/* ── Command palette ───────────────────────────────────────────────────── */

type Command = { id: string; label: string; hint: string; run: () => void }

/**
 * Everything the shell can do, and everything the daemon can be asked to show
 * you, in one list you can type at.
 *
 * The original rule holds and is the reason this is safe to widen: the palette
 * carries **no verbs that act on data**. It navigates, it scopes, it themes,
 * it signs out. Search added runs, cards and sessions as *destinations* — the
 * screen that owns a decision still shows what the decision acts on before
 * anyone can make it. What the palette must never grow is a "dispatch" or an
 * "approve" that fires against something you cannot see.
 *
 * Objects are fetched only once there is a query, and only once per opening.
 * Loading three lists to show six navigation entries would make ⌘K the
 * slowest thing in the panel.
 */
function Palette({
  projects,
  project,
  theme,
  authRequired,
  onProject,
  onTheme,
  onClose,
}: {
  projects: Project[] | null
  project: Project | null
  theme: Theme
  authRequired: boolean
  onProject: (id: string) => void
  onTheme: (t: Theme) => void
  onClose: () => void
}) {
  const [query, setQuery] = useState('')
  const [cursor, setCursor] = useState(0)
  const [found, setFound] = useState<Command[]>([])
  const listRef = useRef<HTMLUListElement | null>(null)
  const ref = useDialog<HTMLDivElement>(onClose)

  const commands = useMemo<Command[]>(() => {
    const go = [...SECTIONS, SETTINGS].map((s) => ({
      id: `go:${s.path}`,
      label: `Go to ${s.label}`,
      hint: s.path,
      run: () => navigate(s.path),
    }))
    const scopes = (projects ?? []).map((p) => ({
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
    return [...go, ...scopes, ...themes, ...account]
  }, [projects, theme, authRequired, onProject, onTheme])

  // One fetch per opening, on the first keystroke. A search that re-queried on
  // every character would put three requests behind every letter for a list
  // that does not change while the palette is open.
  const loaded = useRef(false)
  useEffect(() => {
    if (!query.trim() || loaded.current) return
    loaded.current = true
    let live = true

    void (async () => {
      const out: Command[] = []
      try {
        const board = await api.board()
        for (const c of board.cards) {
          out.push({
            id: `card:${c.id}`,
            label: c.title,
            hint: `card · ${c.project_name}`,
            run: () => navigate('/board'),
          })
        }
      } catch {
        /* Search degrades to what did load. It is a shortcut, not a screen. */
      }
      if (project) {
        try {
          for (const r of await api.runs(project.id)) {
            out.push({
              id: `run:${r.run_id}`,
              label: r.goal,
              hint: `run · ${r.run_id}`,
              run: () => navigate(`/runs/${r.run_id}`),
            })
          }
        } catch {
          /* as above */
        }
        try {
          for (const s of await api.sessions(project.id)) {
            out.push({
              id: `session:${s.session_id}`,
              label: s.first_prompt ?? s.session_id,
              hint: `session · ${s.session_id}`,
              run: () => navigate(`/sessions/${s.session_id}`),
            })
          }
        } catch {
          /* as above */
        }
      }
      if (live) setFound(out)
    })()

    return () => {
      live = false
    }
  }, [query, project])

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return commands
    const all = [...commands, ...found]
    return all.filter((c) => `${c.label} ${c.hint}`.toLowerCase().includes(q)).slice(0, 40)
  }, [commands, found, query])

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

  const activeId = matches[at] ? `palette-${matches[at].id}` : undefined

  return (
    <div
      className="palette-backdrop"
      role="presentation"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        className="palette"
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
      >
        <input
          className="input palette-input"
          autoFocus
          spellCheck={false}
          placeholder="Where to?"
          // A listbox the arrow keys drive from an input is a combobox, and
          // without `activedescendant` a screen reader announces nothing as the
          // highlight moves — the list is visibly working and silently not.
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={activeId}
          aria-autocomplete="list"
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
          <ul className="palette-list" id="palette-list" ref={listRef} role="listbox">
            {matches.map((c, i) => (
              <li key={c.id} role="none">
                <button
                  type="button"
                  id={`palette-${c.id}`}
                  role="option"
                  aria-selected={i === at}
                  className="palette-item"
                  onMouseEnter={() => setCursor(i)}
                  onClick={() => {
                    c.run()
                    onClose()
                  }}
                >
                  <span className="truncate">{c.label}</span>
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
    case '/changes':
      return <Changes project={project?.id ?? null} />
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
  const { link, tick } = useEvents()
  const [board, setBoard] = useState<BoardData | null>(null)

  // The board is what makes this a landing page rather than a health check.
  // It is furniture, though — a failure here leaves the tiles reading "—"
  // rather than replacing the page with an error about a screen you are not on.
  //
  // Refetched on the event counter for the same reason every other screen is:
  // a landing page that was true when the tab opened and has been wrong ever
  // since is worse than one that says it is loading.
  useEffect(() => {
    let alive = true
    api
      .board()
      .then((b) => alive && setBoard(b))
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [tick])

  // `scope` is the board's own resolution, shared rather than reimplemented.
  // Filtering on `project.id` directly was wrong in exactly the way Board.tsx
  // documents: `[[serve.projects]].id` and the registry slug are different
  // namespaces, so when they differ this page confidently reported no cards
  // and no spend — a false zero on the first screen anyone sees.
  const mine = board ? scope(board, project?.id ?? null) : []
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
          {/* Whether indexd is *running* is not the question anyone has. How
              old the index is, is: a live daemon over a week-old index answers
              questions about code that no longer exists. */}
          <span className="tile-note">{indexNote(project)}</span>
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

          <Maintenance project={project.id} />
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

/**
 * How stale the semantic index is, said plainly.
 *
 * Coarse, not exact: `formatUptime` is right for a daemon that has been up for
 * four minutes and wrong for an index last built nine days ago, which it
 * renders as "210h 54m 57s". Nobody counts in hours past the second day.
 */
function indexNote(project: Project | null): string {
  if (!project) return 'no project selected'
  if (project.index_age_secs == null) return 'this repo has never been indexed'
  // `ago` works from a Unix timestamp; an age is the same thing measured from
  // now, so it is turned back into one rather than growing a second formatter.
  const age = ago(Math.floor(Date.now() / 1000) - project.index_age_secs)
  return project.indexd_running ? `serving; indexed ${age}` : `not running; indexed ${age}`
}

/* ── Maintenance ───────────────────────────────────────────────────────── */

/**
 * The write routes with no screen of their own.
 *
 * Six things the CLI can do to a repo that the panel could only watch:
 * checkpoint, rewind, reindex, memory sync, trust, and the scheduler. They are
 * one shape — POST, no body, print what happened — so they are one list rather
 * than six features.
 *
 * `rewind` is the only one that can lose work, so it is the only one that asks
 * first, and with no argument it prints the timeline rather than reverting
 * anything: the CLI's own default, kept, because a button labelled "Rewind"
 * that silently reverted the last edit would be the most dangerous control in
 * the panel.
 */
const ACTIONS: { tail: string; label: string; about: string; confirm?: string }[] = [
  {
    tail: 'checkpoints',
    label: 'Checkpoint',
    about: 'stash the working tree as a recoverable point',
  },
  {
    tail: 'rewind',
    label: 'Rewind timeline',
    about: 'print the mutating edits that could be reverted — reverts nothing',
  },
  { tail: 'index/reindex', label: 'Reindex', about: 'rebuild the semantic index' },
  { tail: 'memory/sync', label: 'Sync memory', about: 'rebuild MEMORY.md from the memory files' },
  {
    tail: 'trust',
    label: 'Trust this config',
    about: "accept the repo's .wingman/config.toml as it stands",
    confirm: "Trust this repo's config as it currently stands?",
  },
  { tail: 'schedule/run', label: 'Run schedule', about: 'run any scheduled prompts now due' },
]

function Maintenance({ project }: { project: string }) {
  const [busy, setBusy] = useState<string | null>(null)
  const [result, setResult] = useState<{ tail: string; value: unknown } | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function run(a: (typeof ACTIONS)[number]) {
    if (a.confirm && !window.confirm(a.confirm)) return
    setBusy(a.tail)
    setError(null)
    setResult(null)
    try {
      setResult({ tail: a.tail, value: await api.action(project, a.tail) })
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(null)
    }
  }

  return (
    <>
      <h2 className="section-head">Maintenance</h2>
      <p className="section-intro">
        The same subcommands a terminal runs, in this repo. Each prints what it did — nothing here
        is applied optimistically.
      </p>
      <div className="rows">
        {ACTIONS.map((a) => (
          <div key={a.tail} className="row">
            <span className="task-meta-block">
              <span>{a.label}</span>
              <span className="muted">{a.about}</span>
            </span>
            <button
              type="button"
              className="button button-sm"
              disabled={busy !== null}
              onClick={() => void run(a)}
            >
              {busy === a.tail ? 'Running…' : 'Run'}
            </button>
          </div>
        ))}
      </div>

      {error && (
        <Note tone="is-failed" role="alert">
          {error}
        </Note>
      )}
      {result && (
        <>
          <h3 className="section-head figure">{result.tail}</h3>
          <Output value={result.value} />
        </>
      )}
    </>
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
