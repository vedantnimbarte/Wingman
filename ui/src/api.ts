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

/* ── Pilot runs ───────────────────────────────────────────────────────────
 *
 * `RunState` is the atomic `state.json` pilot writes after every event, served
 * verbatim. Nothing is projected server-side, so these types are the model
 * itself rather than a wire shape invented for the panel.
 */

export type RunSummary = {
  run_id: string
  goal: string
  status: RunStatus
  done: number
  total: number
  terminal: boolean
}

export type TaskOutcome = { summary: string; files_changed: string[] }

export type Task = {
  id: string
  role: string
  title: string
  goal: string
  deps: string[]
  /** Paths this task declared it would write — the write-set scheduler's input. */
  writes: string[]
  acceptance: unknown[]
  reversibility: string
  reversibility_reason: string | null
  status: TaskStatus
  agent: string | null
  worktree: string | null
  usd: number
  commits: string[]
  outcome: TaskOutcome | null
  started_at: string | null
  ended_at: string | null
  attempts: number
}

export type AgentStatus = 'idle' | 'in_progress' | 'done' | 'failed' | 'aborted'

export type Agent = {
  id: string
  /** Docker-style display name. Empty on runs predating the field. */
  name: string
  role: string
  current_task: string | null
  current_tool: string | null
  pid: number | null
  status: AgentStatus
  session_id: string | null
  spawned_at: string | null
  model: string | null
  usd: number
}

export type RunState = {
  run_id: string
  goal: string
  base_commit: string
  integration_branch: string
  status: RunStatus
  tasks: Task[]
  agents: Agent[]
  totals: { usd: number; tokens_in: number; tokens_out: number }
  pr_url: string | null
}

export type ControlAction = 'approve' | 'veto' | 'abort' | 'retry'

/* ── Sessions ─────────────────────────────────────────────────────────────
 *
 * A session is not a server object with a timeout — it is the same
 * `.wingman/sessions/<id>.jsonl` the TUI writes. One started in the browser
 * shows up in `wingman session list` and resumes from the terminal, because
 * the transcript on disk *is* the state.
 */

export type SessionSummary = {
  session_id: string
  first_prompt: string | null
  model: string | null
  provider: string | null
  turns: number
}

/** A block inside an assistant message. Mirrors `wingman_core::ContentBlock`. */
export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'tool_use'; id: string; name: string; input: unknown }
  | { type: 'tool_result'; tool_use_id: string; content: string; is_error?: boolean }
  | { type: 'image'; data: string; media_type: string }
  | { type: 'thinking'; text: string; signature?: string; redacted?: boolean }

/** One line of a transcript. Mirrors `wingman_session::SessionRecord`. */
export type SessionRecord =
  | { kind: 'session_start'; ts: string; model: string; provider: string }
  | { kind: 'user'; ts: string; text: string }
  | { kind: 'assistant'; ts: string; blocks: ContentBlock[] }
  | { kind: 'tool_result'; ts: string; id: string; output: string; is_error: boolean }
  | { kind: 'usage_delta'; ts: string; usage: Record<string, number> }
  | { kind: 'stop'; ts: string; reason: string }

/**
 * Events a turn streams. These are `wingman_core::AgentEvent` verbatim — the
 * child's NDJSON `type` becomes the SSE event name with no translation table,
 * so this list is the enum and cannot drift from it.
 */
export type TurnEvent =
  | { type: 'text_delta'; text: string }
  | { type: 'thinking_delta'; text: string }
  | { type: 'tool_start'; id: string; name: string; input: unknown }
  | { type: 'tool_result'; id: string; output: string; is_error: boolean }
  | { type: 'usage'; usage: Record<string, number> }
  | { type: 'turn_complete' }
  | { type: 'stop'; reason: string }
  | { type: 'verification'; passed: boolean; summary: string }
  | { type: 'error'; message: string }
  | { type: 'end'; exit?: number }
  | { type: 'log'; [k: string]: unknown }

/* ── Config ───────────────────────────────────────────────────────────────
 *
 * The schema is derived from the `wingman-config` structs, so the forms in the
 * panel are generated rather than written. A field added to a Rust struct
 * appears here with its `///` comment as help text and nobody touches the UI.
 */

/** A JSON Schema node, as much of it as the form renderer looks at. */
export type SchemaNode = {
  type?: string | string[]
  description?: string
  default?: unknown
  format?: string
  enum?: string[]
  oneOf?: SchemaNode[]
  allOf?: SchemaNode[]
  $ref?: string
  properties?: Record<string, SchemaNode>
  additionalProperties?: SchemaNode | boolean
  items?: SchemaNode
  title?: string
}

export type ConfigSchema = {
  schema: SchemaNode & { definitions?: Record<string, SchemaNode> }
  /** Every field's fallback, from `Config::default()` — redacted like any read. */
  defaults: Record<string, unknown>
  /** Keys whose values are credentials and come back as `<redacted>`. */
  redacted_keys: string[]
  /** Sections `PATCH` refuses. Rendered read-only rather than hidden. */
  readonly_sections: string[]
  /** The global file a save lands in. Never a repo's `.wingman/config.toml`. */
  writes_to: string
}

/**
 * Read an SSE stream from a `POST`.
 *
 * `EventSource` cannot do this — it only issues `GET` and sets no body — so a
 * turn's stream is parsed by hand off `fetch`'s `ReadableStream`. That is also
 * why this is the one place a stream is decoded rather than reusing the
 * `EventSource` in `state.tsx`.
 *
 * Only the `data:` lines are read. The event name is already carried inside
 * each payload as `type` (the server sets both from the same field), so
 * parsing `event:` as well would mean two sources for one fact.
 */
async function streamPost(
  path: string,
  body: unknown,
  onEvent: (e: TurnEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  let res: Response
  try {
    res = await fetch(path, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal,
    })
  } catch (e) {
    if (signal?.aborted) return
    throw new ApiError(0, 'No answer from the daemon. Is `wingman serve` running?')
  }

  // A refusal arrives as JSON with a status, not as a stream — a 409 for a
  // second turn on the same session, a 403 above the ceiling, a 429 when the
  // turn queue is full.
  if (!res.ok) throw new ApiError(res.status, await errorText(res))
  if (!res.body) throw new ApiError(500, 'the daemon returned no stream')

  const reader = res.body.getReader()
  const decoder = new TextDecoder()
  let buf = ''

  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buf += decoder.decode(value, { stream: true })
    const { events, rest } = drainFrames(buf)
    buf = rest
    events.forEach(onEvent)
  }
}

/**
 * Pull every complete SSE frame out of a buffer, returning what is left over.
 *
 * Frames are separated by a blank line, and a chunk boundary can fall anywhere
 * — mid-frame, mid-line, or between the two newlines that end one. Whatever
 * follows the last complete frame is returned as `rest` and waits for the next
 * chunk: dropping it would silently lose a message, and parsing it early would
 * truncate one.
 */
export function drainFrames(buf: string): { events: TurnEvent[]; rest: string } {
  const events: TurnEvent[] = []
  let split: number
  while ((split = buf.indexOf('\n\n')) !== -1) {
    const frame = buf.slice(0, split)
    buf = buf.slice(split + 2)
    for (const line of frame.split('\n')) {
      // Only `data:` carries a payload. The `event:` name duplicates the
      // `type` inside it, and a line starting `:` is a keepalive comment.
      if (!line.startsWith('data:')) continue
      const data = line.slice(5).replace(/^ /, '')
      try {
        events.push(JSON.parse(data) as TurnEvent)
      } catch {
        // Not JSON: the child's own output, forwarded as a log line by the
        // server. Surfacing it beats dropping it silently.
        events.push({ type: 'log', raw: data })
      }
    }
  }
  return { events, rest: buf }
}

export const api = {
  health: () => request<Health>('/v1/health'),

  /**
   * Run a turn, streaming the agent's events as they happen.
   *
   * `id` omitted runs a one-shot turn with no session continuity. A second
   * turn on the same session while one is in flight is a `409` — the child
   * would otherwise replay a transcript the first turn is still appending to.
   */
  turn: (
    project: string,
    id: string | null,
    body: { prompt: string; mode?: string; model?: string },
    onEvent: (e: TurnEvent) => void,
    signal?: AbortSignal,
  ) =>
    streamPost(
      id
        ? `/v1/projects/${encodeURIComponent(project)}/sessions/${encodeURIComponent(id)}/turns`
        : `/v1/projects/${encodeURIComponent(project)}/turns`,
      body,
      onEvent,
      signal,
    ),
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

  sessions: (project: string) =>
    request<{ sessions: SessionSummary[] }>(
      `/v1/projects/${encodeURIComponent(project)}/sessions`,
    ).then((r) => r.sessions),

  session: (project: string, id: string) =>
    request<{ session_id: string; records: SessionRecord[] }>(
      `/v1/projects/${encodeURIComponent(project)}/sessions/${encodeURIComponent(id)}`,
    ),

  newSession: (project: string) =>
    request<{ session_id: string }>(`/v1/projects/${encodeURIComponent(project)}/sessions`, {
      method: 'POST',
    }),

  /** Reports `deindexed` so a partial delete is visible now, not a surprise later. */
  deleteSession: (project: string, id: string) =>
    request<{ deleted: string; deindexed: unknown }>(
      `/v1/projects/${encodeURIComponent(project)}/sessions/${encodeURIComponent(id)}`,
      { method: 'DELETE' },
    ),

  config: () => request<Record<string, unknown>>('/v1/config'),
  configSchema: () => request<ConfigSchema>('/v1/config/schema'),

  /**
   * Merge a TOML-shaped object into the **global** config file.
   *
   * The server deep-merges tables, so a patch need only carry the leaves that
   * changed. It validates by round-tripping through the real config parser and
   * returns the parse error as a `400`, which is the only validation there is —
   * the panel deliberately does not second-guess it.
   */
  patchConfig: (patch: Record<string, unknown>) =>
    request<unknown>('/v1/config', {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
    }),

  runs: (project: string) =>
    request<{ runs: RunSummary[] }>(`/v1/projects/${encodeURIComponent(project)}/pilot/runs`).then(
      (r) => r.runs,
    ),

  run: (project: string, runId: string) =>
    request<RunState>(
      `/v1/projects/${encodeURIComponent(project)}/pilot/runs/${encodeURIComponent(runId)}`,
    ),

  /**
   * Each of these appends one `ControlCommand` to the run's `control.jsonl`;
   * the orchestrator's own watchdog applies it. The API never reaches into a
   * running process, which is why a control call can return before the run has
   * acted on it.
   */
  control: (project: string, runId: string, action: ControlAction, body: { task?: string } = {}) =>
    request<unknown>(
      `/v1/projects/${encodeURIComponent(project)}/pilot/runs/${encodeURIComponent(runId)}/${action}`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      },
    ),

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
