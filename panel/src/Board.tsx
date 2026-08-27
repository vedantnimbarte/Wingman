import { useCallback, useEffect, useMemo, useState } from 'react'
import { useDialog } from './a11y'
import {
  api,
  type Badge,
  type BoardData,
  type Card,
  type CardDetail,
  type SubRow,
  type TaskStatus,
} from './api'
import { navigate } from './router'
import { message, useEvents } from './state'
import { Empty, Failed, Icon, Loading, Note, PageHead, Pill } from './ui'

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
  const [open, setOpen] = useState<Card | null>(null)
  const [adding, setAdding] = useState(false)
  const [filter, setFilter] = useState<Filter>({ q: '', label: null, archived: false, live: false })
  const { tick } = useEvents()

  const load = useCallback(async () => {
    try {
      setData(await api.board({ archived: filter.archived }))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [filter.archived])

  useEffect(() => {
    void load()
  }, [load])

  // A run transition anywhere means some card's derived column may have moved.
  // Refetching on the event is what keeps this in step with `pilot watch`
  // without either polling or reimplementing the derivation client-side.
  //
  // On the monotonic counter, not `recent.length`: that saturates at the ring
  // buffer's size and then never changes again, which stopped this refetching
  // entirely after fifty events while the header still read "live".
  useEffect(() => {
    if (tick) void load()
  }, [tick, load])

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not read the board"
          detail={error}
          action={{ label: 'Try again', onClick: () => void load() }}
        />
      </div>
    )
  if (!data) return <Loading what="the board" />

  const scoped = scope(data, project)
  const cards = apply(scoped, filter)

  // Auto-registration means the registry accumulates every repo pilot ever ran
  // in, including ones since deleted (BOARD-PLAN.md § Registry drift). A card
  // filed against a directory that no longer exists can never be dispatched,
  // so only live projects are offered as a destination.
  const known = resolve(data, project)
  const live = data.projects.filter((p) => !p.missing)
  const target = known && !known.missing ? known : (live[0] ?? null)
  const labels = [...new Set(scoped.flatMap((c) => c.labels))].sort()

  return (
    <div className="view view-wide">
      <PageHead
        eyebrow="Board"
        title={cards.length === 1 ? '1 card' : `${cards.length} cards`}
        intro={
          known
            ? `Cards filed against ${known.name}. A card is durable — it outlives the runs that execute it.`
            : 'Every card the registry knows about. Pick a project in the header to scope this to one repo.'
        }
        actions={
          <>
            {/* The ledger's top rung sums what is *shown*, not what exists —
                a filtered board whose total still counted hidden cards would
                be the one number on the screen that cannot be checked. */}
            <span className="figure muted" title="Spend across every card shown">
              {money(total(cards))}
            </span>
            <button
              type="button"
              className="button button-primary"
              onClick={() => setAdding(true)}
              disabled={!target}
            >
              Add card
            </button>
          </>
        }
      />

      <Filters
        filter={filter}
        labels={labels}
        shown={cards.length}
        of={scoped.length}
        onChange={setFilter}
      />

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

      {scoped.length === 0 ? (
        <NoCards hasProject={Boolean(target)} onAdd={() => setAdding(true)} />
      ) : cards.length === 0 ? (
        <Empty
          title="Nothing matches this filter"
          action={{
            label: 'Clear the filter',
            onClick: () => setFilter({ q: '', label: null, archived: false, live: false }),
          }}
        >
          {scoped.length} {scoped.length === 1 ? 'card is' : 'cards are'} filed here; none of them
          match what you have narrowed to.
        </Empty>
      ) : (
        <div className="columns">
          {data.columns.map((col) => {
            const inCol = cards.filter((c) => c.column === col.id)
            return (
              <section key={col.id} className="column" aria-label={col.title}>
                <header className="column-head">
                  <span className="eyebrow">{col.title}</span>
                  <span className="column-count">{inCol.length}</span>
                  {/* The column's rung on the ledger: the cards below sum to
                      this, and it sums into the figure in the page head. */}
                  <span className="figure muted column-usd">{money(total(inCol))}</span>
                </header>
                {inCol.length === 0 && <p className="column-empty">{emptyColumn(col.id)}</p>}
                {inCol.map((c) => (
                  <CardTile
                    key={c.id}
                    card={c}
                    expanded={expanded.has(c.id)}
                    onToggle={() => setExpanded(toggle(expanded, c.id))}
                    onOpen={() => setOpen(c)}
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
      {open && (
        <CardDrawer
          card={open}
          onClose={() => setOpen(null)}
          onChanged={() => {
            setOpen(null)
            void load()
          }}
        />
      )}
    </div>
  )
}

/* ── Scoping ───────────────────────────────────────────────────────────── */

/**
 * The board project that a `[[serve.projects]]` id refers to, if any.
 *
 * The two are **different namespaces**: `[[serve.projects]].id` is
 * user-chosen, the registry slug is generated from the directory name. They
 * usually coincide, which is exactly what makes a mismatch dangerous.
 */
export function resolve(data: BoardData, project: string | null) {
  return data.projects.find((p) => p.id === project) ?? null
}

/**
 * The cards in scope for a `serve` project id.
 *
 * Everything when the id does not resolve, because filtering on an id the
 * board does not know shows an empty board that reads as "no cards" rather
 * than "wrong key". Exported so the Overview's tiles use this resolution
 * instead of a second, subtly different one — which is precisely how they
 * came to report a confident $0.00 for a repo with runs in it.
 */
export function scope(data: BoardData, project: string | null): Card[] {
  const known = resolve(data, project)
  return known ? data.cards.filter((c) => c.project === known.id) : data.cards
}

/* ── Filtering ─────────────────────────────────────────────────────────── */

type Filter = { q: string; label: string | null; archived: boolean; live: boolean }

/** Narrowing only. Nothing here changes what a card *is*, only what is shown. */
export function apply(cards: Card[], f: Filter): Card[] {
  const q = f.q.trim().toLowerCase()
  return cards.filter((c) => {
    if (f.label && !c.labels.includes(f.label)) return false
    if (f.live && c.rollup?.status !== 'running') return false
    if (!q) return true
    return `${c.title} ${c.goal} ${c.short} ${c.labels.join(' ')}`.toLowerCase().includes(q)
  })
}

function Filters({
  filter,
  labels,
  shown,
  of,
  onChange,
}: {
  filter: Filter
  labels: string[]
  shown: number
  of: number
  onChange: (f: Filter) => void
}) {
  return (
    <div className="filters">
      <label className="filter-search">
        <Icon name="search" size={14} />
        <input
          className="input"
          type="search"
          placeholder="Filter cards"
          aria-label="Filter cards by title, goal or label"
          value={filter.q}
          onChange={(e) => onChange({ ...filter, q: e.target.value })}
        />
      </label>

      {labels.length > 0 && (
        <select
          className="select"
          aria-label="Label"
          value={filter.label ?? ''}
          onChange={(e) => onChange({ ...filter, label: e.target.value || null })}
        >
          <option value="">every label</option>
          {labels.map((l) => (
            <option key={l} value={l}>
              {l}
            </option>
          ))}
        </select>
      )}

      <label className="check">
        <input
          type="checkbox"
          checked={filter.live}
          onChange={(e) => onChange({ ...filter, live: e.target.checked })}
        />
        running only
      </label>

      {/* Archived cards come from the server, not from a client-side flag:
          `board()` returns live cards only, so hiding them here would have
          meant they were never fetched and the toggle would do nothing. */}
      <label className="check">
        <input
          type="checkbox"
          checked={filter.archived}
          onChange={(e) => onChange({ ...filter, archived: e.target.checked })}
        />
        include archived
      </label>

      {shown !== of && (
        <span className="faint figure">
          {shown} of {of}
        </span>
      )}
    </div>
  )
}

/** An empty column means something different in each column. */
function emptyColumn(id: string): string {
  switch (id) {
    case 'backlog':
      return 'Nothing filed'
    case 'planned':
      return 'Nothing planned'
    case 'in-progress':
      return 'Nothing running'
    case 'review':
      return 'Nothing waiting on you'
    case 'done':
      return 'Nothing finished yet'
    default:
      return 'Nothing here'
  }
}

function NoCards({ hasProject, onAdd }: { hasProject: boolean; onAdd: () => void }) {
  if (!hasProject) {
    return (
      <Empty title="No registered project still exists on disk">
        Add a repo under <code>[[serve.projects]]</code> in the global config and restart the
        daemon, or clear the stale ones with <code>wingman board projects --forget</code>.
      </Empty>
    )
  }
  return (
    <Empty title="No cards yet" action={{ label: 'Add the first card', onClick: onAdd }}>
      A card is a goal you author; it outlives the runs that execute it. You can also file one from
      a terminal with <code>wingman board add "…"</code>.
    </Empty>
  )
}

/* ── Card ──────────────────────────────────────────────────────────────── */

function CardTile({
  card,
  expanded,
  onToggle,
  onOpen,
  onOpenRow,
  onChanged,
}: {
  card: Card
  expanded: boolean
  onToggle: () => void
  onOpen: () => void
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
    <article
      className={`card${card.project_missing ? ' card-missing' : ''}${card.archived ? ' card-archived' : ''}`}
    >
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
        {/* The title opens the card. Everything durable about it — the goal
            pilot is actually given, the notes, every run it has ever spawned —
            was on the wire from the first release and had nowhere to be read. */}
        <button type="button" className="card-title" onClick={onOpen}>
          {card.title}
        </button>
        <span className="figure muted card-short">{card.short}</span>
      </div>

      <div className="card-meta">
        <span className="muted">{card.project_name}</span>
        {card.rollup && (
          <span className="figure muted">
            {card.rollup.done}/{card.rollup.total}
          </span>
        )}
        <span className="figure card-usd">{card.rollup ? money(card.rollup.usd) : ''}</span>
      </div>

      {/* Progress as a bar rather than a second copy of "3/7". The fill is
          muted, not green: how far along a card is is not a verdict on it. */}
      {card.rollup && card.rollup.total > 0 && (
        <span
          className="meter"
          role="img"
          aria-label={`${card.rollup.done} of ${card.rollup.total} tasks done`}
        >
          <span
            className="meter-fill"
            style={{ width: `${(card.rollup.done / card.rollup.total) * 100}%` }}
          />
        </span>
      )}

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
          className="button button-sm"
          disabled={busy !== null || card.project_missing || card.archived}
          onClick={() =>
            void act('dispatch', () => api.dispatchCard(card.id, { again: Boolean(card.run_id) }))
          }
          title={card.run_id ? 'Start another run for this card' : 'Start a pilot run for this card'}
        >
          {busy === 'dispatch' ? 'Dispatching…' : card.run_id ? 'Run again' : 'Dispatch'}
        </button>
        {card.run_id && (
          <button
            type="button"
            className="button button-quiet button-sm"
            onClick={() => navigate(`/runs/${card.run_id}`)}
            title="Open the run this card is executing"
          >
            Open run
          </button>
        )}
        <button
          type="button"
          className="button button-quiet button-sm"
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

/* ── Card detail ───────────────────────────────────────────────────────── */

/**
 * The durable half of a card: the goal pilot is handed, the notes, the labels,
 * and every run the card has ever spawned.
 *
 * Editing lives here rather than on the tile. A card written weeks ago whose
 * goal turned out to be worded badly was, until now, only fixable by deleting
 * it and losing the dispatch history that goal produced — which is the record
 * of what the wording actually cost.
 */
function CardDrawer({
  card,
  onClose,
  onChanged,
}: {
  card: Card
  onClose: () => void
  onChanged: () => void
}) {
  const ref = useDialog<HTMLDivElement>(onClose)
  const [detail, setDetail] = useState<CardDetail | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState(false)
  const [title, setTitle] = useState(card.title)
  const [goal, setGoal] = useState(card.goal)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let live = true
    api
      .card(card.id)
      .then((d) => live && setDetail(d))
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [card.id])

  async function save() {
    setBusy(true)
    setError(null)
    try {
      // Only what actually changed. Sending both would rewrite a goal someone
      // edited in a terminal between this drawer opening and Save being hit.
      const patch: { title?: string; goal?: string } = {}
      if (title.trim() !== card.title) patch.title = title.trim()
      if (goal.trim() !== card.goal) patch.goal = goal.trim()
      if (Object.keys(patch).length === 0) return setEditing(false)
      await api.updateCard(card.id, patch)
      onChanged()
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    if (
      !window.confirm(
        `Delete "${card.title}" and its dispatch history?\n\nThe runs themselves stay on disk and in wingman pilot watch — only the card is forgotten.`,
      )
    )
      return
    setBusy(true)
    try {
      await api.deleteCard(card.id)
      onChanged()
    } catch (e) {
      setError(message(e))
      setBusy(false)
    }
  }

  return (
    <div
      className="detail"
      ref={ref}
      tabIndex={-1}
      role="dialog"
      aria-modal="true"
      aria-label={`Card ${card.short}`}
    >
      <header className="detail-head">
        <div>
          <span className="eyebrow">{card.short}</span>
          <h2>{card.title}</h2>
        </div>
        <button
          type="button"
          className="button button-quiet button-icon"
          onClick={onClose}
          aria-label="Close the card panel"
          title="Close (Esc)"
        >
          <Icon name="close" />
        </button>
      </header>

      {editing ? (
        <div className="add-card">
          <label>
            <span className="eyebrow">Title</span>
            <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} />
          </label>
          <label>
            <span className="eyebrow">Goal — the prompt pilot receives</span>
            <textarea
              className="input config-area"
              rows={4}
              value={goal}
              onChange={(e) => setGoal(e.target.value)}
            />
          </label>
          <div className="actions">
            <button
              type="button"
              className="button button-primary"
              disabled={busy || title.trim() === ''}
              onClick={() => void save()}
            >
              {busy ? 'Saving…' : 'Save'}
            </button>
            <button
              type="button"
              className="button button-quiet"
              onClick={() => {
                setTitle(card.title)
                setGoal(card.goal)
                setEditing(false)
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <div className="rows">
          <div className="row row-wrap">
            <span className="muted">Goal</span>
            <span className="figure">{card.goal || '— defaults to the title'}</span>
          </div>
          <div className="row row-wrap">
            <span className="muted">Notes</span>
            <span className="figure">{card.notes ?? '—'}</span>
          </div>
          <div className="row">
            <span className="muted">Labels</span>
            <span className="figure">{card.labels.length ? card.labels.join(', ') : '—'}</span>
          </div>
          <div className="row">
            <span className="muted">Column</span>
            <span className="figure">{card.column}</span>
          </div>
          <div className="row row-wrap">
            <span className="muted">Project</span>
            <span className="figure">{card.project_name}</span>
          </div>
          <div className="row row-wrap">
            <span className="muted">Created</span>
            <span className="figure">{stamp(card.created_at)}</span>
          </div>
          <div className="row">
            <span className="muted">Archived</span>
            <span className="figure">{card.archived ? 'yes' : 'no'}</span>
          </div>
        </div>
      )}

      <h3 className="section-head">Dispatches</h3>
      {error && (
        <Note tone="is-failed" role="alert">
          {error}
        </Note>
      )}
      {!detail ? (
        <p className="faint figure">reading the history…</p>
      ) : detail.dispatches.length === 0 ? (
        <p className="faint figure">Never dispatched.</p>
      ) : (
        <div className="rows">
          {detail.dispatches.map((d) => (
            <button
              key={d.run_id}
              type="button"
              className="row row-link"
              onClick={() => navigate(`/runs/${d.run_id}`)}
            >
              <span className="figure identifier truncate">{d.run_id}</span>
              <span className="figure muted">{stamp(d.ended_at ?? d.started_at)}</span>
            </button>
          ))}
        </div>
      )}

      {!editing && (
        <div className="actions detail-tools">
          <button type="button" className="button button-sm" onClick={() => setEditing(true)}>
            Edit
          </button>
          <button
            type="button"
            className="button button-quiet button-sm"
            disabled={busy}
            onClick={() => void remove()}
          >
            <Icon name="trash" size={14} />
            Delete card
          </button>
        </div>
      )}
    </div>
  )
}

/* ── Task detail ───────────────────────────────────────────────────────── */

function TaskDetail({ card, row, onClose }: { card: Card; row: SubRow; onClose: () => void }) {
  // Escape closes it, focus moves in and comes back out — a panel you can only
  // dismiss with the mouse traps a keyboard user, and one that never took
  // focus never announced itself in the first place.
  const ref = useDialog<HTMLDivElement>(onClose)

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
    <div
      className="detail"
      ref={ref}
      tabIndex={-1}
      role="dialog"
      aria-modal="true"
      aria-label={`Task ${row.task_id}`}
    >
      <header className="detail-head">
        <div>
          <span className="eyebrow">{row.task_id}</span>
          <h2>{row.title}</h2>
        </div>
        <button
          type="button"
          className="button button-quiet button-icon"
          onClick={onClose}
          aria-label="Close the task panel"
          title="Close (Esc)"
        >
          <Icon name="close" />
        </button>
      </header>

      <div className="rows">
        {fields.map(([label, value]) =>
          label === 'Status' ? (
            <div key={label} className="row">
              <span className="muted">{label}</span>
              <Pill status={statusClass(row.status)} glyph={glyph(row.status)}>
                {row.status.replace('_', ' ')}
              </Pill>
            </div>
          ) : (
            <div key={label} className="row">
              <span className="muted">{label}</span>
              <span className="figure">{value ?? '—'}</span>
            </div>
          ),
        )}
      </div>

      {row.outcome && (
        <>
          <p className="eyebrow detail-outcome">Worker's outcome</p>
          <p className="figure">{row.outcome}</p>
        </>
      )}

      {/* The most common next click from a task was reading its run id off the
          panel and finding it by hand. */}
      <div className="actions detail-tools">
        {card.run_id && (
          <button
            type="button"
            className="button button-sm"
            onClick={() => navigate(`/runs/${card.run_id}`)}
          >
            Open the run
          </button>
        )}
        {row.session_id && (
          <button
            type="button"
            className="button button-quiet button-sm"
            onClick={() => navigate(`/sessions/${row.session_id}`)}
            title="The worker's transcript, if it is in the selected project"
          >
            Open the transcript
          </button>
        )}
      </div>
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
  const [notes, setNotes] = useState('')
  const [labels, setLabels] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const parsedLabels = useMemo(
    () =>
      labels
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean),
    [labels],
  )

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await api.addCard({
        project,
        title: title.trim(),
        goal: goal.trim() || undefined,
        notes: notes.trim() || undefined,
        labels: parsedLabels,
      })
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
      <div className="add-row">
        <label className="add-grow">
          <span className="eyebrow">Notes (optional)</span>
          <input
            className="input"
            value={notes}
            onChange={(e) => setNotes(e.target.value)}
            placeholder="Context for you, not for the agent"
          />
        </label>
        <label>
          <span className="eyebrow">Labels</span>
          <input
            className="input"
            value={labels}
            onChange={(e) => setLabels(e.target.value)}
            placeholder="comma, separated"
          />
        </label>
      </div>

      {error && (
        <p className="is-failed dot figure" role="alert">
          {error}
        </p>
      )}

      <div className="actions">
        <button
          type="submit"
          className="button button-primary"
          disabled={busy || title.trim() === ''}
        >
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

/**
 * A stored timestamp, as something a person reads.
 *
 * The board writes RFC-3339 with nanosecond precision and an offset —
 * `2026-08-21T17:06:02.240238400+00:00` — which is correct to store and
 * unreadable in a row. Minutes is as fine as any of this gets. An unparseable
 * value is returned untouched rather than becoming "Invalid Date".
 */
export function stamp(iso: string | null): string {
  if (!iso) return '—'
  const at = iso.indexOf('T')
  if (at === -1) return iso
  return `${iso.slice(0, at)} ${iso.slice(at + 1, at + 6)}`
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
