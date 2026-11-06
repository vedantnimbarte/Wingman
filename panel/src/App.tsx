import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useDialog, useHotkey, useLeader } from './a11y'
import { api, type BoardData, type Card, type Health, type Project, type RunStatus } from './api'
import { Board, money, scope } from './Board'
import { Changes } from './Changes'
import { navigate, segments, useRoute } from './router'
import { runClass, runGlyph, runLabel, Runs } from './Runs'
import { Config } from './Config'
import { ago, Sessions } from './Sessions'
import { Insights } from './Insights'
import { Output } from './output'
import { Picker } from './Picker'
import { InboxButton, NotificationCard, NotificationsProvider, useInbox } from './Notifications'
import {
  EventsProvider,
  message,
  useEvents,
  useProjects,
  useSession,
} from './state'
import { nextTheme, useTheme, type Theme } from './theme'
import { Failed, Icon, Loading, Note, PageHead, Pill, type IconName } from './ui'

const SECTIONS = [
  { path: '/', label: 'Overview', icon: 'overview', key: 'o', group: 'Workspace' },
  { path: '/board', label: 'Board', icon: 'board', key: 'b', group: 'Workspace' },
  { path: '/runs', label: 'Runs', icon: 'runs', key: 'r', group: 'Workspace' },
  { path: '/sessions', label: 'Sessions', icon: 'sessions', key: 's', group: 'Workspace' },
  { path: '/changes', label: 'Changes', icon: 'changes', key: 'd', group: 'Review' },
  { path: '/insights', label: 'Insights', icon: 'insights', key: 'i', group: 'Review' },
] as const satisfies readonly {
  path: string
  label: string
  icon: IconName
  key: string
  group: string
}[]

const SETTINGS = { path: '/config', label: 'Settings', icon: 'config', key: 'c' } as const

export function App() {
  const { session, probe } = useSession()

  switch (session.kind) {
    case 'loading':
      return <Loading what="the panel" />
    case 'unreachable':
      return (
        <div className="view">
          <Failed
            title="Wingman isn't answering"
            detail={`${session.detail} Start it with \`wingman serve\`, then reconnect.`}
            action={{ label: 'Reconnect', onClick: () => void probe() }}
          />
        </div>
      )
    case 'needs-token':
      return <SignIn onDone={() => void probe()} />
    case 'ready':
      return (
        <EventsProvider>
          <NotificationsProvider>
            <Shell health={session.health} />
          </NotificationsProvider>
        </EventsProvider>
      )
  }
}

/** The wing. One mark, used on the rail and on the sign-in card. */
function Mark() {
  return (
    <span className="brand-mark" aria-hidden="true">
      <svg width="15" height="15" viewBox="0 0 20 20" fill="currentColor">
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
  // A pairing code is the phone path: short-lived, and it saves carrying the
  // 43-character token across by hand. Same field, same cookie at the end.
  const [pairing, setPairing] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await (pairing ? api.pair(token) : api.signIn(token))
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
          Wingman
        </div>
        <h1>Sign in</h1>
        <p className="signin-sub">
          This Wingman is protected. Enter its access token, or a pairing code from your terminal.
        </p>

        <div>
          <label htmlFor="token">{pairing ? 'Pairing code' : 'Access token'}</label>
          <input
            id="token"
            className="input"
            type="password"
            autoComplete={pairing ? 'one-time-code' : 'current-password'}
            autoCapitalize="off"
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
          {busy ? 'Signing in…' : pairing ? 'Pair this device' : 'Sign in'}
        </button>

        <button
          type="button"
          className="button button-quiet"
          onClick={() => {
            setPairing((p) => !p)
            setToken('')
            setError(null)
          }}
        >
          {pairing ? 'Use an access token instead' : 'I have a pairing code'}
        </button>

        <p className="signin-foot">
          {pairing ? (
            <>
              Run <code>wingman serve --pair</code> and type the code it prints. A code works once
              and expires after 10 minutes.
            </>
          ) : (
            <>
              Create a token with <code>wingman serve --init-token</code>. It is shown only once,
              so copy it somewhere safe.
            </>
          )}
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
  const { link, tick } = useEvents()
  const [palette, setPalette] = useState(false)
  const [keys, setKeys] = useState(false)
  const [tight, setTight] = useState(() => window.localStorage.getItem('wingman.rail') === 'tight')
  const [selected, setSelected] = useState<string | null>(
    () => window.localStorage.getItem('wingman.project') ?? null,
  )
  const [board, setBoard] = useState<BoardData | null>(null)

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

  // The nav's counts come from the board, refetched on the event counter like
  // every other screen. A failure leaves the nav without counts, nothing more.
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
  const nested = segments(path)[1]

  // Three tabs open on three runs all said "Wingman". The section, the repo,
  // and — for a nested route — the id, because that is what tells two run tabs
  // apart in a tab strip that shows twelve characters.
  useEffect(() => {
    document.title = [nested, section?.label, active?.id, 'Wingman'].filter(Boolean).join(' · ')
  }, [nested, section, active])

  const mine = board ? scope(board, active?.id ?? null) : []
  const badges: Record<string, { count?: number; lamp?: string }> = {
    '/board': { count: mine.filter((c) => c.column !== 'done').length || undefined },
    '/runs': (() => {
      const live = mine.filter((c) => c.rollup && LIVE.has(c.rollup.status))
      const waiting = live.some((c) => c.rollup?.status === 'awaiting_approval')
      return {
        count: live.length || undefined,
        lamp: waiting ? 'is-asserted' : live.length ? 'is-proven' : undefined,
      }
    })(),
  }

  const groups = [...new Set(SECTIONS.map((s) => s.group))]

  return (
    <div className={`shell${tight ? ' shell-tight' : ''}`}>
      {/* First tabbable thing on the page, visible only once focused: without
          it a keyboard user walks the whole rail on every navigation. */}
      <a className="skip" href="#main">
        Skip to content
      </a>

      <aside className="rail">
        <div className="rail-brand">
          <Mark />
          {!tight && 'Wingman'}
          <button
            type="button"
            className="button button-quiet button-sm button-icon rail-toggle"
            onClick={() => collapse(!tight)}
            title={tight ? 'Expand the sidebar' : 'Collapse the sidebar'}
            aria-label={tight ? 'Expand the sidebar' : 'Collapse the sidebar'}
          >
            <Icon name={tight ? 'expand' : 'collapse'} size={14} />
          </button>
        </div>

        <ProjectPicker
          projects={projects}
          error={error}
          selected={active}
          onChoose={choose}
        />

        <nav className="rail-nav" aria-label="Sections">
          {groups.map((g) => (
            <div key={g} className="rail-nav">
              <span className="rail-group">{g}</span>
              {SECTIONS.filter((s) => s.group === g).map((s) => (
                <NavItem
                  key={s.path}
                  item={s}
                  current={current}
                  tight={tight}
                  badge={badges[s.path]}
                />
              ))}
            </div>
          ))}
        </nav>

        <span className="nav-spacer" />

        <div className="rail-foot">
          <NavItem item={SETTINGS} current={current} tight={tight} />
          <LinkState version={health.version} />
        </div>
      </aside>

      <div className="canvas">
        <header className="topbar">
          <nav className="crumbs truncate" aria-label="Breadcrumb">
            {nested ? (
              <>
                <button type="button" onClick={() => navigate(current)}>
                  {section?.label ?? 'Wingman'}
                </button>
                <span className="crumb-sep" aria-hidden="true">
                  /
                </span>
                <span className="truncate mono" aria-current="page">
                  {nested}
                </span>
              </>
            ) : (
              <span aria-current="page">{section?.label ?? 'Not found'}</span>
            )}
          </nav>

          <div className="header-end">
            <button type="button" className="omni" onClick={() => setPalette(true)}>
              <Icon name="search" size={14} />
              <span className="omni-label">Search or jump to…</span>
              <kbd className="kbd">⌘K</kbd>
            </button>
            <InboxButton />
            <ThemeToggle theme={theme} onChange={setTheme} />
          </div>
        </header>

        <main className="shell-main" id="main" tabIndex={-1}>
          {/* The stream is the only thing every screen depends on and the only
              thing that can fail without any screen saying so. The rail reports
              it; this says what it means for what is on screen. */}
          {link === 'down' && (
            <div className="banner" role="status">
              <span className="dot is-failed" aria-hidden="true" />
              <span>
                Lost the live connection to Wingman. What you see may be out of date — your
                browser is reconnecting.
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
              otherwise, because nothing about a client-side navigation is a
              page load as far as a screen reader is concerned. */}
          <p className="sr-only" aria-live="polite">
            {section ? `${section.label}${active ? `, ${active.id}` : ''}` : 'Page not found'}
          </p>

          <Section path={path} health={health} project={active} board={board} />
        </main>
      </div>

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

/** Run states that are still going — or waiting on someone. */
const LIVE = new Set(['planning', 'awaiting_approval', 'running', 'merging'])

function NavItem({
  item,
  current,
  tight,
  badge,
}: {
  item: { path: string; label: string; icon: IconName }
  current: string
  tight: boolean
  badge?: { count?: number; lamp?: string }
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
      {badge?.lamp && <span className={`lamp nav-badge-lamp ${badge.lamp}`} aria-hidden="true" />}
      {badge?.count !== undefined && (
        <span className="count" aria-label={`${badge.count} open`}>
          {badge.count}
        </span>
      )}
    </button>
  )
}

function ThemeToggle({ theme, onChange }: { theme: Theme; onChange: (t: Theme) => void }) {
  const next = nextTheme(theme)
  const label = `Appearance: ${theme === 'system' ? 'match system' : theme}. Switch to ${next === 'system' ? 'match system' : next}.`
  return (
    <button
      type="button"
      className="button button-quiet button-icon"
      onClick={() => onChange(next)}
      title={label}
      aria-label={label}
    >
      <Icon name={theme === 'dark' ? 'moon' : theme === 'light' ? 'sun' : 'system'} />
    </button>
  )
}

/** The repo every per-repo screen reads, as the rail's first instrument. */
function ProjectPicker({
  projects,
  error,
  selected,
  onChoose,
}: {
  projects: Project[] | null
  error: string | null
  selected: Project | null
  onChoose: (id: string) => void
}) {
  if (error) return <p className="is-failed dot figure">{error}</p>

  const name = selected?.id ?? (projects ? 'No repositories' : 'Loading…')
  const sub = selected
    ? (selected.branch ?? 'detached HEAD')
    : projects
      ? 'Add one in Settings'
      : ''

  const card = (
    <>
      <span className="workspace-icon" aria-hidden="true">
        {name.slice(0, 1)}
      </span>
      <span className="workspace-text">
        <span className="workspace-name truncate">{name}</span>
        {sub && (
          <span className="workspace-sub truncate">
            {selected ? <Icon name="branch" size={11} /> : null} {sub}
          </span>
        )}
      </span>
    </>
  )

  if (!projects || projects.length < 2)
    return (
      <div className="workspace" title={selected?.root}>
        {card}
      </div>
    )

  return (
    <Picker
      label="Repository"
      value={selected?.id ?? ''}
      onChange={onChoose}
      className="workspace"
      title={selected?.root}
      options={projects.map((p) => ({
        value: p.id,
        label: p.id,
        description: p.root,
        detail: (
          <span className="picker-meta">
            <span
              className={`lamp ${p.indexd_running ? 'is-proven' : ''}`}
              title={p.indexd_running ? 'Code search is running' : 'Code search is off'}
            />
            {p.branch ?? 'detached'}
          </span>
        ),
      }))}
      trigger={() => (
        <>
          {card}
          <span className="workspace-chevron" aria-hidden="true">
            <Icon name="updown" size={14} />
          </span>
          <span className="sr-only">Repository: {name}. Change repository</span>
        </>
      )}
    />
  )
}

/** Live-connection state. The one place the shell reports on itself. */
function LinkState({ version }: { version: string }) {
  const { link } = useEvents()
  const [cls, text] =
    link === 'live'
      ? ['is-proven', 'Connected']
      : link === 'connecting'
        ? ['is-asserted', 'Connecting…']
        : ['is-failed', 'Reconnecting…']

  return (
    <div className="rail-status" title={`Live updates: ${text.toLowerCase()} · Wingman ${version}`}>
      <span className={`lamp ${cls}`} aria-hidden="true" />
      <span className="rail-status-text">
        {text} <span className="faint">· v{version}</span>
      </span>
    </div>
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
  board,
}: {
  path: string
  health: Health
  project: Project | null
  board: BoardData | null
}) {
  // Matched on the first segment so nested routes like `/runs/{id}` reach
  // their section rather than falling through to "No such page".
  const [head, ...rest] = segments(path)

  switch (`/${head ?? ''}`) {
    case '/':
      return <Overview health={health} project={project} board={board} />
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
            title="There's no page here"
            detail={`Nothing lives at ${path}.`}
            action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
          />
        </div>
      )
  }
}

/* ── Overview ──────────────────────────────────────────────────────────── */

/** Live runs first, then whatever needs a person, then the finished ones. */
const RUN_ORDER: Record<RunStatus, number> = {
  awaiting_approval: 0,
  running: 1,
  merging: 2,
  planning: 3,
  failed: 4,
  aborted: 5,
  done: 6,
}

function Overview({
  health,
  project,
  board,
}: {
  health: Health
  project: Project | null
  board: BoardData | null
}) {
  const { link } = useEvents()
  const { cards: inbox, busy, answer } = useInbox()

  // `scope` is the board's own resolution, shared rather than reimplemented:
  // `[[serve.projects]].id` and the registry slug are different namespaces, so
  // filtering on `project.id` directly reported a false zero whenever they
  // differed. The board itself is the shell's copy, refetched on every event.
  const mine = board ? scope(board, project?.id ?? null) : []
  const open = mine.filter((c) => c.column !== 'done')
  const spend = mine.reduce((sum, c) => sum + (c.rollup?.usd ?? 0), 0)
  const withRun = mine
    .filter((c): c is Card & { rollup: NonNullable<Card['rollup']> } => c.rollup !== null)
    .sort((a, b) => RUN_ORDER[a.rollup.status] - RUN_ORDER[b.rollup.status])
  const live = withRun.filter((c) => LIVE.has(c.rollup.status))
  const waiting = live.filter((c) => c.rollup.status === 'awaiting_approval')
  const working = live.flatMap((c) =>
    c.rollup.subrows.filter((s) => s.status === 'in_progress').map((s) => ({ s, c })),
  )
  const liveDone = live.reduce((n, c) => n + c.rollup.done, 0)
  const liveTotal = live.reduce((n, c) => n + c.rollup.total, 0)
  const liveSpend = live.reduce((n, c) => n + c.rollup.usd, 0)
  const needsYou = inbox.length

  const runsLine = live.length
    ? `${live.length} run${live.length === 1 ? ' is' : 's are'} in progress`
    : 'Nothing is running'
  const needsLine = needsYou
    ? ` and ${needsYou} item${needsYou === 1 ? ' is' : 's are'} waiting on you`
    : ''
  const summary = !project
    ? 'Pick a repository in the sidebar to see what Wingman is doing in it.'
    : !board
      ? 'Reading the board…'
      : `${runsLine}${needsLine}. ${money(spend)} spent so far.`

  return (
    <div className="view">
      <PageHead
        title={project ? project.id : 'Welcome to Wingman'}
        intro={summary}
        actions={
          <>
            <button type="button" className="button" onClick={() => navigate('/sessions')}>
              <Icon name="sessions" size={15} />
              New chat
            </button>
            <button
              type="button"
              className="button button-primary"
              onClick={() => navigate('/board')}
            >
              <Icon name="plus" size={15} />
              Add a card
            </button>
          </>
        }
      />

      <section className="deck" aria-label="Now running">
        <div className="deck-main">
          <span className={`deck-badge${working.length ? ' is-live' : ''}`}>
            <span className={`lamp ${working.length ? 'is-asserted' : ''}`} aria-hidden="true" />
            {working.length ? 'Live' : 'Idle'}
          </span>
          <h2>
            {working.length
              ? `${working.length} agent${working.length === 1 ? ' is' : 's are'} working on ${live.length} run${live.length === 1 ? '' : 's'}`
              : waiting.length
                ? `${waiting.length} plan${waiting.length === 1 ? ' is' : 's are'} waiting for your approval`
                : 'No agents are working right now'}
          </h2>
          {working.length > 0 ? (
            <ul className="deck-agents">
              {working.slice(0, 3).map(({ s, c }) => (
                <li key={`${c.id}-${s.task_id}`}>
                  <button
                    type="button"
                    onClick={() => c.run_id && navigate(`/runs/${c.run_id}`)}
                  >
                    <span className="deck-callsign">{s.agent_name ?? s.role}</span>
                    <span className="deck-task truncate">{s.title}</span>
                    {s.current_tool && (
                      <span className="deck-tool truncate mono">{s.current_tool}</span>
                    )}
                  </button>
                </li>
              ))}
              {working.length > 3 && (
                <li className="deck-more">and {working.length - 3} more</li>
              )}
            </ul>
          ) : (
            <p className="deck-empty">
              {waiting.length
                ? 'Review the plan and approve it to let the agents start.'
                : 'Add a card to the board and press Start — the agents that pick it up appear here as they work.'}
            </p>
          )}
        </div>
        <dl className="deck-stats">
          <div>
            <dt>Tasks done</dt>
            <dd>
              {liveDone}
              <span>/{liveTotal}</span>
            </dd>
            <div className="deck-meter" aria-hidden="true">
              <span style={{ width: `${liveTotal ? (liveDone / liveTotal) * 100 : 0}%` }} />
            </div>
          </div>
          <div>
            <dt>Waiting on you</dt>
            <dd>{needsYou}</dd>
          </div>
          <div>
            <dt>Spent on live runs</dt>
            <dd>{money(liveSpend)}</dd>
          </div>
        </dl>
      </section>

      <div className="tiles">
        <Tile
          icon="board"
          label="Open cards"
          value={board ? String(open.length) : '—'}
          note={board ? `${mine.length - open.length} of ${mine.length} finished` : 'Reading the board…'}
          fill={mine.length ? (mine.length - open.length) / mine.length : 0}
        />
        <Tile
          icon="runs"
          label="Runs in progress"
          value={board ? String(live.length) : '—'}
          note={
            waiting.length
              ? `${waiting.length} waiting for approval`
              : `${withRun.length} run${withRun.length === 1 ? '' : 's'} in total`
          }
        />
        <Tile
          icon="coin"
          label="Spend"
          value={board ? money(spend) : '—'}
          note="Across every run on the board"
        />
        <Tile
          icon="index"
          label="Code search"
          value={indexValue(project)}
          note={indexNote(project)}
          tone={indexTone(project)}
        />
      </div>

      <div className="overview-grid">
        <section className="panel">
          <div className="panel-head">
            <div>
              <h2>Needs you</h2>
              <p className="panel-sub">Approvals, questions from agents and failures</p>
            </div>
            {needsYou > 0 && <span className="count">{needsYou}</span>}
          </div>
          {inbox.length === 0 ? (
            <p className="panel-empty">Nothing needs you right now.</p>
          ) : (
            <div className="attention">
              {inbox.slice(0, 4).map((n) => (
                <NotificationCard key={n.id} n={n} busy={busy === n.id} onAnswer={answer} />
              ))}
            </div>
          )}
        </section>

        <section className="panel">
          <div className="panel-head">
            <div>
              <h2>Runs</h2>
              <p className="panel-sub">Work Wingman has planned and carried out</p>
            </div>
            <button type="button" className="button button-sm" onClick={() => navigate('/runs')}>
              View all
            </button>
          </div>
          {withRun.length === 0 ? (
            <p className="panel-empty">
              No runs yet. Press Start on a board card to begin one.
            </p>
          ) : (
            <ul className="run-mini">
              {withRun.slice(0, 6).map((c) => (
                <li key={c.id}>
                  <button
                    type="button"
                    onClick={() => c.run_id && navigate(`/runs/${c.run_id}`)}
                  >
                    <span className="run-mini-title truncate">{c.title}</span>
                    <Pill status={runClass(c.rollup.status)} glyph={runGlyph(c.rollup.status)}>
                      {runLabel(c.rollup.status)}
                    </Pill>
                    <span className="run-mini-meta">
                      {c.rollup.total ? `${c.rollup.done}/${c.rollup.total} tasks` : 'No plan yet'}
                      <span>{money(c.rollup.usd)}</span>
                    </span>
                    <span className={`meter ${runClass(c.rollup.status)}`} aria-hidden="true">
                      <span
                        style={
                          {
                            '--fill': `${c.rollup.total ? (c.rollup.done / c.rollup.total) * 100 : 0}%`,
                          } as React.CSSProperties
                        }
                      />
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <div className="overview-grid">
        <section className="panel">
          <div className="panel-head">
            <h2>Wingman</h2>
            <Pill status="is-proven" glyph="●">
              Running
            </Pill>
          </div>
          <div className="rows">
            <Row label="Version">
              <span className="figure">{health.version}</span>
            </Row>
            <Row label="Up for">
              <span className="figure">{formatUptime(health.uptime_secs)}</span>
            </Row>
            <Row label="Sign-in">
              <span className="figure">
                {health.auth_required ? 'Token required' : 'Not required on this machine'}
              </span>
            </Row>
            <Row label="Live updates">
              <span className="figure">
                {link === 'live' ? 'Connected' : link === 'connecting' ? 'Connecting…' : 'Reconnecting…'}
              </span>
            </Row>
          </div>
        </section>

        <section className="panel">
          <div className="panel-head">
            <h2>Repository</h2>
          </div>
          {project ? (
            <div className="rows">
              <Row label="Branch" wrap>
                <span className="figure mono">{project.branch ?? 'Detached HEAD'}</span>
              </Row>
              <Row label="Location" wrap>
                <span className="figure mono">{project.root}</span>
              </Row>
            </div>
          ) : (
            <p className="panel-empty">
              No repository is set up yet. Add one under <code>[[serve.projects]]</code> in your
              Wingman config, then restart <code>wingman serve</code>.
            </p>
          )}
        </section>
      </div>

      {project && <Maintenance project={project.id} />}
    </div>
  )
}

function Tile({
  icon,
  label,
  value,
  note,
  fill,
  tone,
}: {
  icon: IconName
  label: string
  value: string
  note: string
  fill?: number
  tone?: string
}) {
  return (
    <div className="tile">
      <span className="tile-top">
        {label}
        <span className="tile-icon" aria-hidden="true">
          <Icon name={icon} size={15} />
        </span>
      </span>
      <span className={`tile-value${tone ? ` ${tone}` : ''}`}>{value}</span>
      <span className="tile-note">{note}</span>
      {fill !== undefined && (
        <span className="meter" aria-hidden="true">
          <span style={{ '--fill': `${Math.round(fill * 100)}%` } as React.CSSProperties} />
        </span>
      )}
    </div>
  )
}

function indexValue(project: Project | null): string {
  if (!project) return '—'
  if (project.index_age_secs == null) return 'Not built'
  if (!project.indexd_running) return 'Paused'
  return project.index_age_secs > 86_400 ? 'Stale' : 'Up to date'
}

function indexTone(project: Project | null): string | undefined {
  if (!project || project.index_age_secs == null) return 'muted'
  if (!project.indexd_running || project.index_age_secs > 86_400) return 'is-asserted'
  return undefined
}

/**
 * How stale the semantic index is, said plainly.
 *
 * Coarse, not exact: `formatUptime` is right for a daemon that has been up for
 * four minutes and wrong for an index last built nine days ago, which it
 * renders as "210h 54m 57s". Nobody counts in hours past the second day.
 */
function indexNote(project: Project | null): string {
  if (!project) return 'No repository selected'
  if (project.index_age_secs == null) return 'Rebuild it below to enable code search'
  // `ago` works from a Unix timestamp; an age is the same thing measured from
  // now, so it is turned back into one rather than growing a second formatter.
  const age = ago(Math.floor(Date.now() / 1000) - project.index_age_secs)
  return project.indexd_running ? `Updated ${age}` : `Not watching for changes · built ${age}`
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
const ACTIONS: {
  tail: string
  label: string
  about: string
  icon: IconName
  confirm?: string
}[] = [
  {
    tail: 'checkpoints',
    label: 'Save a checkpoint',
    about: 'Snapshot the working tree so you can restore it later.',
    icon: 'check',
  },
  {
    tail: 'rewind',
    label: 'Show restore points',
    about: 'List the edits that could be undone. Changes nothing.',
    icon: 'undo',
  },
  {
    tail: 'index/reindex',
    label: 'Rebuild code search',
    about: 'Re-read the repository so semantic search is current.',
    icon: 'index',
  },
  {
    tail: 'memory/sync',
    label: 'Refresh memory',
    about: 'Rebuild MEMORY.md from the memory notes.',
    icon: 'file',
  },
  {
    tail: 'trust',
    label: 'Trust repo settings',
    about: "Allow the hooks and tools in this repo's .wingman/config.toml to run.",
    icon: 'check',
    confirm:
      "Trust this repository's .wingman/config.toml as it is now? Its hooks and tools will be allowed to run.",
  },
  {
    tail: 'schedule/run',
    label: 'Run scheduled prompts',
    about: 'Run any scheduled prompts that are due now.',
    icon: 'clock',
  },
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
    <section className="panel maintenance">
      <div className="panel-head">
        <div>
          <h2>Maintenance</h2>
          <p className="panel-sub">
            The same commands you would run in a terminal, for this repository. Each one reports
            what it did.
          </p>
        </div>
      </div>
      <div className="tool-grid">
        {ACTIONS.map((a) => (
          <div key={a.tail} className="tool-card">
            <span className="tile-icon" aria-hidden="true">
              <Icon name={a.icon} size={15} />
            </span>
            <span className="tool-text">
              <span className="tool-name">{a.label}</span>
              <span className="tool-about">{a.about}</span>
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
        <div className="tool-result">
          <h3 className="figure">
            {ACTIONS.find((a) => a.tail === result.tail)?.label ?? result.tail}
          </h3>
          <Output value={result.value} />
        </div>
      )}
    </section>
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
