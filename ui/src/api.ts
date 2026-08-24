/**
 * The one place that talks to `/v1`.
 *
 * Authentication is a cookie the browser holds and page script cannot read
 * (`HttpOnly`), so nothing here handles a token except `signIn`. Every other
 * call just needs the cookie to ride along, which `credentials: 'same-origin'`
 * arranges.
 */

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }

  /** The credential is missing or no longer accepted — sign in again. */
  get isUnauthorized() {
    return this.status === 401
  }
}

/** `GET /v1/health` — the only route that never needs a credential. */
export type Health = {
  ok: boolean
  version: string
  uptime_secs: number
  /** False on a loopback server with no token: skip the sign-in screen. */
  auth_required: boolean
}

/** One entry of `GET /v1/projects`. Field names mirror `serve::projects::describe`. */
export type Project = {
  id: string
  root: string
  branch: string | null
  indexd_running: boolean
  /** Age of the semantic index, or null when the repo has never been indexed. */
  index_age_secs: number | null
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response
  try {
    res = await fetch(path, { credentials: 'same-origin', ...init })
  } catch {
    // A network-level failure means the daemon is not answering at all, which
    // is a different problem from a route that said no — name it as such
    // rather than letting a generic "Failed to fetch" reach the screen.
    throw new ApiError(0, 'No answer from the daemon. Is `wingman serve` running?')
  }

  if (!res.ok) {
    throw new ApiError(res.status, await errorText(res))
  }
  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

/** Prefer the server's own `{"error": ...}` message over a status code. */
async function errorText(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as { error?: string }
    if (body?.error) return body.error
  } catch {
    // Not JSON. The status line is all we have.
  }
  return `${res.status} ${res.statusText}`
}

/* ── Board ────────────────────────────────────────────────────────────────
 *
 * Shapes mirror `serve::board::card_json` and `wingman_board::Rollup`. The
 * column, the roll-up and the badges are all **derived server-side**, by the
 * same code `wingman board` renders — so the panel cannot disagree with the
 * TUI about what state a card is in, and there is no derivation to reimplement
 * here and keep in sync.
 */

/** Serialised `wingman_autonomous::TaskStatus`. */
export type TaskStatus = 'pending' | 'todo' | 'in_progress' | 'review' | 'done' | 'failed' | 'blocked'

/** Serialised `wingman_autonomous::RunStatus`. */
export type RunStatus =
  | 'planning'
  | 'awaiting_approval'
  | 'running'
  | 'merging'
  | 'done'
  | 'failed'
  | 'aborted'

/** One planner task under a card. Ephemeral — projected live from the run. */
export type SubRow = {
  task_id: string
  title: string
  status: TaskStatus
  role: string
  agent_name: string | null
  model: string | null
  session_id: string | null
  usd: number
  attempts: number
  /** Unmet dependencies — why the scheduler is holding this task. */
  blocked_by: string[]
  current_tool: string | null
  deps: string[]
  writes: number
  elapsed_secs: number | null
  outcome: string | null
  worktree: string | null
}

export type Rollup = {
  status: RunStatus
  done: number
  total: number
  failed: number
  blocked: number
  review: number
  in_progress: number
  not_started: number
  usd: number
  subrows: SubRow[]
}

/** A card is durable; everything under `rollup` is projected from the run. */
export type Card = {
  id: string
  short: string
  title: string
  goal: string
  notes: string | null
  labels: string[]
  archived: boolean
  created_at: string
  project: string
  project_name: string
  project_missing: boolean
  column: ColumnId
  run_id: string | null
  badges: Badge[]
  rollup: Rollup | null
}

export type ColumnId = 'backlog' | 'planned' | 'in-progress' | 'review' | 'done'

/**
 * A badge carries its kind as well as its text, so the panel can drop the ones
 * it already renders as structured fields without matching on formatted
 * strings. `wingman board list --json` emits only the text.
 */
export type Badge = {
  kind: 'progress' | 'cost' | 'failed' | 'blocked' | 'aborted' | 'retry' | 'missing' | 'label' | 'more_labels'
  text: string
}

export type BoardProject = { id: string; name: string; root: string; missing: boolean }

export type BoardData = {
  columns: { id: ColumnId; title: string }[]
  cards: Card[]
  projects: BoardProject[]
}

export type Dispatched = { run_id: string; project: string; pid: number }

export const api = {
  health: () => request<Health>('/v1/health'),
  projects: () => request<{ projects: Project[] }>('/v1/projects').then((r) => r.projects),

  /**
   * Exchange a token for the panel cookie. The token is sent once and never
   * stored by the page — the server puts it in an `HttpOnly` cookie, so it
   * is not readable by script afterwards, including by this module.
   */
  signIn: (token: string) =>
    request<{ auth_required: boolean }>('/v1/ui/session', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token }),
    }),

  signOut: () => request<void>('/v1/ui/session', { method: 'DELETE' }),

  board: (project?: string) =>
    request<BoardData>(`/v1/board${project ? `?project=${encodeURIComponent(project)}` : ''}`),

  addCard: (body: { project: string; title: string; goal?: string; labels?: string[] }) =>
    request<{ id: string; short: string }>('/v1/board/cards', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }),

  dispatchCard: (id: string, body: { again?: boolean; args?: string[] } = {}) =>
    request<Dispatched>(`/v1/board/cards/${encodeURIComponent(id)}/dispatch`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }),

  archiveCard: (id: string, restore = false) =>
    request<{ id: string; archived: boolean }>(
      `/v1/board/cards/${encodeURIComponent(id)}/archive`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ restore }),
      },
    ),
}
