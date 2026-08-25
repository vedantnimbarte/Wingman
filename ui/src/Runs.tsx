import { useCallback, useEffect, useState } from 'react'
import {
  api,
  type Agent,
  type ControlAction,
  type RunState,
  type RunSummary,
  type RunStatus,
  type Task,
  type TaskStatus,
} from './api'
import { duration, glyph, money, statusClass } from './Board'
import { navigate } from './router'
import { message, useEvents } from './state'
import { Empty, Failed, Icon, Loading, Note, PageHead, Pill } from './ui'

/**
 * Pilot runs, live.
 *
 * Zero new server code: the run list, the full `state.json` snapshot, the
 * `tasks.jsonl` stream and the four control routes all shipped with
 * `wingman serve`. This phase is a renderer.
 *
 * The plan gate is where a browser genuinely beats the terminal — approving a
 * seven-task plan is better with the whole plan on screen and a mouse.
 */
export function Runs({ project, runId }: { project: string | null; runId: string | null }) {
  if (!project) {
    return (
      <div className="view">
        <Failed
          title="No project selected"
          detail="Runs are per-repo. Pick one in the header."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  return runId ? <RunDetail project={project} runId={runId} /> : <RunList project={project} />
}

/* ── List ──────────────────────────────────────────────────────────────── */

function RunList({ project }: { project: string }) {
  const [runs, setRuns] = useState<RunSummary[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const { recent } = useEvents()

  const load = useCallback(async () => {
    try {
      setRuns(await api.runs(project))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project])

  useEffect(() => {
    void load()
  }, [load])

  // `/v1/events` carries run transitions across every project, so a run
  // starting or finishing anywhere is the signal to re-read this list.
  useEffect(() => {
    if (recent.length) void load()
  }, [recent.length, load])

  if (error) return <div className="view"><Failed title="Could not list runs" detail={error} action={{ label: 'Try again', onClick: () => void load() }} /></div>
  if (!runs) return <Loading what="runs" />

  return (
    <div className="view">
      <PageHead
        eyebrow="Runs"
        title={runs.length === 1 ? '1 run' : `${runs.length} runs`}
        intro={
          <>
            Every pilot run in this repo, newest first. Runs are read from{' '}
            <code>.wingman/autonomous/</code> on disk, so one started from a terminal shows up here
            and one started here shows up in <code>wingman pilot watch</code>.
          </>
        }
        actions={
          <button type="button" className="button" onClick={() => navigate('/board')}>
            Go to the board
          </button>
        }
      />

      {runs.length === 0 ? (
        <Empty
          title="No runs yet"
          action={{ label: 'Dispatch a card', onClick: () => navigate('/board') }}
        >
          A run starts when you dispatch a card from the board, or from a terminal with{' '}
          <code>wingman pilot run "&hellip;"</code>.
        </Empty>
      ) : (
        <div className="rows">
          {runs.map((r) => (
            <button
              key={r.run_id}
              type="button"
              className="row row-link run-row"
              onClick={() => navigate(`/runs/${r.run_id}`)}
            >
              <span className="run-goal">
                <span className="truncate">{r.goal}</span>
                <span className="figure faint identifier">{r.run_id}</span>
              </span>
              <span className="figure run-progress">
                {r.done}/{r.total}
              </span>
              <Pill status={runClass(r.status)} glyph={runGlyph(r.status)}>
                {r.status.replace('_', ' ')}
              </Pill>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/* ── Detail ────────────────────────────────────────────────────────────── */

function RunDetail({ project, runId }: { project: string; runId: string }) {
  const [run, setRun] = useState<RunState | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(false)
  const [busy, setBusy] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [sent, setSent] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setRun(await api.run(project, runId))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project, runId])

  useEffect(() => {
    void load()
  }, [load])

  // Refetch the snapshot on every stream event rather than applying events to
  // local state. `state.json` is written atomically after each event and is a
  // small local read, so this stays authoritative — applying events by hand
  // would be a second reducer to keep in step with the orchestrator's.
  useEffect(() => {
    const src = new EventSource(
      `/v1/projects/${encodeURIComponent(project)}/pilot/runs/${encodeURIComponent(runId)}/stream?tail=0`,
    )
    src.onopen = () => setLive(true)
    src.onerror = () => setLive(false)
    src.onmessage = () => void load()
    // The server closes the stream with an `end` event once the run is
    // terminal. Nothing more is coming, so stop reporting a live link.
    src.addEventListener('end', () => {
      setLive(false)
      src.close()
      void load()
    })
    return () => src.close()
  }, [project, runId, load])

  async function control(action: ControlAction, body: { task?: string } = {}) {
    setBusy(action + (body.task ?? ''))
    setActionError(null)
    setSent(null)
    try {
      await api.control(project, runId, action, body)
      // Each control route appends one command to `control.jsonl`; the
      // orchestrator's watchdog applies it on its own schedule. So a
      // successful call means "recorded", not "done" — and without saying so,
      // a click on a gated run looks like nothing happened. Optimistically
      // flipping the status here would be the dishonest alternative.
      setSent(`${action}${body.task ? ` ${body.task}` : ''}`)
      await load()
    } catch (e) {
      setActionError(message(e))
    } finally {
      setBusy(null)
    }
  }

  if (error) return <div className="view"><Failed title="Could not load the run" detail={error} action={{ label: 'Back to runs', onClick: () => navigate('/runs') }} /></div>
  if (!run) return <Loading what="the run" />

  const gated = run.status === 'awaiting_approval'
  const terminal = run.status === 'done' || run.status === 'failed' || run.status === 'aborted'

  return (
    <div className="view">
      <button type="button" className="button button-quiet back" onClick={() => navigate('/runs')}>
        <Icon name="collapse" size={14} />
        Runs
      </button>

      {/* The run id is an identifier someone will copy into a CLI command, so
          it is not put through the eyebrow's uppercase transform — a
          transformed id reads as real and pastes as wrong. */}
      <PageHead
        eyebrow={
          <>
            <span className="figure identifier">{run.run_id}</span> ·{' '}
            {live ? 'live' : terminal ? 'finished' : 'not streaming'}
          </>
        }
        title={run.goal}
        actions={
          terminal ? null : (
            <button
              type="button"
              className="button"
              disabled={busy !== null}
              onClick={() => void control('abort')}
            >
              {busy === 'abort' ? 'Aborting…' : 'Abort run'}
            </button>
          )
        }
      />

      <div className="rows run-summary">
        <div className="row">
          <span className="muted">Status</span>
          <Pill status={runClass(run.status)} glyph={runGlyph(run.status)}>
            {run.status.replace('_', ' ')}
          </Pill>
        </div>
        <div className="row">
          <span className="muted">Tasks</span>
          <span className="figure">
            {run.tasks.filter((t) => t.status === 'done').length}/{run.tasks.length} done
          </span>
        </div>
        <div className="row">
          <span className="muted">Spend</span>
          <span className="figure">{money(run.totals.usd)}</span>
        </div>
        <div className="row">
          <span className="muted">Tokens</span>
          <span className="figure">
            {run.totals.tokens_in.toLocaleString()} in · {run.totals.tokens_out.toLocaleString()} out
          </span>
        </div>
        <div className="row">
          <span className="muted">Branch</span>
          <span className="figure">{run.integration_branch}</span>
        </div>
        {run.pr_url && (
          <div className="row">
            <span className="muted">Pull request</span>
            <span className="figure">{run.pr_url}</span>
          </div>
        )}
      </div>

      {gated && (
        <section className="gate">
          <h2 className="is-asserted dot">This run is waiting for you</h2>
          <p>
            Pilot planned {run.tasks.length} {run.tasks.length === 1 ? 'task' : 'tasks'} and will not
            start until the plan is approved. Read it below, then decide.
          </p>
          <div className="actions">
            <button
              type="button"
              className="button button-primary"
              disabled={busy !== null}
              onClick={() => void control('approve')}
            >
              {busy === 'approve' ? 'Approving…' : 'Approve plan'}
            </button>
            <button
              type="button"
              className="button button-quiet"
              disabled={busy !== null}
              onClick={() => void control('veto')}
            >
              Reject plan
            </button>
          </div>
        </section>
      )}

      {actionError && (
        <Note tone="is-failed" role="alert">
          {actionError}
        </Note>
      )}

      {sent && !actionError && (
        <Note tone="is-asserted">
          Sent <code>{sent}</code> — the run applies it on its next check.
        </Note>
      )}

      <h2 className="section-head">Plan</h2>
      <p className="section-intro">
        Indented by dependency depth. A task runs when every task it names has finished and no other
        running task claims a file it declared it would write.
      </p>
      <div className="rows">
        {ordered(run.tasks).map(({ task, depth }) => (
          <TaskRow
            key={task.id}
            task={task}
            depth={depth}
            agent={run.agents.find((a) => a.id === task.agent) ?? null}
            terminal={terminal}
            busy={busy}
            onControl={control}
          />
        ))}
      </div>

      {run.agents.length > 0 && (
        <>
          <h2 className="section-head">Workers</h2>
          <div className="rows">
            {run.agents.map((a) => (
              <div key={a.id} className="row">
                <span className="worker-row">
                  <span className={`dot ${agentClass(a.status)}`} aria-hidden="true" />
                  <span className="truncate">
                    {a.name || a.id}
                    <span className="muted"> · {a.role}</span>
                    {a.current_tool && <span className="muted"> · {a.current_tool}</span>}
                  </span>
                </span>
                <span className="figure">
                  <span className="muted">{a.model ?? '—'}</span> {money(a.usd)}
                </span>
              </div>
            ))}
          </div>
        </>
      )}
    </div>
  )
}

function TaskRow({
  task,
  depth,
  agent,
  terminal,
  busy,
  onControl,
}: {
  task: Task
  depth: number
  agent: Agent | null
  terminal: boolean
  busy: string | null
  onControl: (action: ControlAction, body: { task?: string }) => void
}) {
  const [open, setOpen] = useState(false)
  const elapsed = elapsedSecs(task, terminal)

  // Only offer what the server will accept: retry is for a task that has
  // stopped without finishing, abort for one still moving. Rendering a button
  // that always 409s is worse than rendering none.
  const canRetry = task.status === 'failed' || task.status === 'blocked'
  const canAbort = !terminal && (task.status === 'in_progress' || task.status === 'review')

  return (
    <div className="row task-row" style={{ paddingLeft: `${depth * 1.25}rem` }}>
      <span className="task-main">
        <button type="button" className="task-toggle" onClick={() => setOpen(!open)}>
          <span className={`glyph ${statusClass(task.status)}`} aria-hidden="true">
            {glyph(task.status)}
          </span>
          <span className="figure muted">{task.id}</span> {task.title}
        </button>

        <span className="task-meta muted">
          {task.role}
          {agent && ` · ${agent.name || agent.id}`}
          {task.attempts > 1 && ` · attempt ${task.attempts}`}
          {task.deps.length > 0 && ` · needs ${task.deps.join(' ')}`}
        </span>

        {open && (
          <dl className="task-detail">
            {task.goal && <Field label="Goal" value={task.goal} />}
            <Field label="Status" value={task.status} cls={statusClass(task.status)} />
            <Field label="Elapsed" value={elapsed != null ? duration(elapsed) : null} />
            <Field label="Model" value={agent?.model ?? null} />
            <Field label="Worktree" value={task.worktree} />
            <Field
              label="Declared writes"
              value={task.writes.length ? task.writes.join(', ') : null}
            />
            <Field
              label="Commits"
              value={task.commits.length ? String(task.commits.length) : null}
            />
            <Field label="Transcript" value={agent?.session_id ?? null} />
            {task.outcome && <Field label="Outcome" value={task.outcome.summary} />}
            {task.outcome?.files_changed.length ? (
              <Field label="Files changed" value={task.outcome.files_changed.join(', ')} />
            ) : null}
          </dl>
        )}

        {(canRetry || canAbort) && (
          <span className="task-tools">
            {canRetry && (
              <button
                type="button"
                className="button button-sm"
                disabled={busy !== null}
                onClick={() => onControl('retry', { task: task.id })}
              >
                Retry
              </button>
            )}
            {canAbort && (
              <button
                type="button"
                className="button button-quiet button-sm"
                disabled={busy !== null}
                onClick={() => onControl('abort', { task: task.id })}
              >
                Abort task
              </button>
            )}
          </span>
        )}
      </span>

      <span className="figure task-usd">{money(task.usd)}</span>
    </div>
  )
}

function Field({ label, value, cls }: { label: string; value: string | null; cls?: string }) {
  if (!value) return null
  return (
    <>
      <dt className="eyebrow">{label}</dt>
      <dd className={`figure ${cls ?? ''}`}>{value}</dd>
    </>
  )
}

/* ── Derivation ────────────────────────────────────────────────────────── */

/**
 * Order tasks so a dependency always precedes what needs it, and report how
 * deep each sits — the indentation is the DAG.
 *
 * Depth is the longest path to a root, not the shortest, so a task waiting on
 * both `t1` and a chain through `t2` renders below the whole chain rather than
 * jumping up beside `t1`.
 *
 * A dependency cycle would make depth unbounded; the walk is bounded by the
 * task count and anything still unresolved is emitted flat. Pilot's planner
 * rejects cycles, so this is a guard against a malformed `state.json` on disk
 * rather than an expected case — but an infinite loop in a renderer is a hung
 * tab, which is a worse way to find out.
 */
export function ordered(tasks: Task[]): { task: Task; depth: number }[] {
  const byId = new Map(tasks.map((t) => [t.id, t]))
  const depth = new Map<string, number>()

  for (let pass = 0; pass < tasks.length + 1; pass++) {
    let changed = false
    for (const t of tasks) {
      const deps = t.deps.filter((d) => byId.has(d))
      const known = deps.every((d) => depth.has(d))
      if (!known) continue
      const next = deps.length === 0 ? 0 : Math.max(...deps.map((d) => depth.get(d)!)) + 1
      if (depth.get(t.id) !== next) {
        depth.set(t.id, next)
        changed = true
      }
    }
    if (!changed) break
  }

  return tasks
    .map((task) => ({ task, depth: depth.get(task.id) ?? 0 }))
    .sort((a, b) => a.depth - b.depth || a.task.id.localeCompare(b.task.id))
}

/**
 * Wall time from first `in_progress`.
 *
 * Three cases, and the third is the one that matters. A task that recorded an
 * `ended_at` is exact. A task still running on a live run counts up from its
 * start, correctly. But a task with no `ended_at` on a run that has already
 * finished never recorded an end — counting from `now` there would report the
 * time since the run died, which grows forever and is not elapsed work.
 *
 * `wingman-board` applies the same `now`-when-unfinished rule, and its
 * roll-ups are cached against `state.json`'s mtime, so a dead run's card keeps
 * whatever figure was current when it was last written. That is why the board
 * and this view can quote different numbers for the same task; neither was
 * wrong about the rule, and a clock that ticks after the run is over is the
 * part worth not repeating.
 */
export function elapsedSecs(task: Task, runTerminal: boolean, now = Date.now()): number | null {
  if (!task.started_at) return null
  const start = Date.parse(task.started_at)
  if (Number.isNaN(start)) return null

  if (!task.ended_at) {
    if (runTerminal) return null
    return Math.max(0, (now - start) / 1000)
  }

  const end = Date.parse(task.ended_at)
  if (Number.isNaN(end)) return null
  return Math.max(0, (end - start) / 1000)
}

/** The run's state as a glyph, so the pill never depends on hue alone. */
function runGlyph(status: RunStatus): string {
  switch (status) {
    case 'done':
      return '✓'
    case 'failed':
    case 'aborted':
      return '✕'
    case 'awaiting_approval':
      return '◇'
    default:
      return '◐'
  }
}

function runClass(status: RunStatus): string {
  switch (status) {
    case 'done':
      return 'is-proven'
    case 'failed':
    case 'aborted':
      return 'is-failed'
    default:
      return 'is-asserted'
  }
}

function agentClass(status: string): string {
  switch (status) {
    case 'done':
      return 'is-proven'
    case 'failed':
    case 'aborted':
      return 'is-failed'
    case 'in_progress':
      return 'is-asserted'
    default:
      return 'muted'
  }
}

export type { TaskStatus }
