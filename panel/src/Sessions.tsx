import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { copyText } from './a11y'
import {
  api,
  ApiError,
  exportUrl,
  type ContentBlock,
  type RewindChange,
  type RewindPoint,
  type SessionRecord,
  type SessionSummary,
  type TurnEvent,
} from './api'
import { classify } from './Changes'
import { Markdown } from './markdown'
import { navigate } from './router'
import { message } from './state'
import { Failed, Icon, Loading, Note } from './ui'
import { MicButton } from './voice'

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
          title="Pick a repository first"
          detail="Chats run inside a repository. Choose one in the sidebar."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  const open = id ?? 'new'
  return (
    <div className={`sessions${id ? ' has-open' : ''}`}>
      <SessionList project={project} current={open} />
      <Conversation key={open} project={project} id={open} />
    </div>
  )
}

/* ── List ──────────────────────────────────────────────────────────────── */

function SessionList({ project, current }: { project: string; current: string }) {
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

  // Re-read when the open conversation changes: a new chat becomes a real
  // session on its first turn, and it should appear in the list right away.
  useEffect(() => {
    void load()
  }, [load, current])

  async function remove(id: string) {
    if (!window.confirm('Delete this chat? Its transcript is removed from disk.')) return
    setNote(null)
    try {
      const res = await api.deleteSession(project, id)
      // The API reports what happened to the search index as well as the file.
      // "The transcript is gone but recall may still find it" is something to
      // learn here, not from a surprise later.
      const deindexed = res.deindexed
      setNote(
        deindexed && typeof deindexed === 'object' && 'error' in deindexed
          ? `Chat deleted, but search may still find it: ${String((deindexed as { error: unknown }).error)}`
          : 'Chat deleted.',
      )
      if (id === current) navigate('/sessions')
      await load()
    } catch (e) {
      setNote(message(e))
    }
  }

  const shown = useMemo(() => matching(sessions ?? [], q), [sessions, q])

  return (
    <aside className="session-list" aria-label="Chats">
      <div className="session-list-head">
        <h1>Chats</h1>
        <button
          type="button"
          className="button button-primary button-sm"
          onClick={() => navigate('/sessions/new')}
          title="Start a new chat"
        >
          <Icon name="plus" size={14} />
          New
        </button>
      </div>

      <label className="filter-search session-search">
        <Icon name="search" size={14} />
        <input
          className="input"
          type="search"
          placeholder="Search chats"
          aria-label="Search chats by prompt, id or model"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
      </label>

      {note && <Note>{note}</Note>}

      {error ? (
        <Note tone="is-failed" role="alert">
          Couldn't load chats: {error}
        </Note>
      ) : !sessions ? (
        <Loading what="chats" />
      ) : sessions.length === 0 ? (
        <p className="session-list-empty">
          No chats yet. Anything you start here also shows up in <code>wingman session list</code>.
        </p>
      ) : shown.length === 0 ? (
        <p className="session-list-empty">No chat mentions “{q.trim()}”.</p>
      ) : (
        <ul className="session-items">
          {shown.map((s) => (
            <li key={s.session_id}>
              <button
                type="button"
                className="session-item"
                aria-current={s.session_id === current ? 'page' : undefined}
                onClick={() => navigate(`/sessions/${s.session_id}`)}
              >
                <span className="session-item-title">
                  {s.first_prompt ?? <span className="faint">Empty chat</span>}
                </span>
                <span className="session-item-meta">
                  {s.model && <span className="truncate">{s.model}</span>}
                  <span>
                    {s.turns} {s.turns === 1 ? 'message' : 'messages'}
                  </span>
                  {s.mtime ? <span>{ago(s.mtime)}</span> : null}
                </span>
              </button>
              <button
                type="button"
                className="button button-quiet button-sm button-icon session-delete"
                onClick={() => void remove(s.session_id)}
                title="Delete this chat"
                aria-label={`Delete chat: ${s.first_prompt ?? s.session_id}`}
              >
                <Icon name="trash" size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </aside>
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
const SPEAK_KEY = 'wingman.turn.speak'

const speakable = () => 'speechSynthesis' in window

/** How many transcript records render before the "show earlier" fold. */
const WINDOW = 150

function Conversation({ project, id }: { project: string; id: string }) {
  const isNew = id === 'new'
  const [records, setRecords] = useState<SessionRecord[] | null>(isNew ? [] : null)
  const [error, setError] = useState<string | null>(null)
  const [prompt, setPrompt] = useState('')
  const [turn, setTurn] = useState<Turn>({ state: 'idle' })
  const [verification, setVerification] = useState<{ passed: boolean; summary: string } | null>(null)
  const [pinned, setPinned] = useState(true)
  const [mode, setMode] = useState(() => window.localStorage.getItem(MODE_KEY) ?? '')
  const [model, setModel] = useState(() => window.localStorage.getItem(MODEL_KEY) ?? '')
  const [details, setDetails] = useState(() => window.innerWidth >= 1280)
  const [speak, setSpeak] = useState(() => window.localStorage.getItem(SPEAK_KEY) === '1')
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

    // The reply is the text after the last tool call; anything before it was
    // the agent narrating its way there.
    let reply = ''
    try {
      await api.turn(
        project,
        target,
        { prompt: text, mode: mode || undefined, model: model.trim() || undefined },
        (e) => {
          if (e.type === 'tool_start') reply = ''
          else if (e.type === 'text_delta') reply += e.text
          apply(e, setTurn, setVerification)
        },
        abort.current.signal,
      )
      if (speak && speakable() && reply.trim())
        window.speechSynthesis.speak(new SpeechSynthesisUtterance(reply))
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
      <div className="chat">
        <Failed
          title="Couldn't open this chat"
          detail={error}
          action={{ label: 'Start a new chat', onClick: () => navigate('/sessions') }}
        />
      </div>
    )
  if (!records) return <Loading what="the chat" />

  const streaming = turn.state === 'streaming'

  const start = records.find((r) => r.kind === 'session_start')
  const firstAsk = records.find((r) => r.kind === 'user')
  const title = firstAsk && firstAsk.kind === 'user' ? firstAsk.text.split('\n')[0] : 'New chat'
  const hero = isNew && !streaming && turn.state !== 'failed'
  const userTurns = records.filter((r) => r.kind === 'user').length

  return (
    <div className={`chat${hero ? ' chat-hero' : ''}${details && !isNew ? ' with-details' : ''}`}>
      <div className="chat-main">
        {!isNew && (
          <header className="chat-head">
            <button
              type="button"
              className="button button-quiet button-sm button-icon chat-back"
              onClick={() => navigate('/sessions')}
              aria-label="Back to chats"
            >
              <Icon name="collapse" size={14} />
            </button>
            <h2 className="truncate" title={title}>
              {title}
            </h2>
            <button
              type="button"
              className="button button-quiet button-sm"
              aria-expanded={details}
              onClick={() => setDetails((v) => !v)}
            >
              <Icon name="system" size={14} />
              Details
            </button>
          </header>
        )}

        {hero && (
          <div className="hero">
            <span className="hero-mark" aria-hidden="true">
              <svg width="22" height="22" viewBox="0 0 20 20" fill="currentColor">
                <path d="M10 2.5 17.5 17 10 13.2 2.5 17Z" />
              </svg>
            </span>
            <h1>What should Wingman do?</h1>
            <p>
              Ask about <strong>{project}</strong> or describe a change. Wingman reads the code,
              makes the edits and checks its work.
            </p>
            <div className="hero-ideas">
              {IDEAS.map((idea) => (
                <button
                  key={idea}
                  type="button"
                  className="chip"
                  onClick={() => setPrompt(idea)}
                >
                  {idea}
                </button>
              ))}
            </div>
          </div>
        )}

      <div className="transcript" ref={scroller} onScroll={onScroll}>
        <Transcript records={records} />

        {streaming && (
          <div className="msg msg-assistant" aria-live="polite" aria-busy="true">
            <span className="msg-who">Wingman</span>
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
          placeholder={
            streaming ? 'Wingman is working…' : isNew ? 'Describe what you want done…' : 'Reply to Wingman…'
          }
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
            <option value="">Default permissions</option>
            {MODES.map((m) => (
              <option key={m} value={m}>
                {MODE_LABELS[m]}
              </option>
            ))}
          </select>

          <input
            className="input composer-model"
            placeholder="Default model"
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

          <span className="composer-hint">Enter to send, Shift+Enter for a new line</span>

          {speakable() && (
            <button
              type="button"
              className="button button-quiet button-sm"
              aria-pressed={speak}
              title="Read the final reply aloud when a turn finishes"
              onClick={() => {
                const next = !speak
                setSpeak(next)
                if (next) window.localStorage.setItem(SPEAK_KEY, '1')
                else {
                  window.localStorage.removeItem(SPEAK_KEY)
                  window.speechSynthesis.cancel()
                }
              }}
            >
              <Icon name="speak" size={15} />
              <span className="sr-only">Read replies aloud</span>
            </button>
          )}

          <MicButton value={prompt} onChange={setPrompt} disabled={streaming} />

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
            className="button button-primary button-icon composer-send"
            disabled={streaming || prompt.trim() === ''}
            aria-label="Send"
            title="Send"
          >
            <Icon name="send" size={16} />
          </button>
        </div>
      </form>
      </div>

      {details && !isNew && (
        <aside className="chat-details" aria-label="Chat details">
          <section>
            <h3>About this chat</h3>
            <dl className="facts">
              <div>
                <dt>Model</dt>
                <dd className="mono">{start?.kind === 'session_start' ? start.model : '—'}</dd>
              </div>
              <div>
                <dt>Provider</dt>
                <dd>{start?.kind === 'session_start' ? start.provider : '—'}</dd>
              </div>
              <div>
                <dt>Messages</dt>
                <dd>{userTurns}</dd>
              </div>
              <div>
                <dt>Chat id</dt>
                <dd className="mono truncate" title={id}>
                  {id}
                </dd>
              </div>
            </dl>
          </section>
          <section>
            <h3>Share</h3>
            <ExportActions project={project} id={id} />
          </section>
          <Rewind
            project={project}
            id={id}
            onRestored={(forked) => (forked ? navigate(`/sessions/${forked}`) : void load())}
          />
        </aside>
      )}
    </div>
  )
}

/** Starting points for an empty chat. They fill the box; nothing is sent. */
const IDEAS = [
  'Explain how this repository is organised',
  'Find a failing test and fix it',
  'Summarise what changed on this branch',
]

const MODE_LABELS: Record<(typeof MODES)[number], string> = {
  'read-only': 'Read only',
  plan: 'Plan first',
  'auto-edit': 'Edit files',
}

/**
 * Export the session's report — summary, files changed, receipts, cost, tool
 * calls, secrets redacted server-side.
 *
 * Copy puts the Markdown on the clipboard for a PR or an issue; the links
 * are server downloads (`Content-Disposition: attachment`) rather than
 * `data:` links, which a sandboxed page cannot open. Copy says when the
 * browser refuses, for the same reason `Copyable` does.
 */
function ExportActions({ project, id }: { project: string; id: string }) {
  const [state, setState] = useState<'idle' | 'done' | 'failed'>('idle')
  const [error, setError] = useState<string | null>(null)

  async function copy() {
    setError(null)
    try {
      const ok = await copyText(await api.exportSession(project, id, 'md'))
      setState(ok ? 'done' : 'failed')
      window.setTimeout(() => setState('idle'), 1600)
    } catch (e) {
      setError(message(e))
    }
  }

  return (
    <div className="actions export-actions">
      <button
        type="button"
        className="button button-sm"
        onClick={() => void copy()}
        title={
          error ??
          (state === 'failed'
            ? 'The browser refused clipboard access — download it instead'
            : 'Copy the report as Markdown')
        }
      >
        <Icon name={state === 'done' ? 'check' : 'copy'} size={14} />
        {error
          ? 'Export failed'
          : state === 'failed'
            ? 'Clipboard blocked'
            : state === 'done'
              ? 'Copied'
              : 'Copy summary'}
      </button>
      {(['md', 'html', 'json'] as const).map((f) => (
        <a
          key={f}
          className="button button-quiet button-sm"
          href={exportUrl(project, id, f, true)}
          title={`Download the summary as ${f.toUpperCase()}`}
        >
          <Icon name="down" size={13} />
          {f.toUpperCase()}
        </a>
      ))}
    </div>
  )
}

/**
 * The rewind timeline: the files each turn of this session edited, and every
 * restore, newest first — the same checkpoints the TUI's `/rewind` lists.
 *
 * Nothing is restored from a row. A row opens a preview of what restoring to
 * before it would change, diff by diff, and only then asks. Restoring puts
 * back every file touched since the point, including by another session, which
 * is why the preview comes first. The restore is itself a checkpoint, so it
 * appears at the top and is undone the same way; truncating the conversation
 * is a separate, explicit box, and forks rather than cuts.
 */
function Rewind({
  project,
  id,
  onRestored,
}: {
  project: string
  id: string
  onRestored: (forked: string | null) => void
}) {
  const [points, setPoints] = useState<RewindPoint[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [open, setOpen] = useState<RewindPoint | null>(null)
  const [changes, setChanges] = useState<RewindChange[] | null>(null)
  const [truncate, setTruncate] = useState(false)
  const [busy, setBusy] = useState(false)
  const [restored, setRestored] = useState<string[] | null>(null)

  const load = useCallback(async () => {
    try {
      setPoints(await api.rewindTimeline(project, id))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project, id])

  useEffect(() => {
    void load()
  }, [load])

  async function preview(p: RewindPoint) {
    setOpen(p)
    setChanges(null)
    setTruncate(false)
    setRestored(null)
    try {
      setChanges((await api.rewindPreview(project, id, p.seq)).changes)
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }

  async function restore(p: RewindPoint, n: number) {
    if (!window.confirm(rewindQuestion(p, n, truncate))) return
    setBusy(true)
    try {
      const r = await api.rewind(project, id, p.seq, truncate)
      setRestored(r.restored)
      setOpen(null)
      setChanges(null)
      setError(null)
      await load()
      onRestored(r.forked_session)
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section aria-label="Undo history" className="rewind">
      <h3>Undo history</h3>
      <p className="rewind-intro">
        Files each message changed. Preview a point to see what undoing back to it would do.
      </p>
      {error && (
        <Note tone="is-failed" role="alert">
          {error}
        </Note>
      )}
      {restored && (
        <Note>
          {restored.length === 0
            ? 'The files were already as they were at that point.'
            : `${restored.join(' · ')} — the restore is on the timeline and can be undone.`}
        </Note>
      )}
      {!points ? (
        !error && <Loading what="the timeline" />
      ) : points.length === 0 ? (
        <p className="rewind-intro">No file changes in this chat yet.</p>
      ) : (
        <div className="rows">
          {points.map((p) => (
            <div key={p.seq} className="row row-wrap">
              <span className="task-meta-block">
                <span>{pointLabel(p)}</span>
                <span className="muted figure">{p.files.join(', ')}</span>
              </span>
              <button
                type="button"
                className="button button-sm"
                aria-expanded={open?.seq === p.seq}
                disabled={busy}
                onClick={() => (open?.seq === p.seq ? setOpen(null) : void preview(p))}
              >
                {open?.seq === p.seq ? 'Close' : 'Preview'}
              </button>
            </div>
          ))}
        </div>
      )}

      {open && changes && (
        <>
          <h4 className="rewind-head">Undo back to before {pointLabel(open)}</h4>
          {changes.length === 0 ? (
            <p className="section-intro">The files are already as they were at this point.</p>
          ) : (
            changes.map((c) => (
              <div key={c.path}>
                <p className="figure">
                  {!c.exists_after ? 'removed' : !c.exists_now ? 'recreated' : 'restored'}{' '}
                  {c.path}
                </p>
                <div className="diff" role="group" aria-label={`What restoring does to ${c.path}`}>
                  {classify(c.diff).map((l, i) => (
                    <div key={i} className={`diff-line diff-${l.kind}`}>
                      <span className="diff-gutter" aria-hidden="true">
                        {l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ''}
                      </span>
                      <span className="diff-text">
                        {(l.kind === 'add' || l.kind === 'del' ? l.text.slice(1) : l.text) || ' '}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            ))
          )}
          <div className="actions">
            {open.turn !== null && (
              <label className="check">
                <input
                  type="checkbox"
                  checked={truncate}
                  onChange={(e) => setTruncate(e.target.checked)}
                />
                Also continue the chat from before this message
              </label>
            )}
            <button
              type="button"
              className="button button-primary button-sm"
              disabled={busy || (changes.length === 0 && !truncate)}
              onClick={() => void restore(open, changes.length)}
            >
              {busy ? 'Undoing…' : 'Undo changes'}
            </button>
          </div>
        </>
      )}
    </section>
  )
}

/** A timeline row's name: the turn and what it was asked, or the restore it is. */
export function pointLabel(p: RewindPoint): string {
  if (p.restore !== null) return `restore to before #${p.restore}`
  if (p.turn === null) return `edits #${p.seq}`
  const asked = (p.prompt ?? '').split('\n')[0]
  const short = asked.length > 60 ? `${asked.slice(0, 60)}…` : asked
  return short ? `turn ${p.turn + 1}: ${short}` : `turn ${p.turn + 1}`
}

/** The confirmation a restore asks, saying what it will and will not do. */
export function rewindQuestion(p: RewindPoint, files: number, truncate: boolean): string {
  const parts = [`Restore ${files} file${files === 1 ? '' : 's'} to before ${pointLabel(p)}?`]
  if (truncate)
    parts.push('The conversation will continue in a copy cut to before this turn; this one is kept.')
  parts.push('The restore is itself a checkpoint, so it can be undone from this timeline.')
  return parts.join(' ')
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

/**
 * A conversation on disk, rendered.
 *
 * Exported because a pilot worker's transcript is a session transcript —
 * the orchestrator mints `pilot-<run>-<agent>` and the worker writes
 * `<project>/.wingman/sessions/<that>.jsonl` like any other conversation. The
 * run screen shows what an agent actually did by handing its records to this,
 * rather than growing a second renderer that would drift from this one the
 * first time a block type is added.
 */
export function Transcript({ records }: { records: SessionRecord[] }) {
  const [all, setAll] = useState(false)

  // In a live stream `tool_start` and `tool_result` are paired by id as they
  // arrive. In a transcript they are separate records — the call is a block
  // inside an assistant message, the result is its own line — so they have to
  // be rejoined here. Without this every tool renders as still running and its
  // output is never shown.
  const results: Results = new Map()
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
    <>
      {folded && (
        <button type="button" className="button button-quiet fold" onClick={() => setAll(true)}>
          Show {records.length - WINDOW} earlier records
        </button>
      )}
      {visible.map((r, i) => (
        <Record key={folded ? records.length - WINDOW + i : i} record={r} results={results} />
      ))}
    </>
  )
}

function Record({ record, results }: { record: SessionRecord; results: Results }) {
  switch (record.kind) {
    case 'session_start':
      return (
        <p className="msg-start">
          Chat started {clock(record.ts) && `at ${clock(record.ts)}`} with{' '}
          <span className="mono">{record.model}</span>
          <span className="faint"> via {record.provider}</span>
        </p>
      )

    case 'user':
      return (
        <div className="msg msg-user">
          <span className="msg-who">
            You <span className="faint">{clock(record.ts)}</span>
          </span>
          <p className="msg-text">{record.text}</p>
        </div>
      )

    case 'assistant':
      return (
        <div className="msg msg-assistant">
          <span className="msg-who">
            Wingman <span className="faint">{clock(record.ts)}</span>
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
        <p className="msg-start is-asserted">Stopped early: {reason.replace(/_/g, ' ')}</p>
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
        {state === 'failed' ? 'Blocked' : state === 'done' ? 'Copied' : 'Copy'}
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
