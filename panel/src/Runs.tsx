import { useCallback, useEffect, useState } from 'react'
import {
  api,
  type Agent,
  type ControlAction,
  type RunLogEvent,
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
  const [starting, setStarting] = useState(false)
  const { tick } = useEvents()

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
  // starting or finishing anywhere is the signal to re-read this list. On the
  // monotonic counter rather than the buffer length, which stops changing.
  useEffect(() => {
    if (tick) void load()
  }, [tick, load])

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not list runs"
          detail={error}
          action={{ label: 'Try again', onClick: () => void load() }}
        />
      </div>
    )
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
          <>
            <button type="button" className="button" onClick={() => navigate('/board')}>
              Go to the board
            </button>
            <button
              type="button"
              className="button button-primary"
              onClick={() => setStarting(true)}
            >
              New run
            </button>
          </>
        }
      />

      {starting && (
        <StartRun
          project={project}
          onClose={() => setStarting(false)}
          onStarted={(id) => navigate(`/runs/${id}`)}
        />
      )}

      {runs.length === 0 ? (
        <Empty
          title="No runs yet"
          action={{ label: 'Start one', onClick: () => setStarting(true) }}
        >
          A run starts from a goal here, by dispatching a card from the board, or from a terminal
          with <code>wingman pilot run "&hellip;"</code>.
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

/**
 * Start a run from a goal.
 *
 * The board's Dispatch was the only way to start work from the panel, so
 * anything not worth filing a card for meant going back to a terminal — which
 * is the trip this whole surface exists to avoid.
 *
 * **Plan-only is the default and the gate is on.** `--yes` skips the approval
 * that makes the plan gate worth having, and a checkbox that spends money
 * unattended should be the one you tick deliberately, not the one you forget
 * to untick.
 */
function StartRun({
  project,
  onClose,
  onStarted,
}: {
  project: string
  onClose: () => void
  onStarted: (runId: string) => void
}) {
  const [goal, setGoal] = useState('')
  const [model, setModel] = useState('')
  const [planOnly, setPlanOnly] = useState(false)
  const [yes, setYes] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const { run_id } = await api.startRun(project, {
        goal: goal.trim(),
        yes,
        plan_only: planOnly,
        model: model.trim() || undefined,
      })
      onStarted(run_id)
    } catch (err) {
      setError(message(err))
      setBusy(false)
    }
  }

  return (
    <form className="add-card" onSubmit={submit}>
      <label>
        <span className="eyebrow">Goal — the prompt pilot plans against</span>
        <textarea
          className="input config-area"
          rows={3}
          autoFocus
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          placeholder="Add SSE keepalives to the events route and cover them with a test"
        />
      </label>

      <div className="add-row">
        <label className="add-grow">
          <span className="eyebrow">Model (optional)</span>
          <input
            className="input"
            value={model}
            onChange={(e) => setModel(e.target.value)}
            placeholder="whatever the config says"
            spellCheck={false}
          />
        </label>
      </div>

      <div className="filters">
        <label className="check">
          <input
            type="checkbox"
            checked={planOnly}
            onChange={(e) => setPlanOnly(e.target.checked)}
          />
          plan only — stop after planning, execute nothing
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={yes}
            onChange={(e) => setYes(e.target.checked)}
            disabled={planOnly}
          />
          skip the approval gate
        </label>
      </div>

      {yes && !planOnly && (
        <Note tone="is-asserted">
          This run will plan and then start executing without asking. It spends real money against
          your key.
        </Note>
      )}

      {error && (
        <Note tone="is-failed" role="alert">
          {error}
        </Note>
      )}

      <div className="actions">
        <button
          type="submit"
          className="button button-primary"
          disabled={busy || goal.trim() === ''}
        >
          {busy ? 'Starting…' : 'Start run'}
        </button>
        <button type="button" className="button button-quiet" onClick={onClose}>
          Cancel
        </button>
      </div>
    </form>
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
  const [log, setLog] = useState<RunLogEvent[]>([])

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

  // The backlog, once. Without it the log opens empty on a run that has been
  // going for an hour, which reads as "nothing has happened".
  useEffect(() => {
    let alive = true
    api
      .runEvents(project, runId, 60)
      .then((r) => alive && setLog(r.events.filter(isLogEvent)))
      .catch(() => {
        /* The log is a courtesy. The snapshot below is the run. */
      })
    return () => {
      alive = false
    }
  }, [project, runId])

  // Refetch the snapshot on every stream event rather than applying events to
  // local state. `state.json` is written atomically after each event and is a
  // small local read, so this stays authoritative — applying events by hand
  // would be a second reducer to keep in step with the orchestrator's.
  //
  // The event itself is still kept, but only for the log: what it says has
  // already been folded into the snapshot by the time it is rendered.
  useEffect(() => {
    const src = new EventSource(
      `/v1/projects/${encodeURIComponent(project)}/pilot/runs/${encodeURIComponent(runId)}/stream?tail=0`,
    )
    src.onopen = () => setLive(true)
    src.onerror = () => setLive(false)
    src.onmessage = (m: MessageEvent<string>) => {
      try {
        const parsed: unknown = JSON.parse(m.data)
        if (isLogEvent(parsed)) setLog((prev) => [...prev, parsed].slice(-200))
      } catch {
        /* Not JSON. The snapshot refetch below is what matters. */
      }
      void load()
    }
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

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not load the run"
          detail={error}
          action={{ label: 'Back to runs', onClick: () => navigate('/runs') }}
        />
      </div>
    )
  if (!run) return <Loading what="the run" />

  const gated = run.status === 'awaiting_approval'
  const terminal = run.status === 'done' || run.status === 'failed' || run.status === 'aborted'
  const irreversible = run.tasks.filter((t) => isIrreversible(t))

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
        <div className="row row-wrap">
          <span className="muted">Base commit</span>
          <span className="figure identifier">{run.base_commit || '—'}</span>
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

          {/* The one fact the gate was missing. Pilot classifies every task's
              reversibility and says why; approving a plan without seeing which
              parts cannot be undone is approving the wrong thing carefully. */}
          {irreversible.length > 0 && (
            <div className="gate-warn">
              <p className="is-failed dot">
                {irreversible.length === 1
                  ? '1 task is not cleanly reversible:'
                  : `${irreversible.length} tasks are not cleanly reversible:`}
              </p>
              <ul className="md-list">
                {irreversible.map((t) => (
                  <li key={t.id}>
                    <span className="figure muted">{t.id}</span> {t.title}
                    <span className="muted">
                      {' — '}
                      {t.reversibility_reason ?? t.reversibility}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}

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

      {/* Below the plan, not up in the page head. Aborting is the one
          irreversible thing this screen does, and it reads the plan it is
          ending — a destructive control sitting next to the title is one the
          hand finds before the eye has read what it would stop. */}
      {!terminal && (
        <div className="actions run-tools">
          <button
            type="button"
            className="button button-quiet"
            disabled={busy !== null}
            onClick={() => void control('abort')}
          >
            {busy === 'abort' ? 'Aborting…' : 'Abort run'}
          </button>
        </div>
      )}

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
                    {a.pid != null && <span className="faint"> · pid {a.pid}</span>}
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

      <RunLog events={log} live={live} />
    </div>
  )
}

/* ── Live log ──────────────────────────────────────────────────────────── */

/**
 * What the run has actually been doing.
 *
 * The detail view held this stream from the first release and threw every
 * event away, using it only as a signal to refetch — so a run that was quietly
 * retrying the same tool for ten minutes looked identical to one making
 * progress. Folded away by default: it is the working-out, not the state.
 */
function RunLog({ events, live }: { events: RunLogEvent[]; live: boolean }) {
  if (events.length === 0) return null
  return (
    <>
      <h2 className="section-head">Activity</h2>
      <details className="tool run-log">
        <summary>
          <span className={`glyph ${live ? 'is-asserted' : 'muted'}`}>{live ? '◐' : '·'}</span>
          <span className="figure">
            {events.length === 200 ? 'last 200 events' : `${events.length} events`}
          </span>
        </summary>
        <div className="rows">
          {[...events].reverse().map((e, i) => (
            <div key={`${e.t}-${i}`} className="row log-row">
              <span className="truncate">{summarise(e)}</span>
              <span className="figure faint">{clockOf(e.t)}</span>
            </div>
          ))}
        </div>
      </details>
    </>
  )
}

/** Anything with the two keys serde guarantees on every variant. */
export function isLogEvent(v: unknown): v is RunLogEvent {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as RunLogEvent).ev === 'string' &&
    typeof (v as RunLogEvent).t === 'string'
  )
}

/**
 * One line of prose per event.
 *
 * Reads fields defensively rather than switching on a mirrored union: the
 * variants are Rust's, they gain fields, and a log line is not worth a type
 * that has to be re-derived every time one does. An unrecognised `ev` still
 * renders — as itself, which is more useful than being dropped.
 */
export function summarise(e: RunLogEvent): string {
  const s = (k: string): string => (typeof e[k] === 'string' ? (e[k] as string) : '')
  switch (e.ev) {
    case 'run.start':
      return `run started — ${s('goal')}`
    case 'task.create':
      return `planned ${s('id')} — ${s('title')}`
    case 'task.assign':
      return `${s('id')} assigned to ${s('agent')}`
    case 'task.status':
      return `${s('id')} → ${s('status')}`
    case 'task.tool':
      return `${s('id')} ran ${s('tool')}${e.ok === false ? ' (failed)' : ''}${
        s('file') ? ` on ${s('file')}` : ''
      }`
    case 'task.commit':
      return `${s('id')} committed ${s('sha').slice(0, 8)}`
    case 'agent.spawn':
      return `spawned ${s('agent')} (${s('role')})`
    case 'agent.status':
      return `${s('agent')} → ${s('status')}`
    default:
      return `${e.ev}${s('id') ? ` ${s('id')}` : ''}`
  }
}

/** `2026-08-21T20:05:11Z` → `20:05:11`. The date is the run's, not the line's. */
export function clockOf(t: string): string {
  const at = t.indexOf('T')
  return at === -1 ? t : t.slice(at + 1, at + 9)
}

/* ── One task ──────────────────────────────────────────────────────────── */

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
  const elapsed = useElapsed(task, terminal)

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
          {isIrreversible(task) && (
            <span className="badge figure is-failed" title={task.reversibility_reason ?? undefined}>
              {task.reversibility}
            </span>
          )}
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
            <Field label="Reversibility" value={task.reversibility} />
            <Field label="Why" value={task.reversibility_reason} />
            <Field label="Worktree" value={task.worktree} />
            <Field
              label="Declared writes"
              value={task.writes.length ? task.writes.join(', ') : null}
            />
            <Field
              label="Acceptance"
              value={task.acceptance.length ? `${task.acceptance.length} check(s)` : null}
            />
            {/* The count was all this said. The shas are what someone actually
                wants — they are what you paste into `git show`. */}
            {task.commits.length > 0 && (
              <>
                <dt className="eyebrow">Commits</dt>
                <dd className="figure identifier">
                  {task.commits.map((c) => c.slice(0, 8)).join(' ')}
                </dd>
              </>
            )}
            <Field label="Transcript" value={agent?.session_id ?? null} />
            <Field label="Spawned" value={agent?.spawned_at ?? null} />
            {task.outcome && <Field label="Outcome" value={task.outcome.summary} />}
            {task.outcome?.files_changed.length ? (
              <>
                <dt className="eyebrow">Files changed</dt>
                <dd className="figure">
                  <ul className="file-list">
                    {task.outcome.files_changed.map((f) => (
                      <li key={f}>{f}</li>
                    ))}
                  </ul>
                </dd>
              </>
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

/** Pilot's own classification. Anything but `reversible` is worth flagging. */
export function isIrreversible(task: Task): boolean {
  const r = (task.reversibility ?? '').toLowerCase()
  return r !== '' && r !== 'reversible'
}

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
 * Elapsed, and actually counting.
 *
 * `elapsedSecs` was correct and rendered once — a running task's clock only
 * moved when an unrelated event forced a re-render, so it advanced in jumps of
 * whatever the run happened to be doing. The interval exists only while there
 * is something to count: a finished task, or a task on a finished run, has a
 * fixed number and does not earn a timer.
 */
function useElapsed(task: Task, terminal: boolean): number | null {
  const ticking = Boolean(task.started_at) && !task.ended_at && !terminal
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    if (!ticking) return
    const id = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(id)
  }, [ticking])

  return elapsedSecs(task, terminal, now)
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
