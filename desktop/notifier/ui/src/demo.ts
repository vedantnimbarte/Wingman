import type { Notification } from './wire'

/**
 * Cards for `npm run dev`, so the popup can be designed in a browser without a
 * Rust rebuild — the same reason the panel's dev server proxies to a live
 * `wingman serve`.
 *
 * Only reachable behind `import.meta.env.DEV`, so Vite drops this file from the
 * production bundle entirely.
 *
 * One of each kind, because the layout problems are all at the seams: a failure
 * with no buttons, a gate with two, and a question with suggestions *and* a
 * box.
 */
export function demoCards(now: number): Notification[] {
  const run = '/repo/.wingman/autonomous/2026-08-21-1729-m8nnap'
  return [
    {
      id: 'demo-fail',
      severity: 'escalation',
      title: 'Task failed — Wire the parser',
      body: 'cargo test failed: 3 assertions in wingman-config::inbox',
      project: 'Wingman',
      run_dir: run,
      created_at: now,
      expires_at: null,
      actions: [],
      free_text: false,
    },
    {
      id: 'demo-gate',
      severity: 'decision',
      title: 'Plan awaiting approval — 7 tasks',
      body: 'Add actionable desktop notifications\n\nest. $2.40 — touches crates/wingman-config/**',
      project: 'Wingman',
      run_dir: run,
      created_at: now,
      expires_at: now + 3600,
      actions: [
        { id: 'approve', label: 'Approve', control: { cmd: 'approve' } },
        { id: 'veto', label: 'Veto', control: { cmd: 'veto' } },
      ],
      free_text: false,
    },
    {
      id: 'demo-ask',
      severity: 'decision',
      title: 'wingman is asking',
      body: 'Postgres or SQLite for the session store?',
      project: 'Wingman',
      run_dir: null,
      created_at: now,
      expires_at: now + 3600,
      actions: [
        { id: 'postgres', label: 'postgres' },
        { id: 'sqlite', label: 'sqlite' },
      ],
      free_text: true,
    },
  ]
}
