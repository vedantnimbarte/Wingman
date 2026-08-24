import { useCallback, useEffect, useState } from 'react'
import { api, type Badge, type BoardData, type Card, type SubRow, type TaskStatus } from './api'
import { message, useEvents } from './state'
import { Failed, Loading } from './ui'

/**
 * The board.
 *
 * Every column, roll-up and badge on this screen was derived by the server
 * from the same `wingman-board` code the TUI renders. Nothing here recomputes
 * what state a card is in — that would be a second derivation to keep in sync,
 * and the first thing to disagree with `wingman board` on a Friday.
 *
 * There is no drag-and-drop, and not because it is hard in React. Moving a
 * card means forcing a task transition past the dependency gates and the
 * write-set conflict check, which is the machinery that makes runs converge.
 * If it is ever built it belongs in the orchestrator behind its own gate — see
 * BOARD-PLAN.md § Scope creep toward drag-and-drop.
 */
export function Board({ project }: { project: string | null }) {
  const [data, setData] = useState<BoardData | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const [detail, setDetail] = useState<{ card: Card; row: SubRow } | null>(null)
  const [adding, setAdding] = useState(false)
  const { recent } = useEvents()

  const load = useCallback(async () => {
    try {
      setData(await api.board())
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  // A run transition anywhere means some card's derived column may have moved.
  // Refetching on the event is what keeps this in step with `pilot watch`
  // without either polling or reimplementing the derivation client-side.
  useEffect(() => {
    if (recent.length) void load()
  }, [recent.length, load])

  if (error) return <Failed title="Could not load the board" detail={error} action={{ label: 'Try again', onClick: () => void load() }} />
  if (!data) return <Loading what="the board" />

  // The project switcher scopes the view; the board itself spans every project
  // because a card outliving its run is the whole point of the store.
  //
  // `serve`'s project ids and the board registry's are **different
  // namespaces** — both usually derive from the directory name, so they
  // usually coincide, but `[[serve.projects]].id` is user-chosen and the
  // registry slug is generated. Filtering on an id the board does not know
  // would silently show an empty board, which reads as "no cards" rather than
  // "wrong key". So scope only when the id resolves, and show everything
  // otherwise.
  const known = data.projects.find((p) => p.id === project) ?? null
  const cards = known ? data.cards.filter((c) => c.project === known.id) : data.cards

  // Auto-registration means the registry accumulates every repo pilot ever ran
  // in, including ones since deleted (BOARD-PLAN.md § Registry drift). A card
  // filed against a directory that no longer exists can never be dispatched,
  // so only live projects are offered as a destination.
  const live = data.projects.filter((p) => !p.missing)
  const target = known && !known.missing ? known : (live[0] ?? null)

  return (
    <div className="board">
      <header className="board-head">
        <div>
          <span className="eyebrow">Board</span>
          <h1>{cards.length === 1 ? '1 card' : `${cards.length} cards`}</h1>
        </div>
        <div className="board-actions">
          <span className="figure muted">{money(total(cards))}</span>
          <button type="button" className="button" onClick={() => setAdding(true)} disabled={!target}>
            Add card
          </button>
        </div>
      </header>

      {adding && target && (
        <AddCard
          projects={live}
          initial={target.id}
          onClose={() => setAdding(false)}
          onAdded={() => {
            setAdding(false)
            void load()
          }}
        />
      )}

      {cards.length === 0 ? (
        <Empty hasProject={Boolean(target)} />
      ) : (
        <div className="columns">
          {data.columns.map((col) => {
            const inCol = cards.filter((c) => c.column === col.id)
            return (
              <section key={col.id} className="column" aria-label={col.title}>
                <header className="column-head">
                  <span className="eyebrow">{col.title}</span>
                  <span className="figure muted">{inCol.length || ''}</span>
                </header>
                {inCol.map((c) => (
                  <CardTile
                    key={c.id}
                    card={c}
                    expanded={expanded.has(c.id)}
                    onToggle={() => setExpanded(toggle(expanded, c.id))}
                    onOpenRow={(row) => setDetail({ card: c, row })}
                    onChanged={() => void load()}
                  />
                ))}
              </section>
            )
          })}
        </div>
      )}

      {detail && <TaskDetail card={detail.card} row={detail.row} onClose={() => setDetail(null)} />}
    </div>
  )
}

function Empty({ hasProject }: { hasProject: boolean }) {
  return (
    <div className="state">
      <h2>No cards yet</h2>
      {hasProject ? (
        <p>
          A card is a goal you author; it outlives the runs that execute it. Add one above, or from
          a terminal with <code>wingman board add "…"</code>.
        </p>
      ) : (
        <p>
          No registered project still exists on disk. Add a repo under{' '}
          <code>[[serve.projects]]</code> in the global config and restart the daemon, or clear the
          stale ones with <code>wingman board projects --forget</code>.
        </p>
      )}
    </div>
  )
}

/* ── Card ──────────────────────────────────────────────────────────────── */

function CardTile({
  card,
  expanded,
  onToggle,
  onOpenRow,
  onChanged,
}: {
  card: Card
  expanded: boolean
  onToggle: () => void
  onOpenRow: (row: SubRow) => void
  onChanged: () => void
}) {
  const [busy, setBusy] = useState<string | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const rows = card.rollup?.subrows ?? []

  // Progress and cost are already rendered as structured fields above, on the
  // ledger axis. Showing them again as badges is the same number twice.
  const extraBadges = card.badges.filter((b) => b.kind !== 'progress' && b.kind !== 'cost')

  async function act(what: string, run: () => Promise<unknown>) {
    setBusy(what)
    setFailure(null)
    try {
      await run()
      onChanged()
    } catch (e) {
      setFailure(message(e))
    } finally {
      setBusy(null)
    }
  }

  return (
    <article className={`card${card.project_missing ? ' card-missing' : ''}`}>
      <div className="card-top">
        {rows.length > 0 ? (
          <button
            type="button"
            className="card-toggle"
            aria-expanded={expanded}
            onClick={onToggle}
            title={expanded ? 'Collapse tasks' : `Show ${rows.length} tasks`}
          >
            {expanded ? '▾' : '▸'}
          </button>
        ) : (
          <span className="card-toggle" aria-hidden="true" />
        )}
        <h3 className="card-title">{card.title}</h3>
        <span className="figure muted card-short">{card.short}</span>
      </div>

      <div className="card-meta">
        <span className="muted">{card.project_name}</span>
        {card.rollup && (
          <span className="figure">
            {card.rollup.done}/{card.rollup.total}
          </span>
        )}
        <span className="figure card-usd">{card.rollup ? money(card.rollup.usd) : ''}</span>
      </div>

      {extraBadges.length > 0 && (
        <div className="badges">
          {extraBadges.map((b) => (
            <span key={`${b.kind}:${b.text}`} className={`badge figure ${badgeClass(b.kind)}`}>
              {b.text}
            </span>
          ))}
        </div>
      )}

      {card.project_missing && (
        <p className="is-failed dot figure card-note">
          project directory is missing — relocate it with <code>wingman board projects</code>
        </p>
      )}

      {expanded &&
        rows.map((r) => (
          <button key={r.task_id} type="button" className="subrow" onClick={() => onOpenRow(r)}>
            <span className={`glyph ${statusClass(r.status)}`} aria-hidden="true">
              {glyph(r.status)}
            </span>
            <span className="subrow-title">{r.title}</span>
            <span className="muted subrow-agent">{r.agent_name ?? r.role}</span>
            {r.blocked_by.length > 0 && (
              <span className="figure is-failed" title="Held by the scheduler">
                dep {r.blocked_by.join(' ')}
              </span>
            )}
            <span className="figure subrow-usd">{money(r.usd)}</span>
          </button>
        ))}

      <div className="card-tools">
        <button
          type="button"
          className="button button-quiet"
          disabled={busy !== null || card.project_missing}
          onClick={() =>
            void act('dispatch', () => api.dispatchCard(card.id, { again: Boolean(card.run_id) }))
          }
          title={card.run_id ? 'Start another run for this card' : 'Start a pilot run for this card'}
        >
          {busy === 'dispatch' ? 'Dispatching…' : card.run_id ? 'Run again' : 'Dispatch'}
        </button>
        <button
          type="button"
          className="button button-quiet"
          disabled={busy !== null}
          onClick={() => void act('archive', () => api.archiveCard(card.id, card.archived))}
        >
          {card.archived ? 'Restore' : 'Archive'}
        </button>
      </div>

      {failure && (
        <p className="is-failed dot figure card-note" role="alert">
          {failure}
        </p>
      )}
    </article>
  )
}

/* ── Task detail ───────────────────────────────────────────────────────── */

function TaskDetail({ card, row, onClose }: { card: Card; row: SubRow; onClose: () => void }) {
  // Escape closes it — a panel you can only dismiss with the mouse is a panel
  // that traps a keyboard user.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const fields: [string, string | null][] = [
    ['Status', row.status],
    ['Role', row.role],
    ['Worker', row.agent_name],
    ['Model', row.model],
    ['Attempts', String(row.attempts)],
    ['Elapsed', row.elapsed_secs != null ? duration(row.elapsed_secs) : null],
    ['Spend', money(row.usd)],
    ['Declared writes', String(row.writes)],
    ['Dependencies', row.deps.length ? row.deps.join(', ') : null],
    ['Held by', row.blocked_by.length ? row.blocked_by.join(', ') : null],
    ['Current tool', row.current_tool],
    ['Worktree', row.worktree],
    ['Transcript', row.session_id],
    ['Run', card.run_id],
  ]

  return (
    <div className="detail" role="dialog" aria-label={`Task ${row.task_id}`}>
      <header className="detail-head">
        <div>
          <span className="eyebrow">{row.task_id}</span>
          <h2>{row.title}</h2>
        </div>
        <button type="button" className="button button-quiet" onClick={onClose}>
          Close
        </button>
      </header>

      <div className="rows">
        {fields.map(([label, value]) => (
          <div key={label} className="row">
            <span className="muted">{label}</span>
            <span className={`figure${label === 'Status' ? ` ${statusClass(row.status)}` : ''}`}>
              {value ?? '—'}
            </span>
          </div>
        ))}
      </div>

      {row.outcome && (
        <>
          <p className="eyebrow detail-outcome">Worker's outcome</p>
          <p className="figure">{row.outcome}</p>
        </>
      )}
    </div>
  )
}

/* ── Add ───────────────────────────────────────────────────────────────── */

function AddCard({
  projects,
  initial,
  onClose,
  onAdded,
}: {
  projects: BoardData['projects']
  initial: string
  onClose: () => void
  onAdded: () => void
}) {
  const [project, setProject] = useState(initial)
  const [title, setTitle] = useState('')
  const [goal, setGoal] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await api.addCard({ project, title: title.trim(), goal: goal.trim() || undefined })
      onAdded()
    } catch (err) {
      setError(message(err))
      setBusy(false)
    }
  }

  return (
    <form className="add-card" onSubmit={submit}>
      <div className="add-row">
        <label>
          <span className="eyebrow">Project</span>
          <select className="select" value={project} onChange={(e) => setProject(e.target.value)}>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </label>
        <label className="add-grow">
          <span className="eyebrow">Title</span>
          <input
            className="input"
            autoFocus
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="Fix the LSP restart storm"
          />
        </label>
      </div>
      <label>
        {/* The goal is what pilot is actually given. Saying so here is the
            difference between a card that dispatches well and one that does
            not, and it is not guessable from the word "goal". */}
        <span className="eyebrow">Goal — the prompt pilot receives (optional)</span>
        <input
          className="input"
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          placeholder="Defaults to the title"
        />
      </label>

      {error && (
        <p className="is-failed dot figure" role="alert">
          {error}
        </p>
      )}

      <div className="add-tools">
        <button type="submit" className="button" disabled={busy || title.trim() === ''}>
          {busy ? 'Adding…' : 'Add card'}
        </button>
        <button type="button" className="button button-quiet" onClick={onClose}>
          Cancel
        </button>
      </div>
    </form>
  )
}

/* ── Formatting ────────────────────────────────────────────────────────── */

/** Sum of what every card's run has spent. The figure the ledger rule carries. */
function total(cards: Card[]): number {
  return cards.reduce((sum, c) => sum + (c.rollup?.usd ?? 0), 0)
}

export function money(usd: number): string {
  if (!Number.isFinite(usd)) return '—'
  // Sub-cent spend rounds to $0.00 and reads as free. Below a cent, say so.
  if (usd > 0 && usd < 0.01) return '<$0.01'
  return `$${usd.toFixed(2)}`
}

/** Matches the board TUI's detail overlay: 45s, 3m07s, 1h04m. */
export function duration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return '—'
  const s = Math.floor(secs)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`
}

/**
 * Status carries a glyph as well as a colour, so the board still reads for
 * someone who cannot tell the three hues apart.
 */
export function glyph(status: TaskStatus): string {
  switch (status) {
    case 'done':
      return '✓'
    case 'failed':
      return '✕'
    case 'blocked':
      return '⊘'
    case 'in_progress':
      return '◐'
    case 'review':
      return '◇'
    default:
      return '·'
  }
}

export function statusClass(status: TaskStatus): string {
  switch (status) {
    case 'done':
      return 'is-proven'
    case 'failed':
    case 'blocked':
      return 'is-failed'
    case 'in_progress':
    case 'review':
      return 'is-asserted'
    default:
      return 'muted'
  }
}

/**
 * Only the badges that report a genuine failure state get colour. `retry`,
 * labels and `+2` are facts about the card, not verdicts on it — colouring
 * them would break the rule that hue means epistemic status.
 */
export function badgeClass(kind: Badge['kind']): string {
  return kind === 'failed' || kind === 'blocked' || kind === 'aborted' || kind === 'missing'
    ? 'is-failed'
    : ''
}

function toggle(set: Set<string>, id: string): Set<string> {
  const next = new Set(set)
  if (!next.delete(id)) next.add(id)
  return next
}
