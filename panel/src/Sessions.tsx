import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { copyText } from './a11y'
import {
  api,
  ApiError,
  type ContentBlock,
  type SessionRecord,
  type SessionSummary,
  type TurnEvent,
} from './api'
import { Markdown } from './markdown'
import { navigate } from './router'
import { message } from './state'
import { Empty, Failed, Icon, Loading, Note, PageHead } from './ui'

/**
 * Sessions — transcripts, and holding a conversation with the agent.
 *
 * A session is the same `.wingman/sessions/<id>.jsonl` the TUI writes, so one
 * started here appears in `wingman session list` and resumes from a terminal.
 * The server keeps no conversation state; the file on disk is the state, which
 * is what makes "start on the laptop, continue on the phone" work with no sync
 * protocol behind it.
 */
export function Sessions({ project, id }: { project: string | null; id: string | null }) {
  if (!project) {
    return (
      <div className="view">
        <Failed
          title="No project selected"
          detail="Sessions live in a repo. Pick one in the header."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  return id ? (
    <Conversation key={id} project={project} id={id} />
  ) : (
    <SessionList project={project} />
  )
}

/* ── List ──────────────────────────────────────────────────────────────── */

function SessionList({ project }: { project: string }) {
  const [sessions, setSessions] = useState<SessionSummary[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [note, setNote] = useState<string | null>(null)
  const [q, setQ] = useState('')

  const load = useCallback(async () => {
    try {
      setSessions(await api.sessions(project))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project])

  useEffect(() => {
    void load()
  }, [load])

  async function remove(id: string) {
    setNote(null)
    try {
      const res = await api.deleteSession(project, id)
      // The API reports what happened to the search index as well as the file.
      // "The transcript is gone but recall may still find it" is something to
      // learn here, not from a surprise later.
      const deindexed = res.deindexed
      setNote(
        deindexed && typeof deindexed === 'object' && 'error' in deindexed
          ? `Transcript deleted, but its search-index entry remains: ${String((deindexed as { error: unknown }).error)}`
          : `Deleted ${id}${deindexed === false ? ' (nothing was indexed for it)' : ' and its search-index entry'}`,
      )
      await load()
    } catch (e) {
      setNote(message(e))
    }
  }

  const shown = useMemo(() => matching(sessions ?? [], q), [sessions, q])

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not list sessions"
          detail={error}
          action={{ label: 'Try again', onClick: () => void load() }}
        />
      </div>
    )
  if (!sessions) return <Loading what="sessions" />

  return (
    <div className="view">
      <PageHead
        eyebrow="Sessions"
        title={sessions.length === 1 ? '1 session' : `${sessions.length} sessions`}
        intro={
          <>
            Transcripts in <code>.wingman/sessions/</code>, most recently written first. A
            conversation started here is a normal session file — it shows up in{' '}
            <code>wingman session list</code> and resumes from a terminal.
          </>
        }
        actions={
          <button
            type="button"
            className="button button-primary"
            onClick={() => navigate('/sessions/new')}
          >
            New conversation
          </button>
        }
      />

      {note && <Note>{note}</Note>}

      {sessions.length > 0 && (
        <div className="filters">
          <label className="filter-search">
            <Icon name="search" size={14} />
            <input
              className="input"
              type="search"
              placeholder="Filter by first prompt, id or model"
              aria-label="Filter sessions"
              value={q}
              onChange={(e) => setQ(e.target.value)}
            />
          </label>
          {shown.length !== sessions.length && (
            <span className="faint figure">
              {shown.length} of {sessions.length}
            </span>
          )}
        </div>
      )}

      {sessions.length === 0 ? (
        <Empty
          title="No conversations yet"
          action={{ label: 'Start one', onClick: () => navigate('/sessions/new') }}
        >
          A conversation here writes the same transcript a terminal session does, so you can start
          on one and finish on the other.
        </Empty>
      ) : shown.length === 0 ? (
        <Empty title="Nothing matches" action={{ label: 'Clear', onClick: () => setQ('') }}>
          No transcript here mentions “{q.trim()}” in its first prompt, its id or its model.
        </Empty>
      ) : (
        <div className="rows">
          {shown.map((s) => (
            <div key={s.session_id} className="row">
              <button
                type="button"
                className="task-toggle"
                onClick={() => navigate(`/sessions/${s.session_id}`)}
              >
                {s.first_prompt ?? <span className="muted">(no prompt yet)</span>}
                <span className="task-meta faint">
                  <span className="identifier">{s.session_id}</span>
                  {s.model && ` · ${s.model}`}
                  {` · ${s.turns} ${s.turns === 1 ? 'turn' : 'turns'}`}
                  {s.mtime ? ` · ${ago(s.mtime)}` : ''}
                </span>
              </button>
              <button
                type="button"
                className="button button-quiet button-sm"
                onClick={() => void remove(s.session_id)}
                title="Delete the transcript and its search-index entry"
              >
                Delete
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

/** Substring over the three things a session is actually recognised by. */
export function matching(sessions: SessionSummary[], q: string): SessionSummary[] {
  const needle = q.trim().toLowerCase()
  if (!needle) return sessions
  return sessions.filter((s) =>
    `${s.first_prompt ?? ''} ${s.session_id} ${s.model ?? ''}`.toLowerCase().includes(needle),
  )
}

/**
 * Coarse relative time. "3 days ago" is what someone scanning a list needs;
 * the exact stamp is in the transcript and nobody reads it from a row.
 */
export function ago(unixSecs: number, now = Date.now()): string {
  const secs = Math.max(0, Math.floor(now / 1000) - unixSecs)
  if (secs < 90) return 'just now'
  const mins = Math.round(secs / 60)
  if (mins < 60) return `${mins}m ago`
  const hours = Math.round(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.round(hours / 24)
  return days === 1 ? 'yesterday' : `${days}d ago`
}

/* ── Conversation ──────────────────────────────────────────────────────── */

/** What the composer is doing. `live` carries the text streamed so far. */
export type Turn =
  | { state: 'idle' }
  | { state: 'streaming'; text: string; thinking: string; tools: ToolCall[] }
  | { state: 'failed'; detail: string }

export type ToolCall = { id: string; name: string; output?: string; failed?: boolean }

/**
 * A permission mode a turn may ask for.
 *
 * The server clamps to `[serve].max_permission_mode` and refuses anything
 * above it with a `403`, so this can only ever ask for *less* — which is the
 * useful direction. The composer previously sent neither this nor a model and
 * said so in its hint, which described the ceiling as if it were the only
 * choice available.
 */
const MODES = ['read-only', 'plan', 'auto-edit'] as const

const MODE_KEY = 'wingman.turn.mode'
const MODEL_KEY = 'wingman.turn.model'

/** How many transcript records render before the "show earlier" fold. */
const WINDOW = 150

function Conversation({ project, id }: { project: string; id: string }) {
  const isNew = id === 'new'
  const [records, setRecords] = useState<SessionRecord[] | null>(isNew ? [] : null)
  const [error, setError] = useState<string | null>(null)
  const [prompt, setPrompt] = useState('')
  const [turn, setTurn] = useState<Turn>({ state: 'idle' })
  const [verification, setVerification] = useState<{ passed: boolean; summary: string } | null>(null)
  const [all, setAll] = useState(false)
  const [pinned, setPinned] = useState(true)
  const [mode, setMode] = useState(() => window.localStorage.getItem(MODE_KEY) ?? '')
  const [model, setModel] = useState(() => window.localStorage.getItem(MODEL_KEY) ?? '')
  const abort = useRef<AbortController | null>(null)
  const foot = useRef<HTMLDivElement | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)

  const load = useCallback(async () => {
    if (isNew) return
    try {
      setRecords((await api.session(project, id)).records)
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project, id, isNew])

  useEffect(() => {
    void load()
  }, [load])

  // Keep the newest text in view while a turn streams — unless the reader has
  // scrolled up, in which case yanking them back to the bottom every 40ms is
  // the single most hostile thing a streaming view can do.
  useEffect(() => {
    if (pinned) foot.current?.scrollIntoView({ block: 'end' })
  }, [turn, pinned])

  useEffect(() => () => abort.current?.abort(), [])

  function onScroll() {
    const el = scroller.current
    if (!el) return
    // 40px of slack: an exact comparison unpins on the sub-pixel rounding a
    // smooth scroll lands on.
    setPinned(el.scrollHeight - el.scrollTop - el.clientHeight < 40)
  }

  async function send() {
    const text = prompt.trim()
    if (!text) return

    // A session id is minted before the first turn so the URL is stable and
    // the conversation is linkable from the moment it starts.
    let target = id
    if (isNew) {
      try {
        target = (await api.newSession(project)).session_id
      } catch (e) {
        return setTurn({ state: 'failed', detail: message(e) })
      }
    }

    setPrompt('')
    setVerification(null)
    setTurn({ state: 'streaming', text: '', thinking: '', tools: [] })
    setPinned(true)
    abort.current = new AbortController()

    try {
      await api.turn(
        project,
        target,
        { prompt: text, mode: mode || undefined, model: model.trim() || undefined },
        (e) => apply(e, setTurn, setVerification),
        abort.current.signal,
      )
      // The transcript on disk is authoritative: re-reading it is what puts
      // this turn into the same shape as every earlier one, rather than
      // keeping a separately-assembled copy in memory.
      if (isNew) navigate(`/sessions/${target}`)
      else await load()
      setTurn({ state: 'idle' })
    } catch (e) {
      const detail =
        e instanceof ApiError && e.status === 409
          ? 'This session already has a turn running. Wait for it to finish — a second turn would replay a transcript the first is still writing.'
          : message(e)
      setTurn({ state: 'failed', detail })
    }
  }

  if (error)
    return (
      <div className="view">
        <Failed
          title="Could not load the session"
          detail={error}
          action={{ label: 'Back to sessions', onClick: () => navigate('/sessions') }}
        />
      </div>
    )
  if (!records) return <Loading what="the transcript" />

  const streaming = turn.state === 'streaming'

  // In a live stream `tool_start` and `tool_result` are paired by id as they
  // arrive. In a transcript they are separate records — the call is a block
  // inside an assistant message, the result is its own line — so they have to
  // be rejoined here. Without this every tool renders as still running and its
  // output is never shown.
  const results = new Map<string, { output: string; failed: boolean }>()
  for (const r of records) {
    if (r.kind === 'tool_result') results.set(r.id, { output: r.output, failed: r.is_error })
  }

  // A long session is thousands of records, each with a `<pre>` of tool output
  // — enough to make scrolling stutter and a phone give up. The newest window
  // renders; the rest is one click away. Not virtualisation: a windowed list
  // with a fold is twenty lines and has no scroll-anchoring bugs to find.
  const folded = !all && records.length > WINDOW
  const visible = folded ? records.slice(-WINDOW) : records

  return (
    <div className="view chat">
      <button
        type="button"
        className="button button-quiet back"
        onClick={() => navigate('/sessions')}
      >
        <Icon name="collapse" size={14} />
        Sessions
      </button>
      <header className="page-head">
        <div className="page-title">
          <span className="eyebrow">Conversation</span>
          <h1 className="figure identifier">{isNew ? 'new conversation' : id}</h1>
        </div>
      </header>

      <div className="transcript" ref={scroller} onScroll={onScroll}>
        {folded && (
          <button type="button" className="button button-quiet fold" onClick={() => setAll(true)}>
            Show {records.length - WINDOW} earlier records
          </button>
        )}

        {visible.map((r, i) => (
          <Record key={folded ? records.length - WINDOW + i : i} record={r} results={results} />
        ))}

        {streaming && (
          <div className="msg msg-assistant" aria-live="polite" aria-busy="true">
            <span className="eyebrow">Assistant</span>
            {turn.thinking && (
              <details className="thinking">
                <summary className="muted">Thinking</summary>
                <pre className="figure">{turn.thinking}</pre>
              </details>
            )}
            {turn.tools.map((t) => (
              <ToolLine key={t.id} tool={t} />
            ))}
            {/* Plain text while it streams: re-parsing markdown on every delta
                makes a half-written fence flicker between code and prose. The
                re-read after the turn renders it properly, once. */}
            <p className="msg-text">
              {turn.text}
              <span className="caret" aria-hidden="true" />
            </p>
          </div>
        )}

        {turn.state === 'failed' && (
          <Note tone="is-failed" role="alert">
            {turn.detail}
          </Note>
        )}

        {verification && (
          <Note tone={verification.passed ? 'is-proven' : 'is-failed'}>
            Verification {verification.passed ? 'passed' : 'failed'} — {verification.summary}
          </Note>
        )}

        <div ref={foot} />
      </div>

      {!pinned && (
        <button
          type="button"
          className="button button-sm jump"
          onClick={() => {
            setPinned(true)
            foot.current?.scrollIntoView({ block: 'end', behavior: 'smooth' })
          }}
        >
          <Icon name="down" size={14} />
          Newest
        </button>
      )}

      <form
        className="composer"
        onSubmit={(e) => {
          e.preventDefault()
          void send()
        }}
      >
        <textarea
          className="input"
          rows={3}
          value={prompt}
          disabled={streaming}
          placeholder={streaming ? 'Waiting for this turn to finish…' : 'Ask for something'}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={(e) => {
            // Enter sends; Shift+Enter is a newline. A multi-line prompt is
            // common enough that Enter-only would be the wrong default.
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              void send()
            }
          }}
        />
        <div className="composer-tools">
          <select
            className="select"
            aria-label="Permission mode for this turn"
            title="The server clamps this to its ceiling — a turn can ask for less, never more."
            value={mode}
            disabled={streaming}
            onChange={(e) => {
              setMode(e.target.value)
              if (e.target.value) window.localStorage.setItem(MODE_KEY, e.target.value)
              else window.localStorage.removeItem(MODE_KEY)
            }}
          >
            <option value="">server's ceiling</option>
            {MODES.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>

          <input
            className="input composer-model"
            placeholder="model"
            aria-label="Model for this turn"
            spellCheck={false}
            value={model}
            disabled={streaming}
            onChange={(e) => {
              setModel(e.target.value)
              if (e.target.value.trim()) window.localStorage.setItem(MODEL_KEY, e.target.value)
              else window.localStorage.removeItem(MODEL_KEY)
            }}
          />

          <span className="composer-hint">Enter sends · Shift+Enter for a newline · {project}</span>

          {streaming && (
            <button
              type="button"
              className="button button-quiet button-sm"
              onClick={() => abort.current?.abort()}
            >
              Stop
            </button>
          )}
          <button
            type="submit"
            className="button button-primary"
            disabled={streaming || prompt.trim() === ''}
          >
            Send
          </button>
        </div>
      </form>
    </div>
  )
}

/** Fold one streamed event into the in-flight turn. */
export function apply(
  e: TurnEvent,
  setTurn: React.Dispatch<React.SetStateAction<Turn>>,
  setVerification: (v: { passed: boolean; summary: string } | null) => void,
) {
  if (e.type === 'verification') return setVerification({ passed: e.passed, summary: e.summary })

  setTurn((prev) => {
    if (prev.state !== 'streaming') return prev
    switch (e.type) {
      case 'text_delta':
        return { ...prev, text: prev.text + e.text }
      // Kept separate from the answer and folded away by default: this is the
      // model's working-out, not what it is telling you.
      case 'thinking_delta':
        return { ...prev, thinking: prev.thinking + e.text }
      case 'tool_start':
        return { ...prev, tools: [...prev.tools, { id: e.id, name: e.name }] }
      case 'tool_result':
        return {
          ...prev,
          tools: prev.tools.map((t) =>
            t.id === e.id ? { ...t, output: e.output, failed: e.is_error } : t,
          ),
        }
      default:
        return prev
    }
  })
}

/* ── Transcript rendering ──────────────────────────────────────────────── */

type Results = Map<string, { output: string; failed: boolean }>

function Record({ record, results }: { record: SessionRecord; results: Results }) {
  switch (record.kind) {
    case 'session_start':
      return (
        <p className="eyebrow msg-start">
          {record.provider} · <span className="identifier figure">{record.model}</span>
          <span className="faint"> · {clock(record.ts)}</span>
        </p>
      )

    case 'user':
      return (
        <div className="msg msg-user">
          <span className="eyebrow">
            You <span className="faint">{clock(record.ts)}</span>
          </span>
          <p className="msg-text">{record.text}</p>
        </div>
      )

    case 'assistant':
      return (
        <div className="msg msg-assistant">
          <span className="eyebrow">
            Assistant <span className="faint">{clock(record.ts)}</span>
          </span>
          {record.blocks.map((b, i) => (
            <Block key={i} block={b} results={results} />
          ))}
        </div>
      )

    // Rendered under the `tool_use` block that produced it, via `results`.
    case 'tool_result':
      return null

    // What the turn cost, on the ledger axis like every other figure in the
    // panel. These records were read and dropped, which meant the one screen
    // where tokens are actually spent was the one screen that never showed
    // them.
    case 'usage_delta': {
      const line = usageLine(record.usage)
      return line ? <p className="usage figure faint">{line}</p> : null
    }

    case 'stop': {
      const reason = unquote(record.reason)
      return reason === 'end_turn' ? null : (
        <p className="eyebrow msg-start is-asserted">stopped: {reason}</p>
      )
    }
  }
}

/**
 * Token counts as one line, from whichever keys this provider actually used.
 *
 * The map is `Record<string, number>` on the wire because providers disagree
 * about names, so this reads the ones that exist rather than assuming a shape.
 * Cache reads are called out separately: a turn that was 90% cache is a
 * different fact from one that was not, and the totals hide it.
 */
export function usageLine(usage: Record<string, number>): string | null {
  const pick = (...keys: string[]): number => {
    for (const k of keys) if (typeof usage[k] === 'number') return usage[k]
    return 0
  }
  const input = pick('input_tokens', 'prompt_tokens', 'input')
  const output = pick('output_tokens', 'completion_tokens', 'output')
  const cached = pick('cache_read_input_tokens', 'cache_read_tokens', 'cached_tokens')
  if (!input && !output && !cached) return null

  const parts = [`${input.toLocaleString()} in`, `${output.toLocaleString()} out`]
  if (cached) parts.push(`${cached.toLocaleString()} cached`)
  return parts.join(' · ')
}

/**
 * A stop reason, with the quotes the writer left on it.
 *
 * `record_agent_event` stores the reason as `serde_json::to_string(reason)`,
 * which JSON-encodes the enum into a *string literal* — so the field holds
 * `"end_turn"`, quote characters and all, and is then JSON-encoded a second
 * time by the record. The sibling writer (`ContextFact::Stop`) stores the bare
 * name. The panel read the bare form, so every ordinary turn ever written by
 * the first path rendered a spurious `stopped: "end_turn"` line under it.
 *
 * Tolerating both is not optional even if the writer is fixed: the transcripts
 * already on disk keep whichever form wrote them, and this view's whole premise
 * is that the file is the state.
 */
export function unquote(reason: string): string {
  return reason.replace(/^"(.*)"$/s, '$1')
}

/**
 * `2026-08-21T20:05:11Z` → `20:05`.
 *
 * Sessions also carry `epoch:1787755228` — the older stamp, still on disk in
 * every transcript written before the switch, and the reason the first message
 * of an old conversation showed no time at all.
 */
export function clock(ts: string): string {
  if (ts.startsWith('epoch:')) {
    const secs = Number(ts.slice(6))
    if (!Number.isFinite(secs)) return ''
    const d = new Date(secs * 1000)
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`
  }
  const at = ts.indexOf('T')
  return at === -1 ? '' : ts.slice(at + 1, at + 6)
}

function pad(n: number): string {
  return String(n).padStart(2, '0')
}

function Block({ block, results }: { block: ContentBlock; results: Results }) {
  switch (block.type) {
    case 'text':
      // The one change that made this view worth reading: an answer with a
      // fenced code block used to arrive as literal backticks in a paragraph.
      return <Markdown text={block.text} />
    case 'tool_use': {
      const done = results.get(block.id)
      return (
        <ToolLine
          tool={{ id: block.id, name: block.name, output: done?.output, failed: done?.failed }}
          input={block.input}
        />
      )
    }
    case 'tool_result':
      return (
        <ToolLine
          tool={{
            id: block.tool_use_id,
            name: 'result',
            output: block.content,
            failed: block.is_error,
          }}
        />
      )
    case 'thinking':
      return null
    case 'image':
      return <p className="muted figure">[image · {block.media_type}]</p>
  }
}

function ToolLine({ tool, input }: { tool: ToolCall; input?: unknown }) {
  const done = tool.output !== undefined
  return (
    <details className="tool">
      <summary>
        <span className={`glyph ${tool.failed ? 'is-failed' : done ? 'is-proven' : 'is-asserted'}`}>
          {tool.failed ? '✕' : done ? '✓' : '◐'}
        </span>
        <span className="figure">{tool.name}</span>
      </summary>
      {input !== undefined && <Copyable text={stringify(input)} />}
      {tool.output !== undefined && <Copyable text={clamp(tool.output)} full={tool.output} />}
    </details>
  )
}

/**
 * A block of output with a copy button.
 *
 * `full` is the uncut text where the display is clamped: what someone wants on
 * the clipboard is the whole stack trace, not the first 4000 characters of it
 * followed by "… 12000 more".
 *
 * The button reports failure rather than going quiet. `navigator.clipboard` is
 * unavailable on a plain-HTTP non-loopback origin — which is exactly the
 * phone-on-the-LAN case this panel is built for — and a control that silently
 * does nothing there is worse than one that says why.
 */
function Copyable({ text, full }: { text: string; full?: string }) {
  const [state, setState] = useState<'idle' | 'done' | 'failed'>('idle')

  return (
    <div className="copyable">
      <button
        type="button"
        className="button button-quiet button-sm copy"
        onClick={() =>
          void copyText(full ?? text).then((ok) => {
            setState(ok ? 'done' : 'failed')
            window.setTimeout(() => setState('idle'), 1600)
          })
        }
        title={state === 'failed' ? 'The browser refused clipboard access' : 'Copy'}
      >
        <Icon name={state === 'done' ? 'check' : 'copy'} size={14} />
        {state === 'failed' ? 'blocked' : state === 'done' ? 'copied' : 'copy'}
      </button>
      <pre className="figure">{text}</pre>
    </div>
  )
}

/** Tool output can be enormous; the transcript is not a log viewer. */
function clamp(s: string, max = 4000): string {
  return s.length <= max ? s : `${s.slice(0, max)}\n… ${s.length - max} more characters`
}

function stringify(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2) ?? ''
  } catch {
    return String(v)
  }
}
