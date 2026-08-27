import { useCallback, useEffect, useRef, useState } from 'react'
import { api, isTextOutput } from './api'
import { Output } from './Insights'
import { navigate } from './router'
import { message } from './state'
import { Failed, Loading, PageHead } from './ui'

/**
 * Changes — what is different, and whether it is right.
 *
 * Four routes that all answer one question and had nowhere to be asked from:
 * `diff` (what changed in a file), `explain` (what it means), `review` (is it
 * any good), `attest` (what left this machine). They shipped with the daemon
 * and were reachable only as raw text at the bottom of Insights, under a list
 * of report paths.
 *
 * Zero new server code. This is a renderer, and the only judgement in it is
 * which of the four you are looking at.
 */

type Tab = 'diff' | 'explain' | 'review' | 'attest'

const TABS: { id: Tab; label: string; about: string }[] = [
  { id: 'diff', label: 'Diff', about: 'the working-tree diff for one file' },
  { id: 'explain', label: 'Explain', about: 'the current changes, in prose' },
  { id: 'review', label: 'Review', about: 'a PR, or local commits against a base' },
  { id: 'attest', label: 'Attest', about: 'what this machine sent anywhere' },
]

export function Changes({ project }: { project: string | null }) {
  const [tab, setTab] = useState<Tab>('diff')
  const [file, setFile] = useState('')
  const [base, setBase] = useState('')
  const [staged, setStaged] = useState(false)
  const [pr, setPr] = useState('')
  const [result, setResult] = useState<unknown>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const run = useCallback(async () => {
    if (!project) return
    // The route would otherwise answer 500 carrying the CLI's usage text,
    // which is a worse way to learn that this one takes an argument.
    if (tab === 'diff' && !file.trim()) return setError('Name a file to diff.')
    setBusy(true)
    setError(null)
    setResult(null)
    try {
      setResult(await api.report(project, path(tab, { file, base, staged, pr })))
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }, [project, tab, file, base, staged, pr])

  // Only the posture report runs on arrival. `diff` needs a file — it is
  // `git diff -- <file>` underneath and refuses without one. Explain and
  // review can each cost a model call, so they wait to be asked: a screen that
  // spends money when you click its tab is a screen people stop clicking.
  //
  // `run` is held in a ref rather than named as a dependency: it is rebuilt on
  // every keystroke in the argument boxes, and this effect is meant to fire on
  // the tab, not on a half-typed filename.
  const latest = useRef(run)
  latest.current = run
  useEffect(() => {
    setResult(null)
    setError(null)
    if (tab === 'attest') void latest.current()
  }, [tab, project])

  if (!project) {
    return (
      <div className="view">
        <Failed
          title="No project selected"
          detail="Changes are per-repo. Pick one in the header."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }

  const current = TABS.find((t) => t.id === tab)!

  return (
    <div className="view">
      <PageHead
        eyebrow="Changes"
        title="What changed, and whether it holds"
        intro="A file's working-tree diff, an explanation of the current changes, a review of them, and what this machine has sent anywhere. The same four subcommands a terminal runs, in this repo."
      />

      <div className="tabs" role="tablist" aria-label="Change views">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            className="tab"
            aria-selected={t.id === tab}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>

      <p className="section-intro">{current.about}</p>

      <div className="filters">
        {tab === 'diff' && (
          <label className="filter-search">
            <input
              className="input"
              placeholder="Path to a file — e.g. crates/wingman-cli/src/main.rs"
              aria-label="File to diff"
              value={file}
              onChange={(e) => setFile(e.target.value)}
              spellCheck={false}
            />
          </label>
        )}

        {(tab === 'explain' || tab === 'review') && (
          <label className="filter-search">
            <input
              className="input"
              placeholder="Base ref — e.g. main"
              aria-label="Base ref"
              value={base}
              onChange={(e) => setBase(e.target.value)}
              spellCheck={false}
            />
          </label>
        )}

        {tab === 'explain' && (
          <label className="check">
            <input type="checkbox" checked={staged} onChange={(e) => setStaged(e.target.checked)} />
            staged only
          </label>
        )}

        {tab === 'review' && (
          <label className="filter-search">
            <input
              className="input"
              placeholder="PR number"
              aria-label="Pull request number"
              value={pr}
              onChange={(e) => setPr(e.target.value)}
              spellCheck={false}
            />
          </label>
        )}

        <button
          type="button"
          className="button button-primary button-sm"
          disabled={busy}
          onClick={() => void run()}
        >
          {busy
            ? 'Running…'
            : tab === 'attest'
              ? 'Refresh'
              : tab === 'diff'
                ? 'Show diff'
                : `Run ${tab}`}
        </button>
      </div>

      {error && (
        <Failed
          title={`Could not run ${tab}`}
          detail={error}
          action={{ label: 'Try again', onClick: () => void run() }}
        />
      )}

      {busy && !result && <Loading what={tab} />}

      {!busy && !error && result != null && <Result tab={tab} value={result} />}

      {!busy && !error && result == null && tab === 'diff' && (
        <p className="section-intro">
          Name a file. <code>wingman diff</code> is <code>git diff -- &lt;file&gt;</code> underneath,
          so it works one file at a time rather than showing the whole tree.
        </p>
      )}

      {!busy && !error && result == null && (tab === 'explain' || tab === 'review') && (
        <p className="section-intro">
          Nothing run yet. {tab === 'review' ? 'Review' : 'Explain'} asks a model, so it waits to be
          asked.
        </p>
      )}
    </div>
  )
}

/** Build the route tail, encoding only the parameters the route declares. */
export function path(
  tab: Tab,
  args: { file: string; base: string; staged: boolean; pr: string },
): string {
  const q = new URLSearchParams()
  switch (tab) {
    case 'diff':
      // Required, not optional: the subcommand takes a positional FILE.
      q.set('file', args.file.trim())
      return withQuery('diff', q)
    case 'explain':
      if (args.base.trim()) q.set('base', args.base.trim())
      if (args.staged) q.set('staged', '1')
      return withQuery('explain', q)
    case 'review':
      if (args.pr.trim()) q.set('pr', args.pr.trim())
      if (args.base.trim()) q.set('base', args.base.trim())
      return withQuery('review', q)
    default:
      return 'attest'
  }
}

function withQuery(tail: string, q: URLSearchParams): string {
  const s = q.toString()
  return s ? `${tail}?${s}` : tail
}

function Result({ tab, value }: { tab: Tab; value: unknown }) {
  // Only `diff` produces something worth parsing. The other three are prose or
  // a report, and dressing them up as structure they do not have is the thing
  // `Output` exists to avoid.
  //
  // A non-zero exit falls through to `Output` as well: the parser would find
  // no hunks in a usage message and render "No changes in that file", which is
  // a confident wrong answer where the command's own stderr is the right one.
  if (tab === 'diff' && isTextOutput(value) && value.exit === 0) {
    return <Diff text={value.stdout} stderr={value.stderr} />
  }
  return <Output value={value} />
}

/* ── The diff ──────────────────────────────────────────────────────────── */

export type DiffLine = { kind: 'file' | 'hunk' | 'add' | 'del' | 'same'; text: string }

/**
 * `wingman diff` is an **interactive hunk reviewer**, not a diff printer.
 *
 * That is the whole reason this is not a unified-diff parser. Run without a
 * terminal it prints its hunks, offers `[a]ccept / [r]eject`, reads EOF, and
 * quits cleanly having written nothing — so what arrives over HTTP is its own
 * review format, wrapped in ANSI colour codes, with a prompt and a
 * `done: accepted 0` footer on the end:
 *
 * ```
 * === panel/src/ui.tsx -> panel/src/ui.tsx (1 hunk(s)) ===
 * --- hunk 1/1 @ -31,6 +31,15 ---
 *    close: '...',
 * +  changes: '...',
 *  } as const
 * [a]ccept / [r]eject / [s]kip / [q]uit / [?] help: (quitting ...)
 * done: accepted 0, rejected 0, files written 0
 * ```
 *
 * The ANSI codes have to go — unstripped they render as `[32m` in the middle
 * of every added line — and so do the two trailer lines, which describe an
 * interaction that did not happen. Left in, they would read as a report of
 * what this screen just did to the file. It did nothing: the route is a GET.
 */
export function classify(text: string): DiffLine[] {
  return strip(text).replace(CRLF, '\n').split('\n').filter(keep).map(line)
}

const CRLF = /\r\n?/g

/** SGR sequences only — the CLI colours its output and nothing else does. */
export function strip(text: string): string {
  return text.replace(ANSI, '')
}

// Built from a char code rather than written as an escape, so the literal
// control character never ends up pasted into this file.
const ANSI = new RegExp(`${String.fromCharCode(27)}\\[[0-9;]*m`, 'g')

const TRAILER = /^(\[a\]ccept|done: accepted|wingman: )/

/**
 * Drop the interactive footer.
 *
 * `[a]ccept …` and `done: accepted 0` describe a prompt nobody answered.
 */
function keep(text: string): boolean {
  return !TRAILER.test(text.trimStart())
}

const FILE_HEAD = /^===\s/
const HUNK_HEAD = /^---\s+hunk/

function line(text: string): DiffLine {
  if (FILE_HEAD.test(text)) return { kind: 'file', text: text.replace(/^=+\s*|\s*=+$/g, '') }
  if (HUNK_HEAD.test(text)) return { kind: 'hunk', text }
  if (text.startsWith('+')) return { kind: 'add', text }
  if (text.startsWith('-')) return { kind: 'del', text }
  return { kind: 'same', text }
}

/**
 * A diff, in the panel's palette.
 *
 * **Added and removed are not green and red here**, and that is the one
 * deliberate departure from what a diff usually looks like. The stylesheet's
 * single rule is that colour encodes epistemic status — proven, asserted,
 * failed — and a removed line is not a failure. So the two sides are told
 * apart by ground and by a gutter glyph, the same way every other state in the
 * panel carries a second channel: an added line sits on `--raised`, a removed
 * one is `--muted` on `--sunken`.
 *
 * The reasoning is recorded in
 * `docs/decisions/0014-the-diff-is-not-green-and-red.md`, because "make the
 * diff green" is a change someone will otherwise propose as a fix.
 */
function Diff({ text, stderr }: { text: string; stderr: string }) {
  const lines = classify(text)
  const added = lines.filter((l) => l.kind === 'add').length
  const removed = lines.filter((l) => l.kind === 'del').length
  const hunks = lines.filter((l) => l.kind === 'hunk').length

  // What the CLI prints when the file is clean is one clear sentence
  // ("no diff to review"). Framing it under a heading and two zeroed counters
  // would be a worse version of a sentence it already wrote.
  if (added + removed === 0) {
    return <p className="section-intro">No changes in that file.</p>
  }

  return (
    <>
      <div className="rows">
        <div className="row">
          <span className="muted">Hunks</span>
          <span className="figure">{hunks || '—'}</span>
        </div>
        <div className="row">
          <span className="muted">Lines</span>
          <span className="figure">
            +{added} / −{removed}
          </span>
        </div>
      </div>

      <div className="diff" role="group" aria-label="Working-tree diff">
        {lines.map((l, i) =>
          l.kind === 'file' ? (
            <p key={i} className="diff-file">
              {l.text}
            </p>
          ) : (
            <div key={i} className={`diff-line diff-${l.kind}`}>
              <span className="diff-gutter" aria-hidden="true">
                {l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ''}
              </span>
              {/* The marker is in the gutter now, so it is not also left at
                  the head of the text where it would shift every changed line
                  one column out of alignment with its neighbours. */}
              <span className="diff-text">
                {(l.kind === 'add' || l.kind === 'del' ? l.text.slice(1) : l.text) || ' '}
              </span>
            </div>
          ),
        )}
      </div>

      {/* Git's CRLF advisory lands here on Windows on a command that exited
          cleanly. It is a warning about the next checkout, not a failure of
          this read, so it is muted rather than coloured as one. */}
      {stderr.trim() && <pre className="report figure muted">{stderr}</pre>}
    </>
  )
}
