import { Fragment, useCallback, useEffect, useState } from 'react'
import {
  api,
  type Agent,
  type ControlAction,
  type PilotLog,
  type RunLogEvent,
  type RunState,
  type RunSummary,
  type RunStatus,
  type SessionRecord,
  type Task,
  type TaskStatus,
} from './api'
import { duration, glyph, money, statusClass } from './Board'
import { Report } from './output'
import { navigate } from './router'
import { Transcript } from './Sessions'
import { message, useEvents } from './state'
import { Empty, Failed, Icon, Loading, Note, PageHead, Pill } from './ui'
import { MicButton } from './voice'

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
  const [view, setView] = useState<RunView>('all')
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
          title="Couldn't load runs"
          detail={error}
          action={{ label: 'Try again', onClick: () => void load() }}
        />
      </div>
    )
  if (!runs) return <Loading what="runs" />

  const shown = runs.filter((r) => matchesView(r, view))
  const counts = {
    all: runs.length,
    active: runs.filter((r) => matchesView(r, 'active')).length,
    approval: runs.filter((r) => matchesView(r, 'approval')).length,
    finished: runs.filter((r) => matchesView(r, 'finished')).length,
  }

  return (
    <div className="view">
      <PageHead
        title="Runs"
        intro={
          <>
            A run is Wingman working through a goal: it plans tasks, hands them to agents, and
            checks the result. Runs started from a terminal with <code>wingman pilot</code> show up
            here too.
          </>
        }
        actions={
          <button
            type="button"
            className="button button-primary"
            onClick={() => setStarting(true)}
          >
            <Icon name="plus" size={15} />
            New run
          </button>
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
          action={{ label: 'Start a run', onClick: () => setStarting(true) }}
        >
          Describe a goal and Wingman plans it, asks for your approval, then does the work. You can
          also start one by pressing Start on a board card.
        </Empty>
      ) : (
        <>
          <div className="filters" role="toolbar" aria-label="Show runs">
            {VIEWS.map((v) => (
              <button
                key={v.id}
                type="button"
                className="chip"
                aria-pressed={view === v.id}
                onClick={() => setView(v.id)}
              >
                {v.label}
                <span className="chip-count">{counts[v.id]}</span>
              </button>
            ))}
          </div>

          {shown.length === 0 ? (
            <p className="panel-empty">No runs match this view.</p>
          ) : (
            <div className="run-cards">
              {shown.map((r) => (
                <button
                  key={r.run_id}
                  type="button"
                  className={`run-card${r.status === 'awaiting_approval' ? ' needs-you' : ''}`}
                  onClick={() => navigate(`/runs/${r.run_id}`)}
                >
                  <span className="run-card-top">
                    <Pill status={runClass(r.status)} glyph={runGlyph(r.status)}>
                      {runLabel(r.status)}
                    </Pill>
                    <span className="run-card-id mono truncate">{r.run_id}</span>
                  </span>
                  <span className="run-card-goal">{r.goal}</span>
                  <span className="run-card-progress">
                    <span>
                      {r.total ? `${r.done} of ${r.total} tasks done` : 'Planning the tasks…'}
                    </span>
                    <span className={`meter ${runClass(r.status)}`} aria-hidden="true">
                      <span style={{ width: `${r.total ? (r.done / r.total) * 100 : 0}%` }} />
                    </span>
                  </span>
                  <span className="run-card-cta">
                    {r.status === 'awaiting_approval' ? 'Review the plan' : 'View details'}
                  </span>
                </button>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  )
}

type RunView = 'all' | 'active' | 'approval' | 'finished'

const VIEWS: { id: RunView; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'active', label: 'In progress' },
  { id: 'approval', label: 'Needs approval' },
  { id: 'finished', label: 'Finished' },
]

function matchesView(r: RunSummary, view: RunView): boolean {
  switch (view) {
    case 'active':
      return !r.terminal && r.status !== 'awaiting_approval'
    case 'approval':
      return r.status === 'awaiting_approval'
    case 'finished':
      return r.terminal
    default:
      return true
  }
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
        <span className="field-label">What should Wingman do?</span>
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
          <span className="field-label">Model (optional)</span>
          <input
            className="input"
            value={model}
            onChange={(e) => setModel(e.target.value)}
            placeholder="Uses your default model"
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
          Only make a plan — don't start any work
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={yes}
            onChange={(e) => setYes(e.target.checked)}
            disabled={planOnly}
          />
          Start without asking me to approve the plan
        </label>
      </div>

      {yes && !planOnly && (
        <Note tone="is-asserted">
          Wingman will plan and start working right away, without asking you first. This spends
          real money on your API key.
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
  const [tab, setTab] = useState<RunTab>('plan')

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

  async function control(
    action: ControlAction,
    body: { task?: string; message?: string } = {},
  ): Promise<boolean> {
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
      return true
    } catch (e) {
      setActionError(message(e))
      return false
    } finally {
      setBusy(null)
    }
  }

  if (error)
    return (
      <div className="view">
        <Failed
          title="Couldn't load this run"
          detail={error}
          action={{ label: 'Back to runs', onClick: () => navigate('/runs') }}
        />
      </div>
    )
  if (!run) return <Loading what="the run" />

  const gated = run.status === 'awaiting_approval'
  const terminal = run.status === 'done' || run.status === 'failed' || run.status === 'aborted'
  const irreversible = run.tasks.filter((t) => isIrreversible(t))

  const done = run.tasks.filter((t) => t.status === 'done').length
  const tabs: { id: RunTab; label: string; count?: number }[] = [
    { id: 'plan', label: 'Plan', count: run.tasks.length },
    { id: 'agents', label: 'Agents', count: run.agents.length },
    { id: 'activity', label: 'Activity', count: log.length || undefined },
    { id: 'log', label: 'Output' },
  ]

  return (
    <div className="view run-view">
      <button type="button" className="button button-quiet back" onClick={() => navigate('/runs')}>
        <Icon name="collapse" size={14} />
        All runs
      </button>

      <PageHead
        title={run.goal}
        intro={
          <span className="run-meta">
            <span className={`lamp ${live ? 'is-asserted' : terminal ? runClass(run.status) : ''}`} aria-hidden="true" />
            {live ? 'Updating live' : terminal ? 'Finished' : 'Not updating'}
            <span className="run-meta-id mono" title="Run id — use it with wingman pilot">
              {run.run_id}
            </span>
          </span>
        }
      />

      <dl className="context-strip">
        <div>
          <dt>Status</dt>
          <dd>
            <Pill status={runClass(run.status)} glyph={runGlyph(run.status)}>
              {runLabel(run.status)}
            </Pill>
          </dd>
        </div>
        <div>
          <dt>Progress</dt>
          <dd>
            {done} of {run.tasks.length} tasks
            <span className={`meter ${runClass(run.status)}`} aria-hidden="true">
              <span style={{ width: `${run.tasks.length ? (done / run.tasks.length) * 100 : 0}%` }} />
            </span>
          </dd>
        </div>
        <div>
          <dt>Spent</dt>
          <dd>{money(run.totals.usd)}</dd>
        </div>
        <div>
          <dt>Branch</dt>
          <dd className="mono truncate" title={run.integration_branch}>
            {run.integration_branch}
          </dd>
        </div>
        {run.subscription && (
          <div>
            <dt>Plan usage</dt>
            <dd>
              {Math.round(run.subscription.utilization * 100)}%
              <span className={`meter ${run.subscription.utilization >= 0.8 ? 'is-asserted' : ''}`} aria-hidden="true">
                <span style={{ width: `${Math.round(run.subscription.utilization * 100)}%` }} />
              </span>
            </dd>
          </div>
        )}
      </dl>

      <div className="run-layout">
        <div className="run-main">
          <div className="tabs" role="tablist" aria-label="Run sections">
            {tabs.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                id={`run-tab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls={`run-panel-${t.id}`}
                className="tab"
                onClick={() => setTab(t.id)}
              >
                {t.label}
                {t.count !== undefined && <span className="count">{t.count}</span>}
              </button>
            ))}
          </div>

          <div
            role="tabpanel"
            id={`run-panel-${tab}`}
            aria-labelledby={`run-tab-${tab}`}
            className="run-panel"
          >
            {tab === 'plan' &&
              (run.tasks.length === 0 ? (
                <p className="panel-empty">
                  Wingman is still planning. The tasks appear here as soon as the plan is ready.
                </p>
              ) : (
                <>
                  <p className="section-intro">
                    Indented tasks wait for the one above them. Tasks that touch different files run
                    at the same time.
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
                </>
              ))}
            {tab === 'agents' &&
              (run.agents.length > 0 ? (
                <Workers project={project} agents={run.agents} />
              ) : (
                <p className="panel-empty">No agents have started on this run yet.</p>
              ))}
            {tab === 'activity' && <RunLog events={log} live={live} />}
            {tab === 'log' && <PilotLogView project={project} runId={run.run_id} />}
          </div>
        </div>

        <aside className="run-aside">
          {gated && (
            <section className="gate">
              <h2>Your approval is needed</h2>
              <p>
                Wingman planned {run.tasks.length} {run.tasks.length === 1 ? 'task' : 'tasks'}.
                Nothing starts until you approve. Read the plan, then decide.
              </p>

              {/* The one fact the gate was missing. Pilot classifies every
                  task's reversibility and says why; approving a plan without
                  seeing which parts cannot be undone is approving the wrong
                  thing carefully. */}
              {irreversible.length > 0 && (
                <div className="gate-warn">
                  <p className="is-failed dot">
                    {irreversible.length === 1
                      ? "1 task can't be cleanly undone:"
                      : `${irreversible.length} tasks can't be cleanly undone:`}
                  </p>
                  <ul className="md-list">
                    {irreversible.map((t) => (
                      <li key={t.id}>
                        {t.title}
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
                  {busy === 'approve' ? 'Approving…' : 'Approve and start'}
                </button>
                <button
                  type="button"
                  className="button"
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
              Sent. The run picks it up on its next check.
              {sent === 'ask' && ' The reply appears under Activity.'}
            </Note>
          )}

          {!terminal && !gated && (
            <Steer
              busy={busy !== null}
              onSend={(action, text) => control(action, { message: text })}
            />
          )}

          <section className="panel run-facts">
            <h2>Details</h2>
            <div className="rows">
              <div className="row">
                <span className="muted">Tokens</span>
                <span className="figure">
                  {compact(run.totals.tokens_in)} in · {compact(run.totals.tokens_out)} out
                </span>
              </div>
              <div className="row row-wrap">
                <span className="muted">Started from</span>
                <span className="figure mono" title={run.base_commit}>
                  {run.base_commit ? run.base_commit.slice(0, 10) : '—'}
                </span>
              </div>
              {run.pr_url && (
                <div className="row row-wrap">
                  <span className="muted">Pull request</span>
                  <a className="figure" href={run.pr_url} target="_blank" rel="noreferrer">
                    {run.pr_url.replace(/^https?:\/\/(www\.)?github\.com\//, '')}
                  </a>
                </div>
              )}
            </div>
          </section>

          {/* At the bottom of the side column, away from the title. Stopping is
              the one irreversible thing this screen does, and a destructive
              control next to the title is one the hand finds before the eye
              has read what it would stop. */}
          {!terminal && (
            <button
              type="button"
              className="button button-quiet run-abort"
              disabled={busy !== null}
              onClick={() => {
                if (window.confirm('Stop this run? Agents stop after their current step.'))
                  void control('abort')
              }}
            >
              {busy === 'abort' ? 'Stopping…' : 'Stop this run'}
            </button>
          )}
        </aside>
      </div>
    </div>
  )
}

type RunTab = 'plan' | 'agents' | 'activity' | 'log'

/** 1234567 → "1.2M". Token counts are read for magnitude, not to the unit. */
function compact(n: number): string {
  return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(n)
}

/**
 * `pilot tell` / `pilot ask` from the browser. Every worker holding a task
 * hears it on its next turn — the CLI's `--task` targeting is left to the
 * terminal until someone needs it from a phone.
 */
function Steer({
  busy,
  onSend,
}: {
  busy: boolean
  onSend: (action: 'tell' | 'ask', text: string) => Promise<boolean>
}) {
  const [text, setText] = useState('')
  const empty = text.trim() === ''

  async function send(action: 'tell' | 'ask') {
    if (empty) return
    if (await onSend(action, text.trim())) setText('')
  }

  return (
    <form
      className="steer"
      onSubmit={(e) => {
        e.preventDefault()
        void send('tell')
      }}
    >
      <label className="sr-only" htmlFor="steer-message">
        Message to the run's workers
      </label>
      <textarea
        id="steer-message"
        className="input"
        rows={2}
        value={text}
        placeholder="Give the agents a hint, or ask them a question"
        onChange={(e) => setText(e.target.value)}
      />
      <div className="composer-tools">
        <span className="composer-hint">Every working agent reads it on its next step</span>
        <MicButton value={text} onChange={setText} disabled={busy} />
        <button
          type="button"
          className="button"
          disabled={busy || empty}
          onClick={() => void send('ask')}
        >
          Ask
        </button>
        <button type="submit" className="button button-primary" disabled={busy || empty}>
          Send
        </button>
      </div>
    </form>
  )
}

/* ── Workers ───────────────────────────────────────────────────────────── */

/**
 * Who ran, and what each one actually did.
 *
 * A worker's transcript is a session transcript: the orchestrator mints
 * `pilot-<run>-<agent>` and the worker writes
 * `<project>/.wingman/sessions/<that>.jsonl` like any other conversation, so
 * `GET /sessions/{id}` already serves it and `Transcript` already renders it.
 * The panel knew the id from the first release and never opened it — the one
 * screen that could show the work showed a name, a model and a figure.
 *
 * One at a time. Each transcript is a full conversation with a `<pre>` per
 * tool result, and fetching four because a run had four workers would pull
 * megabytes nobody asked to read.
 */
function Workers({ project, agents }: { project: string; agents: Agent[] }) {
  const [open, setOpen] = useState<string | null>(null)

  return (
    <>
      <p className="section-intro">
        Open an agent to read everything it did — each prompt, tool call and result.
      </p>
      <div className="rows">
        {agents.map((a) => {
          const isOpen = open === a.id
          return (
            <Fragment key={a.id}>
              <div className="row">
                <span className="worker-row">
                  <span className={`dot ${agentClass(a.status)}`} aria-hidden="true" />
                  <button
                    type="button"
                    className="row-link truncate"
                    aria-expanded={isOpen}
                    onClick={() => setOpen(isOpen ? null : a.id)}
                  >
                    {a.name || a.id}
                    <span className="muted"> · {a.role}</span>
                    {a.current_tool && <span className="muted"> · {a.current_tool}</span>}
                    {a.pid != null && <span className="faint"> · pid {a.pid}</span>}
                  </button>
                </span>
                <span className="figure">
                  <span className="muted">{a.model ?? '—'}</span> {money(a.usd)}
                </span>
              </div>
              {isOpen && <AgentWork project={project} agent={a} />}
            </Fragment>
          )
        })}
      </div>
    </>
  )
}

/**
 * One worker's transcript, fetched when it is opened.
 *
 * A missing transcript is reported as what it is rather than as a failure.
 * Workers only began opening the log their id already promised in
 * `fix(pilot): give a worker the session log its id already promised`, so
 * every run from before that names a file that was never written — and "no
 * transcript, here is why" is a better answer than a red error.
 */
function AgentWork({ project, agent }: { project: string; agent: Agent }) {
  const [records, setRecords] = useState<SessionRecord[] | null>(null)
  const [missing, setMissing] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!agent.session_id) return
    let live = true
    setError(null)
    setMissing(false)
    api
      .session(project, agent.session_id)
      .then((r) => live && setRecords(r.records))
      .catch((e: unknown) => {
        if (!live) return
        // A 404 is the expected shape of "this run predates worker logs", not
        // an error the reader can act on.
        if (/404|no such session/i.test(message(e))) setMissing(true)
        else setError(message(e))
      })
    return () => {
      live = false
    }
  }, [project, agent.session_id])

  if (!agent.session_id)
    return (
      <p className="worker-note muted figure">
        This worker never recorded a session id, so there is no transcript to open.
      </p>
    )

  if (missing)
    return (
      <p className="worker-note muted figure">
        No transcript on disk for <span className="identifier">{agent.session_id}</span>. Workers
        only started writing the log their id promised recently, so runs from before that have
        events but no conversation. The activity log below still has every tool this run ran.
      </p>
    )

  if (error) return <p className="worker-note is-failed dot figure">{error}</p>
  if (!records) return <Loading what="the transcript" />
  if (records.length === 0)
    return <p className="worker-note muted figure">This transcript is empty.</p>

  return (
    <div className="worker-transcript transcript">
      <Transcript records={records} />
    </div>
  )
}

/* ── Run log ───────────────────────────────────────────────────────────── */

/**
 * The orchestrator's own stdout.
 *
 * `tasks.jsonl` records what the run did; this is what it said — the plan it
 * costed, why it asked for approval or did not, and what it exited on. Every
 * failed run on this machine is explained here and nowhere else, which is
 * exactly the thing a run screen was missing.
 */
function PilotLogView({ project, runId }: { project: string; runId: string }) {
  const [log, setLog] = useState<PilotLog | null>(null)

  useEffect(() => {
    let live = true
    api
      .runLog(project, runId)
      .then((l) => live && setLog(l))
      .catch(() => {
        /* The log is an extra; the plan above is the run. */
      })
    return () => {
      live = false
    }
  }, [project, runId])

  if (!log) return <Loading what="the output" />
  if (log.total_lines === 0) return <p className="panel-empty">Nothing was printed for this run.</p>
  const truncated = log.shown_lines < log.total_lines

  return (
    <>
      <p className="section-intro">
        What Wingman printed while running this goal.{' '}
        {truncated
          ? `The last ${log.shown_lines.toLocaleString()} of ${log.total_lines.toLocaleString()} lines.`
          : `All ${log.total_lines.toLocaleString()} ${log.total_lines === 1 ? 'line' : 'lines'}.`}
      </p>
      <Report text={log.text} />
    </>
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
  if (events.length === 0)
    return <p className="panel-empty">No activity yet. Steps appear here as agents take them.</p>
  return (
    <>
      <p className="section-intro">
        {live ? 'Newest first, updating live.' : 'Newest first.'}{' '}
        {events.length === 200 ? 'Showing the last 200 steps.' : `${events.length} steps.`}
      </p>
      <ol className="timeline">
        {[...events].reverse().map((e, i) => (
          <li key={`${e.t}-${i}`}>
            <span className="timeline-time figure">{clockOf(e.t)}</span>
            <span className="timeline-text">{summarise(e)}</span>
          </li>
        ))}
      </ol>
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
      // The reply half of `ask` rides the event log as a tool line.
      if (s('tool').startsWith('worker_msg:')) {
        try {
          const m = JSON.parse(s('tool').slice('worker_msg:'.length)) as { msg?: string; text?: string }
          if (m.msg === 'answer' || m.msg === 'question') return `${s('id')} ${m.msg}: ${m.text ?? ''}`
        } catch {
          /* Not the shape pilot writes. Falls through to the plain line. */
        }
      }
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
export function runGlyph(status: RunStatus): string {
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

/** The run's state in words a newcomer reads without the source open. */
export function runLabel(status: RunStatus): string {
  switch (status) {
    case 'planning':
      return 'Planning'
    case 'awaiting_approval':
      return 'Needs approval'
    case 'running':
      return 'Running'
    case 'merging':
      return 'Merging'
    case 'done':
      return 'Done'
    case 'failed':
      return 'Failed'
    case 'aborted':
      return 'Stopped'
  }
}

export function runClass(status: RunStatus): string {
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
