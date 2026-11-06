/**
 * A fake `wingman serve` for `npm run dev:mock`, so the panel can be designed
 * in a browser with no daemon and no Rust build — the panel's twin of the
 * notifier's `demo.ts`.
 *
 * Only wired in by `vite.config.ts` under `--mode mock`; the production build
 * never imports it. State is in memory and resets on restart.
 *
 * The data is deliberately uneven: a busy repo, a repo whose name is far too
 * long and which has never been indexed, and an empty repo — plus failures,
 * a gated run, an irreversible task, huge labels lists and an erroring report,
 * because layout problems live at the edges, not in the happy path.
 */
import type { Plugin } from 'vite'
import type {
  Agent,
  BoardData,
  Card,
  ColumnId,
  RunState,
  RunStatus,
  SessionRecord,
  Task,
  TaskStatus,
  WingmanNotification,
} from './src/api'

/* ── Time ─────────────────────────────────────────────────────────────── */

const NOW = Date.now()
const iso = (minsAgo: number) => new Date(NOW - minsAgo * 60_000).toISOString().replace(/\.\d+Z$/, 'Z')
const unix = (minsAgo: number) => Math.floor((NOW - minsAgo * 60_000) / 1000)

/* ── Projects ─────────────────────────────────────────────────────────── */

const LONG = 'acme-billing-service-platform-monorepo-legacy-do-not-touch'
const projects = [
  { id: 'wingman', root: 'C:/Users/dev/Projects/Wingman', branch: 'feat/notifier-redesign', indexd_running: true, index_age_secs: 420 },
  { id: LONG, root: `/srv/checkouts/${LONG}`, branch: null, indexd_running: false, index_age_secs: null },
  { id: 'scratch', root: 'C:/Users/dev/scratch', branch: 'main', indexd_running: false, index_age_secs: 86_400 * 12 },
]

/* ── Runs ─────────────────────────────────────────────────────────────── */

const AGENTS = ['brave_turing', 'quiet_hopper', 'eager_lovelace', 'sleepy_knuth', 'jolly_ritchie', 'stoic_liskov']

type TaskSpec = [id: string, title: string, status: TaskStatus, extra?: Partial<Task>]

function tasks(specs: TaskSpec[], startedMinsAgo: number): Task[] {
  return specs.map(([id, title, status, extra], i) => {
    const started = status !== 'pending' && status !== 'todo'
    const ended = status === 'done' || status === 'failed'
    return {
      id,
      role: i === 0 ? 'architect' : i % 3 === 0 ? 'tester' : 'implementer',
      title,
      goal: `${title}. Keep the change inside the declared write-set and leave the tests green.`,
      deps: i === 0 ? [] : [specs[Math.max(0, i - 2)][0]],
      writes: [`crates/wingman-config/src/${id}.rs`, 'crates/wingman-config/tests/inbox.rs'],
      acceptance: ['cargo test -p wingman-config passes', 'no new clippy warnings'],
      reversibility: 'reversible',
      reversibility_reason: null,
      status,
      agent: started ? AGENTS[i % AGENTS.length] : null,
      worktree: started ? `.wingman/worktrees/${id}` : null,
      usd: started ? +(0.08 + i * 0.13).toFixed(2) : 0,
      commits: status === 'done' ? [`a${i}f3c9e2d41b07e5c8a1`] : [],
      outcome:
        status === 'done'
          ? { summary: `Implemented ${title.toLowerCase()}; 4 tests added.`, files_changed: [`crates/wingman-config/src/${id}.rs`] }
          : status === 'failed'
            ? { summary: 'cargo test failed: 3 assertions in wingman-config::inbox (expected 2 entries, got 3)', files_changed: [] }
            : null,
      started_at: started ? iso(startedMinsAgo - i * 3) : null,
      ended_at: ended ? iso(startedMinsAgo - i * 3 - 6) : null,
      attempts: status === 'failed' ? 2 : started ? 1 : 0,
      ...extra,
    }
  })
}

function agents(ts: Task[], terminal: boolean): Agent[] {
  return ts
    .filter((t) => t.agent)
    .map((t, i) => ({
      id: `agent-${i + 1}`,
      name: t.agent!,
      role: t.role,
      current_task: t.status === 'in_progress' ? t.id : null,
      current_tool: t.status === 'in_progress' ? (i % 2 ? 'edit_file' : 'bash: cargo test -p wingman-config') : null,
      pid: t.status === 'in_progress' && !terminal ? 40_000 + i : null,
      status: t.status === 'in_progress' ? 'in_progress' : t.status === 'failed' ? 'failed' : 'done',
      session_id: `20261106T09${10 + i}00000Z`,
      spawned_at: t.started_at,
      model: i % 2 ? 'claude-sonnet-5-5' : 'claude-opus-5-5',
      usd: t.usd,
    }))
}

function run(project: string, run_id: string, goal: string, status: RunStatus, ts: Task[], extra: Partial<RunState> = {}): RunState {
  const terminal = status === 'done' || status === 'failed' || status === 'aborted'
  const usd = +ts.reduce((n, t) => n + t.usd, 0).toFixed(2)
  return {
    run_id,
    goal,
    base_commit: 'a6d5cea1f0e9b8c7d6e5f4a3b2c1d0e9f8a7b6c5',
    integration_branch: `wingman/run-${run_id.slice(-6)}`,
    status,
    tasks: ts,
    agents: agents(ts, terminal),
    totals: { usd, tokens_in: Math.round(usd * 180_000), tokens_out: Math.round(usd * 21_000) },
    pr_url: null,
    ...extra,
    // Not part of RunState — the project the run belongs to, stripped on the wire.
    ...({ project } as object),
  }
}

const runs: RunState[] = [
  run('wingman', '2026-11-06-0912-k3j9qa', 'Wire the config parser into the inbox', 'running',
    tasks([
      ['t1', 'Sketch the inbox schema', 'done'],
      ['t2', 'Parse `[pilot.notifications]` routing table', 'done'],
      ['t3', 'Thread the parsed routes into the notifier', 'done'],
      ['t4', 'Dedupe repeated escalations within a window', 'in_progress'],
      ['t5', 'Expire decision cards after their deadline', 'in_progress'],
      ['t6', 'Integration tests for the reply path', 'blocked'],
      ['t7', 'Document the routing table in docs/CONFIG.md', 'todo'],
    ], 48),
    { subscription: { utilization: 0.72, resets_at: unix(-95) } }),
  run('wingman', '2026-11-06-0841-p0x2mz', 'Add actionable desktop notifications', 'awaiting_approval',
    tasks([
      ['t1', 'Design the card wire format', 'pending'],
      ['t2', 'Tauri window that stacks cards bottom-right', 'pending'],
      ['t3', 'Approve / veto buttons route to control.jsonl', 'pending'],
      ['t4', 'Free-text answers for ask_user', 'pending'],
      ['t5', 'Drop the legacy notifications table', 'pending', { reversibility: 'irreversible', reversibility_reason: 'drops `notifications` from learn.db; there is no down-migration' }],
      ['t6', 'Panel renders the same inbox', 'pending'],
      ['t7', 'Docs and screenshots', 'pending'],
    ], 0)),
  run('wingman', '2026-11-05-1730-f9a1cc', 'Migrate session store to SQLite', 'failed',
    tasks([
      ['t1', 'Schema for sessions and records', 'done'],
      ['t2', 'Write path behind a feature flag', 'done'],
      ['t3', 'Backfill existing .jsonl transcripts', 'failed'],
      ['t4', 'Switch the read path', 'blocked'],
    ], 900)),
  run('wingman', '2026-11-05-1102-qq81vd', 'Rewrite the cost ladder chart', 'merging',
    tasks([
      ['t1', 'Log-scale ladder with rung labels', 'done'],
      ['t2', 'Compare row against other models', 'done'],
      ['t3', 'Snapshot tests for the chart', 'review'],
    ], 1300)),
  run('wingman', '2026-11-04-1615-ab12cd', 'Pair a browser with a QR code', 'done',
    tasks([
      ['t1', 'Pairing code endpoint', 'done'],
      ['t2', 'QR rendering in the TUI', 'done'],
      ['t3', 'Redeem flow in the panel', 'done'],
      ['t4', 'Rate-limit redeem attempts', 'done'],
    ], 2800),
    { pr_url: 'https://github.com/vedantnimbarte/Wingman/pull/261' }),
  run('wingman', '2026-11-03-2210-zz9y8x', 'Experimental: voice input for the composer', 'aborted',
    tasks([
      ['t1', 'Spike: Web Speech API in the composer', 'done'],
      ['t2', 'Push-to-talk shortcut', 'in_progress'],
    ], 4400)),
  run('wingman', '2026-11-06-0955-plan01', 'Audit unused dependencies across every crate and the two web UIs, then remove what nothing imports', 'planning', []),
  run(LONG, '2026-10-30-0900-inv001', 'Fix invoice rounding for currencies with three decimal places', 'done',
    tasks([
      ['t1', 'Reproduce with KWD and BHD fixtures', 'done'],
      ['t2', 'Use minor units from ISO 4217', 'done'],
    ], 10_000)),
]

const projectOf = (r: RunState) => (r as unknown as { project: string }).project
const wire = (r: RunState): RunState => {
  const { project: _, ...rest } = r as RunState & { project: string }
  return rest
}

/* ── Board ────────────────────────────────────────────────────────────── */

type CardRow = {
  id: string
  title: string
  goal: string
  notes: string | null
  labels: string[]
  archived: boolean
  created_at: string
  project: string
  run_id: string | null
  missing?: boolean
  dispatches: string[]
}

let nextCard = 100
const cards: CardRow[] = [
  { id: 'c01', title: 'Dark-mode contrast pass on the board', goal: 'Every status hue meets 4.5:1 on both themes.', notes: null, labels: ['ui', 'a11y'], archived: false, created_at: iso(60 * 30), project: 'wingman', run_id: null, dispatches: [] },
  { id: 'c02', title: 'Investigate why the semantic index sometimes double-counts files that were renamed across a branch switch while indexd was paused and the watcher missed the event', goal: 'Find the root cause; a fix is a separate card.', notes: 'Seen twice on Windows only. Possibly the OneDrive sync layer renaming in two steps.', labels: ['indexing', 'bug', 'p2', 'needs-repro', 'windows', 'rag'], archived: false, created_at: iso(60 * 50), project: 'wingman', run_id: null, dispatches: [] },
  { id: 'c03', title: 'Bump tokio and axum', goal: '', notes: null, labels: [], archived: false, created_at: iso(60 * 72), project: 'gone-repo', run_id: null, missing: true, dispatches: [] },
  { id: 'c04', title: 'Add actionable desktop notifications', goal: 'Cards the user can answer without opening the panel.', notes: null, labels: ['notifier'], archived: false, created_at: iso(200), project: 'wingman', run_id: '2026-11-06-0841-p0x2mz', dispatches: ['2026-11-06-0841-p0x2mz'] },
  { id: 'c05', title: 'Audit unused dependencies', goal: 'Remove what nothing imports.', notes: null, labels: ['chore'], archived: false, created_at: iso(30), project: 'wingman', run_id: '2026-11-06-0955-plan01', dispatches: ['2026-11-06-0955-plan01'] },
  { id: 'c06', title: 'Wire the config parser into the inbox', goal: 'Notifications are routed by `[pilot.notifications]`.', notes: null, labels: ['notifier', 'config'], archived: false, created_at: iso(120), project: 'wingman', run_id: '2026-11-06-0912-k3j9qa', dispatches: ['2026-11-06-0912-k3j9qa'] },
  { id: 'c07', title: 'Migrate session store to SQLite', goal: 'Transcripts in one file, queryable.', notes: null, labels: ['sessions'], archived: false, created_at: iso(60 * 20), project: 'wingman', run_id: '2026-11-05-1730-f9a1cc', dispatches: ['2026-11-04-1000-old001', '2026-11-05-1730-f9a1cc'] },
  { id: 'c08', title: 'Rewrite the cost ladder chart', goal: '', notes: null, labels: ['insights', 'ui'], archived: false, created_at: iso(60 * 26), project: 'wingman', run_id: '2026-11-05-1102-qq81vd', dispatches: ['2026-11-05-1102-qq81vd'] },
  { id: 'c09', title: 'Pair a browser with a QR code', goal: '', notes: null, labels: ['auth'], archived: false, created_at: iso(60 * 50), project: 'wingman', run_id: '2026-11-04-1615-ab12cd', dispatches: ['2026-11-04-1615-ab12cd'] },
  { id: 'c10', title: 'Experimental: voice input', goal: '', notes: null, labels: ['spike'], archived: false, created_at: iso(60 * 75), project: 'wingman', run_id: '2026-11-03-2210-zz9y8x', dispatches: ['2026-11-03-2210-zz9y8x'] },
  { id: 'c11', title: 'Fix invoice rounding', goal: '', notes: null, labels: ['billing'], archived: false, created_at: iso(60 * 170), project: LONG, run_id: '2026-10-30-0900-inv001', dispatches: ['2026-10-30-0900-inv001'] },
  { id: 'c12', title: 'Old idea: Electron shell', goal: '', notes: null, labels: [], archived: true, created_at: iso(60 * 400), project: 'wingman', run_id: null, dispatches: [] },
  { id: 'c13', title: 'Try a vendored ripgrep', goal: '', notes: null, labels: ['perf'], archived: true, created_at: iso(60 * 500), project: 'wingman', run_id: null, dispatches: [] },
]

const COLUMNS: { id: ColumnId; title: string }[] = [
  { id: 'backlog', title: 'Backlog' },
  { id: 'planned', title: 'Planned' },
  { id: 'in-progress', title: 'In progress' },
  { id: 'review', title: 'Review' },
  { id: 'done', title: 'Done' },
]

function columnOf(s: RunStatus): ColumnId {
  if (s === 'planning' || s === 'awaiting_approval') return 'planned'
  if (s === 'running' || s === 'failed') return 'in-progress'
  if (s === 'merging') return 'review'
  return 'done'
}

function cardJson(c: CardRow): Card {
  const r = c.archived ? undefined : runs.find((x) => x.run_id === c.run_id)
  const count = (s: TaskStatus) => r?.tasks.filter((t) => t.status === s).length ?? 0
  const rollup = r && {
    status: r.status,
    done: count('done'),
    total: r.tasks.length,
    failed: count('failed'),
    blocked: count('blocked'),
    review: count('review'),
    in_progress: count('in_progress'),
    not_started: count('pending') + count('todo'),
    usd: r.totals.usd,
    subrows: r.tasks.map((t) => ({
      task_id: t.id,
      title: t.title,
      status: t.status,
      role: t.role,
      agent_name: t.agent,
      model: r.agents.find((a) => a.name === t.agent)?.model ?? null,
      session_id: r.agents.find((a) => a.name === t.agent)?.session_id ?? null,
      usd: t.usd,
      attempts: t.attempts,
      blocked_by: t.status === 'blocked' ? t.deps : [],
      current_tool: r.agents.find((a) => a.current_task === t.id)?.current_tool ?? null,
      deps: t.deps,
      writes: t.writes.length,
      elapsed_secs: t.started_at ? Math.round((Date.parse(t.ended_at ?? new Date().toISOString()) - Date.parse(t.started_at)) / 1000) : null,
      outcome: t.outcome?.summary ?? null,
      worktree: t.worktree,
    })),
  }
  const badges: Card['badges'] = []
  if (c.missing) badges.push({ kind: 'missing', text: 'project missing' })
  if (rollup) {
    if (rollup.total) badges.push({ kind: 'progress', text: `${rollup.done}/${rollup.total}` })
    if (rollup.usd) badges.push({ kind: 'cost', text: `$${rollup.usd.toFixed(2)}` })
    if (rollup.failed) badges.push({ kind: 'failed', text: `${rollup.failed} failed` })
    if (rollup.blocked) badges.push({ kind: 'blocked', text: `${rollup.blocked} blocked` })
    if (r!.status === 'aborted') badges.push({ kind: 'aborted', text: 'aborted' })
  }
  if (c.dispatches.length > 1) badges.push({ kind: 'retry', text: `attempt ${c.dispatches.length}` })
  c.labels.slice(0, 3).forEach((l) => badges.push({ kind: 'label', text: l }))
  if (c.labels.length > 3) badges.push({ kind: 'more_labels', text: `+${c.labels.length - 3}` })

  return {
    id: c.id,
    short: c.id.replace('c', '#'),
    title: c.title,
    goal: c.goal,
    notes: c.notes,
    labels: c.labels,
    archived: c.archived,
    created_at: c.created_at,
    project: c.project,
    project_name: c.project,
    project_missing: Boolean(c.missing),
    column: r ? columnOf(r.status) : 'backlog',
    run_id: c.run_id,
    badges,
    rollup: rollup ?? null,
  }
}

/* ── Run events ───────────────────────────────────────────────────────── */

function events(r: RunState) {
  const out: Record<string, unknown>[] = [{ ev: 'run.start', t: iso(50), goal: r.goal }]
  for (const t of r.tasks) out.push({ ev: 'task.create', t: t.started_at ?? iso(49), id: t.id, title: t.title })
  for (const a of r.agents) out.push({ ev: 'agent.spawn', t: a.spawned_at ?? iso(45), agent: a.name, role: a.role })
  for (const t of r.tasks.filter((x) => x.agent)) {
    out.push({ ev: 'task.assign', t: t.started_at!, id: t.id, agent: t.agent })
    out.push({ ev: 'task.status', t: t.started_at!, id: t.id, status: 'in_progress' })
    out.push({ ev: 'task.tool', t: t.started_at!, id: t.id, tool: 'read_file', file: t.writes[0], ok: true })
    out.push({ ev: 'task.tool', t: t.started_at!, id: t.id, tool: 'bash: cargo test -p wingman-config', ok: t.status !== 'failed' })
    if (t.commits[0]) out.push({ ev: 'task.commit', t: t.ended_at!, id: t.id, sha: t.commits[0] })
    if (t.status !== 'in_progress') out.push({ ev: 'task.status', t: t.ended_at ?? t.started_at!, id: t.id, status: t.status })
  }
  if (r.status === 'running') {
    out.push({ ev: 'task.tool', t: iso(2), id: 't4', tool: 'worker_msg:{"msg":"question","text":"Should duplicate escalations inside the window bump the original card or be dropped?"}' })
    out.push({ ev: 'budget.warn', t: iso(1), id: '' })
  }
  return out
}

/* ── Sessions ─────────────────────────────────────────────────────────── */

type SessionRow = { session_id: string; first_prompt: string | null; model: string | null; provider: string | null; mtime: number; project: string; records: SessionRecord[] }

const RICH: SessionRecord[] = [
  { kind: 'session_start', ts: iso(90), model: 'claude-opus-5-5', provider: 'claude-code' },
  { kind: 'user', ts: iso(90), text: 'Why does `wingman board` show the card in Review while the run says merging?' },
  {
    kind: 'assistant', ts: iso(89), blocks: [
      { type: 'thinking', text: 'The column is derived server-side from the run status. Check `wingman_board::column_for`.' },
      { type: 'text', text: 'Let me look at how the column is derived.' },
      { type: 'tool_use', id: 'tu1', name: 'read_file', input: { path: 'crates/wingman-board/src/lib.rs', offset: 120, limit: 40 } },
    ],
  },
  { kind: 'tool_result', ts: iso(89), id: 'tu1', output: 'pub fn column_for(status: RunStatus) -> Column {\n    match status {\n        RunStatus::Merging => Column::Review,\n        ...\n    }\n}', is_error: false },
  {
    kind: 'assistant', ts: iso(88), blocks: [
      { type: 'tool_use', id: 'tu2', name: 'bash', input: { command: 'cargo test -p wingman-board column' } },
    ],
  },
  { kind: 'tool_result', ts: iso(88), id: 'tu2', output: 'error[E0425]: cannot find value `Merged` in enum `RunStatus`\n --> crates/wingman-board/src/lib.rs:131:20\n\nerror: could not compile `wingman-board` (lib test) due to 1 previous error', is_error: true },
  {
    kind: 'assistant', ts: iso(87), blocks: [
      {
        type: 'text',
        text: [
          'That is intentional. **Merging** means every task passed and the integration branch is being merged — the work is ready to look at, so the board files it under *Review*.',
          '',
          '| Run status | Board column |',
          '| --- | --- |',
          '| `planning`, `awaiting_approval` | Planned |',
          '| `running`, `failed` | In progress |',
          '| `merging` | Review |',
          '| `done`, `aborted` | Done |',
          '',
          'If you want it to read differently, the one place to change is:',
          '',
          '```rust',
          'RunStatus::Merging => Column::Review,',
          '```',
          '',
          '1. Change the arm',
          '2. Update the snapshot in `tests/board.rs`',
          '3. The panel follows automatically — it never derives columns itself.',
        ].join('\n'),
      },
    ],
  },
  { kind: 'usage_delta', ts: iso(87), usage: { input_tokens: 18_204, output_tokens: 1_122, cache_read_tokens: 12_000 } },
  { kind: 'stop', ts: iso(87), reason: 'end_turn' },
  { kind: 'user', ts: iso(70), text: 'ok leave it. can you tighten the card padding instead?' },
  { kind: 'assistant', ts: iso(69), blocks: [{ type: 'text', text: 'Done — padding is now `0.625rem 0.75rem` and the badges wrap instead of overflowing.' }, { type: 'tool_use', id: 'tu3', name: 'edit_file', input: { path: 'panel/src/app.css' } }] },
  { kind: 'tool_result', ts: iso(69), id: 'tu3', output: 'Edited panel/src/app.css (+3 −3)', is_error: false },
  { kind: 'stop', ts: iso(69), reason: 'end_turn' },
]

const simple = (prompt: string, answer: string, minsAgo: number, model = 'claude-sonnet-5-5', provider = 'anthropic'): SessionRecord[] => [
  { kind: 'session_start', ts: iso(minsAgo), model, provider },
  { kind: 'user', ts: iso(minsAgo), text: prompt },
  { kind: 'assistant', ts: iso(minsAgo - 1), blocks: [{ type: 'text', text: answer }] },
  { kind: 'stop', ts: iso(minsAgo - 1), reason: 'end_turn' },
]

const sessions: SessionRow[] = [
  { session_id: '20261106T091500000Z', first_prompt: 'Why does `wingman board` show the card in Review while the run says merging?', model: 'claude-opus-5-5', provider: 'claude-code', mtime: unix(69), project: 'wingman', records: RICH },
  { session_id: '20261106T080200000Z', first_prompt: 'Summarise what changed on this branch since main, grouped by crate, and flag anything that touches the auth path or the config parser because those need a second reviewer before merge', model: 'claude-sonnet-5-5', provider: 'anthropic', mtime: unix(150), project: 'wingman', records: simple('Summarise what changed on this branch since main, grouped by crate…', '- **wingman-config**: inbox routing table\n- **wingman-cli**: `serve` notifications routes\n- **panel**: notification stack\n\nNothing touches auth.', 150) },
  { session_id: '20261105T221000000Z', first_prompt: 'write a regex for semver', model: 'qwen3-coder:30b', provider: 'ollama', mtime: unix(60 * 12), project: 'wingman', records: simple('write a regex for semver', '```\n^(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)(?:-[\\w.]+)?(?:\\+[\\w.]+)?$\n```', 60 * 12, 'qwen3-coder:30b', 'ollama') },
  { session_id: '20261104T140000000Z', first_prompt: null, model: null, provider: null, mtime: unix(60 * 50), project: 'wingman', records: [] },
  { session_id: '20261101T100000000Z', first_prompt: 'Explain the write-set scheduler', model: 'claude-haiku-5-5', provider: 'anthropic', mtime: unix(60 * 24 * 5), project: 'wingman', records: simple('Explain the write-set scheduler', 'Two tasks may run at once only when the paths they declared they would write do not overlap.', 60 * 24 * 5, 'claude-haiku-5-5') },
  { session_id: '20261030T090000000Z', first_prompt: 'Where is rounding done for invoices?', model: 'claude-sonnet-5-5', provider: 'anthropic', mtime: unix(60 * 24 * 7), project: LONG, records: simple('Where is rounding done for invoices?', 'In `billing/money.py::round_minor`.', 60 * 24 * 7) },
]

const turns = (s: SessionRow) => s.records.filter((r) => r.kind === 'user').length

/* ── Notifications ────────────────────────────────────────────────────── */

const RUN_DIR = 'C:/Users/dev/Projects/Wingman/.wingman/autonomous/2026-11-06-0912-k3j9qa'
let notifications: WingmanNotification[] = [
  { id: 'n-fail', severity: 'escalation', title: 'Task failed — Backfill existing .jsonl transcripts', body: 'cargo test failed: 3 assertions in wingman-session::backfill', project: 'wingman', run_dir: RUN_DIR, created_at: unix(12), expires_at: null, actions: [], free_text: false },
  { id: 'n-gate', severity: 'decision', title: 'Plan awaiting approval — 7 tasks', body: 'Add actionable desktop notifications\n\nest. $2.40 — 1 irreversible task', project: 'wingman', run_dir: RUN_DIR, created_at: unix(8), expires_at: unix(-52), actions: [{ id: 'approve', label: 'Approve', control: { cmd: 'approve' } }, { id: 'veto', label: 'Veto', control: { cmd: 'veto' } }], free_text: false },
  { id: 'n-ask', severity: 'decision', title: 'brave_turing is asking', body: 'Should duplicate escalations inside the window bump the original card or be dropped?', project: 'wingman', run_dir: RUN_DIR, created_at: unix(2), expires_at: unix(-58), actions: [{ id: 'bump', label: 'bump' }, { id: 'drop', label: 'drop' }], free_text: true },
  { id: 'n-prog', severity: 'progress', title: 'Run finished — Pair a browser with a QR code', body: '4/4 tasks · $3.12 · PR #261 opened', project: 'wingman', run_dir: null, created_at: unix(30), expires_at: null, actions: [], free_text: false },
]

/* ── Reports & commands ───────────────────────────────────────────────── */

const ESC = String.fromCharCode(27)
const green = (s: string) => `${ESC}[32m${s}${ESC}[0m`
const red = (s: string) => `${ESC}[31m${s}${ESC}[0m`
const out = (stdout: string, exit = 0, stderr = '') => ({ stdout, stderr, exit })

function report(tail: string, q: URLSearchParams): [number, unknown] {
  switch (tail) {
    case 'diff': {
      const file = q.get('file')
      if (!file) return [500, out('', 2, 'error: the following required arguments were not provided:\n  <FILE>\n\nUsage: wingman diff <FILE>')]
      return [200, out([
        `=== ${file} -> ${file} (2 hunk(s)) ===`,
        '--- hunk 1/2 @ -31,6 +31,9 ---',
        '   close: \'M6 6l12 12M18 6L6 18\',',
        green('+  changes: \'M4 6h16M4 12h10M4 18h7\','),
        green('+  // Same weight as the others so the rail reads as one set.'),
        green('+  bell: \'M6 8a6 6 0 1112 0c0 7 3 9 3 9H3s3-2 3-9\','),
        ' } as const',
        '--- hunk 2/2 @ -88,7 +91,7 @@ ---',
        ' export function Icon({ name, size = 16 }: Props) {',
        red('-  return <svg width={size} height={size} viewBox="0 0 24 24">'),
        green('+  return <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">'),
        '     <path d={PATHS[name]} />',
        '[a]ccept / [r]eject / [s]kip / [q]uit / [?] help: (quitting ...)',
        'done: accepted 0, rejected 0, files written 0',
      ].join('\n'))]
    }
    case 'explain':
      return [200, out('This branch adds an actionable notification inbox.\n\n• wingman-config parses a new [pilot.notifications] table that routes each severity to desktop, panel, webhook or nowhere.\n• wingman-cli gains GET /v1/notifications and POST /v1/notifications/{id}/reply.\n• The panel renders the inbox as a stack in the corner and can raise browser notifications for cards that wait on a person.\n\nRisk: the reply route writes control.jsonl when a button carries a control, so an "Approve" click can start a run.')]
    case 'review':
      return [200, out('Review of feat/notifier-redesign against main\n\nBLOCKER  crates/wingman-cli/src/serve/notifications.rs:88\n  The reply handler trusts `action` from the body without checking it is one of the card\'s actions.\n\nWARN     panel/src/Notifications.tsx:95\n  Polling every 3s while the tab is hidden wastes battery; pause on visibilitychange.\n\nNIT      desktop/notifier/ui/src/Card.tsx:40\n  `expires_at` countdown re-renders the whole stack every second.\n\n1 blocker, 1 warning, 1 nit')]
    case 'attest':
      return [500, out('Egress attestation — last 24h\n\n  api.anthropic.com        412 requests   ok (provider)\n  127.0.0.1:11434           38 requests   ok (ollama)\n  hooks.slack.com            3 requests   ok (webhook, configured)\n  telemetry.example.net      1 request    UNEXPECTED — not in any allowlist\n\nposture: NOT CLEAN (1 unexpected destination)', 1)]
    case 'memory':
      return [200, { files: 7, bytes: 18_442, entries: [{ name: 'MEMORY.md', bytes: 2_210 }, { name: 'build.md', bytes: 4_080 }, { name: 'style.md', bytes: 1_902 }] }]
    case 'index/status':
      return [200, { chunks: 18_903, files: 1_204, age_secs: 420, model: 'nomic-embed-text', stale_files: 3 }]
    case 'doctor':
      return [500, out('✓ git 2.47.0\n✓ cargo 1.91.0\n✓ node 22.20.1\n✗ ollama not reachable at 127.0.0.1:11434\n! indexd not running for this repo\n\n2 problems', 1)]
    case 'skills':
      return [200, out('NAME              SOURCE    DESCRIPTION\nrelease           repo      Cut a release: bump, changelog, tag\nfix-ci            user      Read the failing job and fix it\nreview-sql        plugin    Check migrations for locking issues')]
    case 'schedule':
      return [200, out('')]
    case 'mcp':
      return [502, { error: 'mcp server "linear" exited during startup: invalid token' }]
    default:
      return [404, { error: `no such route: ${tail}` }]
  }
}

const SCHEMA = {
  version: '0.5.0',
  ceiling: 'write',
  routes: [
    ...['memory', 'index/status', 'doctor', 'skills', 'schedule', 'mcp'].map((t) => ({
      method: 'GET', path: `/v1/projects/{project}/${t}`, about: {
        memory: 'memory files and their sizes', 'index/status': 'semantic index freshness', doctor: 'check this machine for problems',
        skills: 'skills available in this repo', schedule: 'scheduled prompts', mcp: 'configured MCP servers and their tools',
      }[t], runs: `wingman ${t.replace('/', ' ')}`,
    })),
    { method: 'GET', path: '/v1/projects/{project}/cost', about: 'spend by model' },
    { method: 'GET', path: '/v1/projects/{project}/diff', about: 'hunks for one file' },
  ],
}

/* ── Config ───────────────────────────────────────────────────────────── */

let config: Record<string, unknown> = {
  default_provider: 'claude-code',
  default_model: 'claude-opus-5-5',
  permission_mode: 'ask',
  reasoning: 'medium',
  providers: { anthropic: { api_key: '<redacted>', base_url: null }, ollama: { base_url: 'http://127.0.0.1:11434' } },
  tui: { theme: 'auto', vim_mode: false, show_thinking: true },
  tokens: { max_output: 16_000, compact_at: 0.8 },
  logging: { level: 'info', file: '~/.wingman/logs/wingman.log' },
  tools: { allow: ['read_file', 'grep', 'bash:cargo *'], deny: ['bash:rm -rf *'] },
  pilot: { max_parallel: 3, budget_usd: 10, notifications: { escalation: 'desktop', decision: 'desktop', progress: 'panel', info: 'none' } },
  serve: { addr: '127.0.0.1:8787', token: '<redacted>' },
}

const str = (description: string, extra: object = {}) => ({ type: 'string', description, ...extra })
const CONFIG_SCHEMA = {
  schema: {
    type: 'object',
    properties: {
      default_provider: str('Provider used when a command does not name one.', { enum: ['anthropic', 'claude-code', 'ollama', 'openai'] }),
      default_model: str('Model used when a command does not name one.'),
      permission_mode: str('What a tool call needs before it runs.', { enum: ['ask', 'auto', 'plan', 'yolo'] }),
      reasoning: str('How hard the model thinks by default.', { enum: ['low', 'medium', 'high'] }),
      providers: { type: 'object', description: 'Credentials and endpoints per provider.', additionalProperties: { $ref: '#/definitions/Provider' } },
      tui: { type: 'object', description: 'Terminal UI.', properties: {
        theme: str('Colour theme.', { enum: ['auto', 'light', 'dark'] }),
        vim_mode: { type: 'boolean', description: 'Vim keybindings in the composer.' },
        show_thinking: { type: 'boolean', description: 'Render thinking blocks inline instead of collapsed.' },
      } },
      tokens: { type: 'object', description: 'Token budgets.', properties: {
        max_output: { type: 'integer', description: 'Hard cap on output tokens per turn.' },
        compact_at: { type: 'number', description: 'Fraction of the context window at which the transcript is compacted. A long description to check wrapping: compaction summarises older turns into one message, keeps every tool result that a later turn referenced, and never drops the system prompt.' },
      } },
      logging: { type: 'object', description: 'Where logs go.', properties: { level: str('Minimum level.', { enum: ['trace', 'debug', 'info', 'warn', 'error'] }), file: str('Log file path.') } },
      tools: { type: 'object', description: 'Tool permissions.', properties: { allow: { type: 'array', items: { type: 'string' }, description: 'Patterns that never ask.' }, deny: { type: 'array', items: { type: 'string' }, description: 'Patterns that are always refused.' } } },
      pilot: { type: 'object', description: 'Autonomous runs.', properties: {
        max_parallel: { type: 'integer', description: 'Agents at once.' },
        budget_usd: { type: 'number', description: 'Abort a run that would spend more.' },
        notifications: { type: 'object', description: 'Where each severity is delivered.', additionalProperties: str('desktop | panel | webhook | none') },
      } },
      serve: { type: 'object', description: 'The HTTP daemon.', properties: { addr: str('Listen address.'), token: str('Bearer token.') } },
    },
    definitions: { Provider: { type: 'object', properties: { api_key: str('API key.'), base_url: { type: ['string', 'null'], description: 'Override the endpoint.' } } } },
  },
  defaults: { permission_mode: 'ask', reasoning: 'medium', tui: { theme: 'auto', vim_mode: false, show_thinking: false }, tokens: { max_output: 8_000, compact_at: 0.85 } },
  redacted_keys: ['providers.*.api_key', 'serve.token'],
  readonly_sections: ['serve'],
  writes_to: '~/.wingman/config.toml',
}

function merge(a: Record<string, unknown>, b: Record<string, unknown>): Record<string, unknown> {
  const o = { ...a }
  for (const [k, v] of Object.entries(b)) {
    o[k] = v && typeof v === 'object' && !Array.isArray(v) && typeof o[k] === 'object' && o[k] ? merge(o[k] as Record<string, unknown>, v as Record<string, unknown>) : v
  }
  return o
}

/* ── HTTP plumbing ────────────────────────────────────────────────────── */

type Req = { url?: string; method?: string; on(e: string, f: (c?: unknown) => void): void }
type Res = {
  statusCode: number
  setHeader(k: string, v: string): void
  write(s: string): void
  end(s?: string): void
  on(e: string, f: () => void): void
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

function body(req: Req): Promise<Record<string, unknown>> {
  return new Promise((resolve) => {
    let raw = ''
    req.on('data', (c) => (raw += String(c)))
    req.on('end', () => {
      try {
        resolve(raw ? JSON.parse(raw) : {})
      } catch {
        resolve({})
      }
    })
  })
}

function json(res: Res, status: number, value: unknown) {
  res.statusCode = status
  if (value === undefined) return res.end()
  res.setHeader('Content-Type', 'application/json')
  res.end(JSON.stringify(value))
}

function sse(res: Res) {
  res.statusCode = 200
  res.setHeader('Content-Type', 'text/event-stream')
  res.setHeader('Cache-Control', 'no-cache')
  res.write(': open\n\n')
  return (data: unknown, event?: string) => res.write(`${event ? `event: ${event}\n` : ''}data: ${JSON.stringify(data)}\n\n`)
}

async function turn(res: Res, s: SessionRow | undefined, prompt: string) {
  const send = sse(res)
  const reply = `Here is a mock answer to: "${prompt.slice(0, 80)}".\n\nThe dev server streams this in small chunks so the composer's **streaming state**, the tool rows and the final usage line can all be looked at.\n\n- one\n- two\n- three`
  send({ type: 'thinking_delta', text: 'Checking the repo layout first.' })
  await sleep(400)
  send({ type: 'tool_start', id: 'mt1', name: 'grep', input: { pattern: 'notifications', path: 'crates/' } })
  await sleep(700)
  send({ type: 'tool_result', id: 'mt1', output: 'crates/wingman-cli/src/serve/notifications.rs\ncrates/wingman-config/src/lib.rs', is_error: false })
  for (const chunk of reply.match(/.{1,12}/gs) ?? []) {
    send({ type: 'text_delta', text: chunk })
    await sleep(40)
  }
  send({ type: 'usage', usage: { input_tokens: 9_812, output_tokens: 344 } })
  send({ type: 'verification', passed: true, summary: 'cargo check passed' })
  send({ type: 'turn_complete' })
  send({ type: 'end', exit: 0 })
  res.end()
  if (s) {
    s.records.push(
      { kind: 'user', ts: new Date().toISOString(), text: prompt },
      { kind: 'assistant', ts: new Date().toISOString(), blocks: [{ type: 'text', text: reply }] },
      { kind: 'stop', ts: new Date().toISOString(), reason: 'end_turn' },
    )
    s.first_prompt ??= prompt
    s.mtime = Math.floor(Date.now() / 1000)
  }
}

/* ── Routes ───────────────────────────────────────────────────────────── */

async function handle(req: Req, res: Res): Promise<void> {
  const url = new URL(req.url ?? '/', 'http://mock')
  const method = req.method ?? 'GET'
  const p = url.pathname.split('/').slice(2).map(decodeURIComponent) // drop '', 'v1'
  const q = url.searchParams

  // Long-lived streams first: no artificial latency.
  if (p[0] === 'events') {
    const send = sse(res)
    const timer = setInterval(() => send({ project: 'wingman', run_id: runs[0].run_id, status: 'running' }), 20_000)
    return res.on('close', () => clearInterval(timer))
  }
  if (p[0] === 'projects' && p[2] === 'pilot' && p[5] === 'stream') {
    const r = runs.find((x) => x.run_id === p[4])
    const send = sse(res)
    if (!r || ['done', 'failed', 'aborted'].includes(r.status)) {
      send({}, 'end')
      return res.end()
    }
    const timer = setInterval(() => send({ ev: 'task.tool', t: new Date().toISOString(), id: 't4', tool: 'bash: cargo test -p wingman-config', ok: true }), 6_000)
    return res.on('close', () => clearInterval(timer))
  }
  if (method === 'POST' && p[0] === 'projects' && p.at(-1) === 'turns') {
    const b = await body(req)
    return turn(res, sessions.find((s) => s.session_id === p[3]), String(b.prompt ?? ''))
  }

  await sleep(150 + Math.random() * 250)

  if (p[0] === 'health') return json(res, 200, { ok: true, version: '0.5.0-mock', uptime_secs: 3 * 3600 + 1234, auth_required: false })
  if (p[0] === 'projects' && p.length === 1) return json(res, 200, { projects })
  if (p[0] === 'schema') return json(res, 200, SCHEMA)
  if (p[0] === 'config' && p[1] === 'schema') return json(res, 200, CONFIG_SCHEMA)
  if (p[0] === 'config') {
    if (method === 'PATCH') {
      const b = await body(req)
      if ('serve' in b) return json(res, 400, { error: 'section `serve` is read-only over HTTP' })
      config = merge(config, b)
    }
    return json(res, 200, config)
  }
  if (p[0] === 'ui' || p[0] === 'pair') return json(res, 200, { auth_required: false, token: 'mock' })

  if (p[0] === 'notifications') {
    if (method === 'POST') {
      notifications = notifications.filter((n) => n.id !== p[1])
      return json(res, 200, { answered: p[1], via: 'reply' })
    }
    return json(res, 200, { notifications })
  }

  if (p[0] === 'board') {
    if (!p[1]) {
      const project = q.get('project')
      const rows = cards.filter((c) => (!c.archived || q.get('archived')) && (!project || c.project === project))
      const data: BoardData = {
        columns: COLUMNS,
        cards: rows.map(cardJson),
        projects: [...projects.map((x) => ({ id: x.id, name: x.id, root: x.root, missing: false })), { id: 'gone-repo', name: 'gone-repo', root: 'D:/old/gone-repo', missing: true }],
      }
      return json(res, 200, data)
    }
    if (method === 'POST' && !p[2]) {
      const b = await body(req)
      const id = `c${nextCard++}`
      cards.push({ id, title: String(b.title), goal: String(b.goal ?? ''), notes: (b.notes as string) ?? null, labels: (b.labels as string[]) ?? [], archived: false, created_at: new Date().toISOString(), project: String(b.project), run_id: null, dispatches: [] })
      return json(res, 200, { id, short: id.replace('c', '#') })
    }
    const c = cards.find((x) => x.id === p[2])
    if (!c) return json(res, 404, { error: `no card ${p[2]}` })
    if (p[3] === 'archive') {
      c.archived = !(await body(req)).restore
      return json(res, 200, { id: c.id, archived: c.archived })
    }
    if (p[3] === 'dispatch') {
      const r = run(c.project, `2026-11-06-${String(Date.now()).slice(-4)}-mock${cards.indexOf(c)}`, c.title, 'planning', [])
      runs.unshift(r)
      c.run_id = r.run_id
      c.dispatches.push(r.run_id)
      return json(res, 200, { run_id: r.run_id, project: c.project, pid: 41_234 })
    }
    if (method === 'DELETE') {
      cards.splice(cards.indexOf(c), 1)
      return json(res, 204, undefined)
    }
    if (method === 'PATCH') {
      Object.assign(c, await body(req))
      return json(res, 200, { id: c.id, title: c.title, goal: c.goal })
    }
    return json(res, 200, {
      card: { id: c.id, short: c.id.replace('c', '#'), title: c.title, goal: c.goal, notes: c.notes, labels: c.labels, archived: c.archived, project: c.project, created_at: c.created_at },
      dispatches: c.dispatches.map((id, i) => ({ run_id: id, project: c.project, started_at: iso(60 * (c.dispatches.length - i) * 20), ended_at: i < c.dispatches.length - 1 ? iso(60 * (c.dispatches.length - i) * 20 - 30) : null })),
    })
  }

  if (p[0] !== 'projects') return json(res, 404, { error: `mock has no route for ${url.pathname}` })
  const project = p[1]
  const rest = p.slice(2)
  const tail = rest.join('/')

  if (rest[0] === 'pilot') {
    const mine = runs.filter((r) => projectOf(r) === project)
    if (!rest[2]) {
      if (method === 'POST') {
        const b = await body(req)
        const r = run(project, `2026-11-06-${String(Date.now()).slice(-4)}-new`, String(b.goal), 'planning', [])
        runs.unshift(r)
        return json(res, 200, { run_id: r.run_id })
      }
      return json(res, 200, {
        runs: mine.map((r) => ({ run_id: r.run_id, goal: r.goal, status: r.status, done: r.tasks.filter((t) => t.status === 'done').length, total: r.tasks.length, terminal: ['done', 'failed', 'aborted'].includes(r.status) })),
      })
    }
    const r = mine.find((x) => x.run_id === rest[2])
    if (!r) return json(res, 404, { error: `no run ${rest[2]}` })
    if (rest[3] === 'events') return json(res, 200, { events: events(r) })
    if (rest[3] === 'log') return json(res, 200, { text: r.status === 'failed' ? 'planning… ok\nt3: cargo test failed\nrun failed: 1 task failed after 2 attempts' : 'planning… ok', total_lines: 3, shown_lines: 3 })
    if (rest[3]) {
      if (rest[3] === 'approve' && r.status === 'awaiting_approval') r.status = 'running'
      if (rest[3] === 'veto' || rest[3] === 'abort') r.status = 'aborted'
      return json(res, 200, { queued: rest[3] })
    }
    return json(res, 200, wire(r))
  }

  if (rest[0] === 'sessions') {
    const mine = sessions.filter((s) => s.project === project)
    if (!rest[1]) {
      if (method === 'POST') {
        const id = new Date().toISOString().replace(/[-:.]/g, '').replace('Z', '000Z')
        sessions.unshift({ session_id: id, first_prompt: null, model: 'claude-opus-5-5', provider: 'claude-code', mtime: unix(0), project, records: [] })
        return json(res, 200, { session_id: id })
      }
      return json(res, 200, { sessions: mine.sort((a, b) => b.mtime - a.mtime).map(({ records, project: _, ...s }) => ({ ...s, turns: turns({ records } as SessionRow) })) })
    }
    const s = mine.find((x) => x.session_id === rest[1])
    if (!s) return json(res, 404, { error: `no session ${rest[1]}` })
    if (method === 'DELETE') {
      sessions.splice(sessions.indexOf(s), 1)
      return json(res, 200, { deleted: s.session_id, deindexed: { chunks: 12 } })
    }
    if (rest[2] === 'export') {
      res.setHeader('Content-Type', q.get('format') === 'json' ? 'application/json' : 'text/plain; charset=utf-8')
      return res.end(q.get('format') === 'json' ? JSON.stringify(s.records, null, 2) : `# Session ${s.session_id}\n\n**Prompt:** ${s.first_prompt ?? '(none)'}\n\n## Files changed\n- panel/src/app.css\n\n## Cost\n$0.42 · ${turns(s)} turns\n`)
    }
    if (rest[2] === 'rewind') {
      if (!rest[3]) return json(res, 200, { session_id: s.session_id, points: s.records.length ? [
        { seq: 7, turn: null, prompt: null, restore: 3, ts: unix(60), files: ['panel/src/app.css'] },
        { seq: 5, turn: 1, prompt: 'ok leave it. can you tighten the card padding instead?', restore: null, ts: unix(69), files: ['panel/src/app.css', 'panel/src/Board.tsx'] },
        { seq: 3, turn: 0, prompt: s.first_prompt, restore: null, ts: unix(88), files: ['crates/wingman-board/src/lib.rs'] },
      ] : [] })
      if (method === 'POST') {
        const b = await body(req)
        return json(res, 200, { restored: ['panel/src/app.css'], forked_session: b.truncate ? `${s.session_id.slice(0, -4)}999Z` : null })
      }
      return json(res, 200, { seq: Number(rest[3]), redacted: 1, changes: [
        { path: 'panel/src/app.css', exists_now: true, exists_after: true, diff: '@@ -210,3 +210,3 @@\n .card {\n-  padding: 0.625rem 0.75rem;\n+  padding: 0.75rem 1rem;\n }' },
        { path: 'panel/src/new-file.ts', exists_now: true, exists_after: false, diff: '@@ -1,2 +0,0 @@\n-export const x = 1\n-export const y = 2' },
      ] })
    }
    return json(res, 200, { session_id: s.session_id, records: s.records })
  }

  const empty = project === 'scratch'
  if (rest[0] === 'cost' && rest[1] === 'timeline') {
    const n = Number(q.get('days') ?? 30)
    const days = Array.from({ length: n }, (_, i) => {
      const date = new Date(NOW - (n - 1 - i) * 86_400_000).toISOString().slice(0, 10)
      // Quiet weekends, one spike, and a run of zero days — gap-filled like the real route.
      const usd = empty || (i > 8 && i < 13) ? 0 : +(Math.abs(Math.sin(i * 1.7)) * 4 + (i === n - 3 ? 14 : 0)).toFixed(2)
      return { date, usd, input_tokens: Math.round(usd * 90_000), output_tokens: Math.round(usd * 8_000), cache_read_tokens: Math.round(usd * 300_000), cache_write_tokens: Math.round(usd * 20_000), turns: Math.round(usd * 6) }
    })
    const window_usd = +days.reduce((s, d) => s + d.usd, 0).toFixed(2)
    return json(res, 200, {
      days,
      models: empty ? [] : [
        { model: 'claude-opus-5-5', usd: +(window_usd * 0.7).toFixed(2), turns: 210, input_tokens: 3_100_000, output_tokens: 240_000 },
        { model: 'claude-sonnet-5-5', usd: +(window_usd * 0.28).toFixed(2), turns: 160, input_tokens: 1_900_000, output_tokens: 150_000 },
        { model: 'qwen3-coder:30b', usd: 0, turns: 41, input_tokens: 400_000, output_tokens: 30_000 },
      ],
      window_days: n,
      window_usd,
      total_usd: empty ? 0 : window_usd + 212.4,
      total_turns: empty ? 0 : 1_384,
      first_day: empty ? null : '2026-06-02',
      last_day: empty ? null : days[n - 1].date,
      sessions: empty ? 0 : 96,
      unpriced_turns: empty ? 0 : 41,
    })
  }
  if (rest[0] === 'metrics') {
    if (empty) return json(res, 200, { sessions: 0, turns: 0, completed_turns: 0, time_to_first_token_ms: { median: null, p90: null, samples: 0 }, total_tokens: 0, tokens_per_completed_task: null, gated_turns: 0, verified_turns: 0, verified_done_rate: null, sessions_with_receipt: 0, sessions_verified: 0, session_verified_done_rate: null, routing: [] })
    return json(res, 200, {
      sessions: 96, turns: 1_384, completed_turns: 1_310,
      time_to_first_token_ms: { median: 1_240, p90: 3_980, samples: 96 },
      total_tokens: 9_517_000, tokens_per_completed_task: 7_265,
      gated_turns: 402, verified_turns: 351, verified_done_rate: 0.873,
      sessions_with_receipt: 61, sessions_verified: 1, session_verified_done_rate: 1,
      routing: [
        { task_class: 'refactor', model: 'claude-opus-5-5', passed: 88, total: 94, pass_rate: 0.936 },
        { task_class: 'refactor', model: 'claude-sonnet-5-5', passed: 51, total: 63, pass_rate: 0.81 },
        { task_class: 'test-fix', model: 'claude-sonnet-5-5', passed: 120, total: 141, pass_rate: 0.851 },
        { task_class: 'docs', model: 'claude-haiku-5-5', passed: 30, total: 31, pass_rate: 0.968 },
        { task_class: 'feature', model: 'qwen3-coder:30b', passed: 2, total: 9, pass_rate: 0.222 },
        { task_class: 'migration', model: 'claude-opus-5-5', passed: 1, total: 1, pass_rate: 1 },
      ],
    })
  }
  if (rest[0] === 'context') {
    const tools = ['read_file', 'edit_file', 'write_file', 'bash', 'grep', 'glob', 'semantic_search', 'web_fetch', 'ask_user', 'todo', 'mcp__linear__search_issues_and_projects_with_filters', 'browser']
      .map((name, i) => ({ name, bytes: 900 + ((i * 677) % 3_100), tokens: 225 + ((i * 677) % 3_100) / 4 }))
    const tool_schema_tokens = Math.round(tools.reduce((s, t) => s + t.tokens, 0))
    return json(res, 200, { system_prompt_bytes: 21_400, system_prompt_tokens: 5_350, tool_count: tools.length, tool_schema_tokens, first_turn_tokens: 5_350 + tool_schema_tokens, tools })
  }

  if (rest[0] === 'cost') {
    if (empty) return json(res, 200, { rows: [], comparison: [], total_input_tokens: 0, total_output_tokens: 0, total_usd: 0 })
    return json(res, 200, {
      rows: [
        { key: 'claude-opus-5-5', input_tokens: 4_210_000, output_tokens: 312_000, cache_read_tokens: 18_400_000, cache_write_tokens: 1_200_000, usd: 48.21 },
        { key: 'claude-sonnet-5-5', input_tokens: 2_900_000, output_tokens: 240_000, cache_read_tokens: 9_100_000, cache_write_tokens: 640_000, usd: 14.07 },
        { key: 'claude-haiku-5-5', input_tokens: 1_100_000, output_tokens: 90_000, cache_read_tokens: 0, cache_write_tokens: 0, usd: 1.36 },
        { key: 'qwen3-coder:30b', input_tokens: 610_000, output_tokens: 55_000, cache_read_tokens: 0, cache_write_tokens: 0, usd: 0 },
      ],
      comparison: q.has('compare') ? [
        { model: 'claude-opus-5-5', would_cost_usd: 102.4 },
        { model: 'claude-sonnet-5-5', would_cost_usd: 31.9 },
        { model: 'claude-haiku-5-5', would_cost_usd: 8.6 },
        { model: 'gpt-5', would_cost_usd: 27.3 },
      ] : [],
      total_input_tokens: 8_820_000,
      total_output_tokens: 697_000,
      total_usd: 63.64,
    })
  }

  // Table-driven routes: writes are POSTs, reads are GETs.
  if (method === 'POST') {
    const done: Record<string, string> = {
      checkpoints: 'checkpoint 0042 created (3 files)',
      rewind: 'restored 3 files to checkpoint 0041',
      'index/reindex': 'reindexed 1,204 files (18,903 chunks) in 41.2s',
      'memory/sync': 'MEMORY.md rebuilt from 7 files',
      trust: 'repo trusted: hooks and MCP servers in .wingman/ will run',
      'schedule/run': 'no scheduled prompts due',
    }
    return json(res, 200, out(done[tail] ?? `ran ${tail}`))
  }
  const [status, value] = report(rest[0] === 'index' ? tail : rest[0], q)
  return json(res, status, value)
}

export function mockApi(): Plugin {
  return {
    name: 'wingman-mock-api',
    configureServer(server) {
      server.middlewares.use((rawReq, rawRes, next) => {
        const req = rawReq as unknown as Req
        const res = rawRes as unknown as Res
        if (!req.url?.startsWith('/v1/')) return next()
        handle(req, res).catch((e: unknown) => json(res, 500, { error: String(e) }))
      })
    },
  }
}
