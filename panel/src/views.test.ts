import { describe, expect, it } from 'vitest'
import { apply, resolve, scope } from './Board'
import { classify, path, strip } from './Changes'
import { verdict } from './Insights'
import { clockOf, isIrreversible, summarise } from './Runs'
import { ago, clock, matching, unquote, usageLine } from './Sessions'
import type { BoardData, Card, SessionSummary, Task } from './api'

/**
 * The derivations the second pass added.
 *
 * Same rule as `format.test.ts`: only the parts that can be wrong without
 * looking wrong. The board's scoping is here because getting it wrong is what
 * made the Overview report a confident, false `$0.00`.
 */

function card(id: string, project: string, extra: Partial<Card> = {}): Card {
  return {
    id,
    short: id.slice(0, 4),
    title: id,
    goal: '',
    notes: null,
    labels: [],
    archived: false,
    created_at: '2026-08-01T00:00:00Z',
    project,
    project_name: project,
    project_missing: false,
    column: 'backlog',
    run_id: null,
    badges: [],
    rollup: null,
    ...extra,
  }
}

function board(cards: Card[], projectIds: string[]): BoardData {
  return {
    columns: [],
    cards,
    projects: projectIds.map((id) => ({ id, name: id, root: `/${id}`, missing: false })),
  }
}

describe('scope', () => {
  it('narrows to the project when the id resolves', () => {
    const data = board([card('a', 'wingman'), card('b', 'api')], ['wingman', 'api'])
    expect(scope(data, 'wingman').map((c) => c.id)).toEqual(['a'])
  })

  it('shows everything when the id is from the other namespace', () => {
    // `[[serve.projects]].id` is user-chosen; the registry slug is generated.
    // Filtering on an unresolvable id would show an empty board, which reads
    // as "no cards" rather than "wrong key".
    const data = board([card('a', 'wingman')], ['wingman'])
    expect(scope(data, 'my-wingman')).toHaveLength(1)
    expect(resolve(data, 'my-wingman')).toBeNull()
  })

  it('shows everything when no project is selected', () => {
    const data = board([card('a', 'x'), card('b', 'y')], ['x', 'y'])
    expect(scope(data, null)).toHaveLength(2)
  })
})

describe('apply', () => {
  const cards = [
    card('one', 'p', { title: 'Fix the LSP restart storm', labels: ['bug'] }),
    card('two', 'p', { title: 'Add board filters', goal: 'narrow the board' }),
    card('three', 'p', {
      title: 'Bench the runner',
      rollup: {
        status: 'running',
        done: 1,
        total: 3,
        failed: 0,
        blocked: 0,
        review: 0,
        in_progress: 1,
        not_started: 1,
        usd: 1,
        subrows: [],
      },
    }),
  ]
  const none = { q: '', label: null, archived: false, live: false }

  it('passes everything through with an empty filter', () => {
    expect(apply(cards, none)).toHaveLength(3)
  })

  it('matches the goal as well as the title', () => {
    expect(apply(cards, { ...none, q: 'narrow' }).map((c) => c.id)).toEqual(['two'])
  })

  it('is case-insensitive', () => {
    expect(apply(cards, { ...none, q: 'LSP' }).map((c) => c.id)).toEqual(['one'])
    expect(apply(cards, { ...none, q: 'lsp' }).map((c) => c.id)).toEqual(['one'])
  })

  it('narrows by label and by a live run', () => {
    expect(apply(cards, { ...none, label: 'bug' }).map((c) => c.id)).toEqual(['one'])
    expect(apply(cards, { ...none, live: true }).map((c) => c.id)).toEqual(['three'])
  })
})

describe('classify', () => {
  const esc = String.fromCharCode(27)

  const sample = [
    '=== panel/src/ui.tsx -> panel/src/ui.tsx (1 hunk(s)) ===',
    '--- hunk 1/1 @ -31,6 +31,15 ---',
    "   close: 'M5.5 5.5l9 9',",
    `${esc}[32m+  changes: 'M3.5 7.5h9',${esc}[0m`,
    `${esc}[31m-  gone: 'M1 1',${esc}[0m`,
    ' } as const',
    '[a]ccept / [r]eject / [s]kip / [q]uit / [?] help: (quitting without writing)',
    'done: accepted 0, rejected 0, files written 0',
  ].join('\n')

  it('reads the reviewer format, not a unified diff', () => {
    // `wingman diff` is an interactive hunk accepter. What arrives over HTTP is
    // its review output, which is not `git diff` and does not parse as one.
    expect(classify(sample).map((l) => l.kind)).toEqual([
      'file',
      'hunk',
      'same',
      'add',
      'del',
      'same',
    ])
  })

  it('strips the colour codes', () => {
    // Unstripped these render as a literal `[32m` in the middle of every added
    // line, which is the first thing anyone would notice.
    const added = classify(sample).find((l) => l.kind === 'add')!
    expect(added.text).not.toContain('[32m')
    expect(added.text).toBe("+  changes: 'M3.5 7.5h9',")
    expect(strip(`${esc}[1mbold${esc}[0m`)).toBe('bold')
  })

  it('drops the prompt nobody answered', () => {
    // The CLI offers to write the hunk and reads EOF. Rendering its prompt and
    // its `done: accepted 0` footer would read as a report of what this screen
    // just did to the file — and the route is a GET.
    const text = classify(sample)
      .map((l) => l.text)
      .join('\n')
    expect(text).not.toContain('[a]ccept')
    expect(text).not.toContain('done: accepted')
  })

  it('takes the rule off the file heading', () => {
    const [first] = classify('=== a/b.ts -> a/b.ts (2 hunk(s)) ===')
    expect(first).toEqual({ kind: 'file', text: 'a/b.ts -> a/b.ts (2 hunk(s))' })
  })

  it('does not read a hunk header as a deleted line', () => {
    // Captured verbatim from `wingman diff` against a real edit: the heading
    // uses an arrow rather than `->`, the hunk header carries a trailing
    // comment, and — the trap — it *starts with three hyphens*. Checked after
    // the `-` prefix it would classify as a deletion on every hunk in every
    // file.
    const real = [
      '=== panel/src/theme.ts → panel/src/theme.ts (1 hunk(s)) ===',
      '',
      '--- hunk 1/1 @ -42,3 +42,5 ---  // fn nextTheme',
      ' }',
      `${esc}[32m+${esc}[0m`,
      `${esc}[32m+/* temporary probe */${esc}[0m`,
    ].join('\n')
    expect(classify(real).map((l) => l.kind)).toEqual(['file', 'same', 'hunk', 'same', 'add', 'add'])
  })
})

describe('path', () => {
  const args = { file: '', base: '', staged: false, pr: '' }

  it('sends no query for a route that takes none', () => {
    expect(path('attest', args)).toBe('attest')
    expect(path('explain', args)).toBe('explain')
  })

  it('encodes only what each route declares', () => {
    expect(path('diff', { ...args, file: 'src/App.tsx', pr: '42' })).toBe('diff?file=src%2FApp.tsx')
    expect(path('explain', { ...args, base: 'main', staged: true })).toBe(
      'explain?base=main&staged=1',
    )
    expect(path('review', { ...args, pr: '42', base: 'main' })).toBe('review?pr=42&base=main')
  })
})

describe('summarise', () => {
  it('says what each kind of event was', () => {
    expect(summarise({ ev: 'run.start', t: '', goal: 'ship it' })).toBe('run started — ship it')
    expect(summarise({ ev: 'task.status', t: '', id: 't1', status: 'done' })).toBe('t1 → done')
    expect(summarise({ ev: 'task.tool', t: '', id: 't1', tool: 'edit', ok: false })).toBe(
      't1 ran edit (failed)',
    )
    expect(summarise({ ev: 'task.commit', t: '', id: 't1', sha: 'abcdef1234567890' })).toBe(
      't1 committed abcdef12',
    )
  })

  it('renders an event it has never seen rather than dropping it', () => {
    // The variants are Rust's and they gain fields. Silence is the one wrong
    // answer for a log line.
    expect(summarise({ ev: 'run.pr', t: '', id: 'x' })).toBe('run.pr x')
  })
})

describe('isIrreversible', () => {
  const base = { reversibility: 'reversible', reversibility_reason: null } as Task

  it('flags anything pilot did not call reversible', () => {
    expect(isIrreversible(base)).toBe(false)
    expect(isIrreversible({ ...base, reversibility: 'irreversible' })).toBe(true)
    expect(isIrreversible({ ...base, reversibility: 'Irreversible' })).toBe(true)
  })

  it('treats a run predating the field as reversible rather than as a warning', () => {
    expect(isIrreversible({ ...base, reversibility: '' } as Task)).toBe(false)
  })
})

describe('clocks', () => {
  it('takes the time out of an RFC-3339 stamp', () => {
    expect(clockOf('2026-08-21T20:05:11Z')).toBe('20:05:11')
    expect(clock('2026-08-21T20:05:11Z')).toBe('20:05')
  })

  it('reads the older epoch stamp, which is still on disk', () => {
    // Every transcript written before the switch carries this form, and it was
    // why the first message of an old conversation showed no time at all.
    expect(clock('epoch:1787755228')).toMatch(/^\d\d:\d\d$/)
    expect(clock('epoch:nonsense')).toBe('')
  })

  it('returns something for a stamp in an unexpected shape', () => {
    expect(clockOf('not a date')).toBe('not a date')
    expect(clock('not a date')).toBe('')
  })
})

describe('unquote', () => {
  it('takes the quotes a double-encoded stop reason arrived with', () => {
    // `record_agent_event` writes `serde_json::to_string(reason)`, so the
    // stored value is a JSON string literal. Every ordinary turn written that
    // way rendered a spurious `stopped: "end_turn"` line.
    expect(unquote('"end_turn"')).toBe('end_turn')
  })

  it('leaves the bare form the sibling writer produces alone', () => {
    expect(unquote('end_turn')).toBe('end_turn')
    expect(unquote('gate_failed')).toBe('gate_failed')
  })
})

describe('ago', () => {
  const now = Date.parse('2026-08-21T12:00:00Z')
  const at = (iso: string) => ago(Date.parse(iso) / 1000, now)

  it('reads coarsely, which is all a list row needs', () => {
    expect(at('2026-08-21T11:59:30Z')).toBe('just now')
    expect(at('2026-08-21T11:30:00Z')).toBe('30m ago')
    expect(at('2026-08-21T09:00:00Z')).toBe('3h ago')
    expect(at('2026-08-20T12:00:00Z')).toBe('yesterday')
    expect(at('2026-08-18T12:00:00Z')).toBe('3d ago')
  })

  it('never counts backwards from a clock that is ahead', () => {
    expect(at('2026-08-21T12:05:00Z')).toBe('just now')
  })
})

describe('matching', () => {
  const sessions: SessionSummary[] = [
    {
      session_id: '2026-08-21-abc',
      first_prompt: 'why is the index stale',
      model: 'claude-opus-5',
      provider: 'anthropic',
      turns: 3,
      mtime: 1,
    },
    {
      session_id: '2026-08-20-def',
      first_prompt: null,
      model: null,
      provider: null,
      turns: 0,
      mtime: 0,
    },
  ]

  it('searches the prompt, the id and the model', () => {
    expect(matching(sessions, 'stale')).toHaveLength(1)
    expect(matching(sessions, 'opus')).toHaveLength(1)
    expect(matching(sessions, '2026-08-20')).toHaveLength(1)
    expect(matching(sessions, '')).toHaveLength(2)
  })

  it('does not fall over on a session with no prompt yet', () => {
    expect(matching(sessions, 'zzz')).toHaveLength(0)
  })
})

describe('usageLine', () => {
  it('reads whichever names this provider used', () => {
    expect(usageLine({ input_tokens: 1200, output_tokens: 300 })).toBe('1,200 in · 300 out')
    expect(usageLine({ prompt_tokens: 10, completion_tokens: 2 })).toBe('10 in · 2 out')
  })

  it('calls out cache reads, which the totals hide', () => {
    expect(usageLine({ input_tokens: 100, output_tokens: 5, cache_read_input_tokens: 9000 })).toBe(
      '100 in · 5 out · 9,000 cached',
    )
  })

  it('renders nothing rather than a row of zeroes', () => {
    expect(usageLine({})).toBeNull()
    expect(usageLine({ something_else: 4 })).toBeNull()
  })
})

describe('verdict', () => {
  it('gives a line the hue its own glyph already means', () => {
    expect(verdict('✓ rust-analyzer on PATH')).toBe('is-proven')
    expect(verdict('  ✗ no ANTHROPIC_API_KEY')).toBe('is-failed')
    expect(verdict('⚠ index is 9 days old')).toBe('is-asserted')
  })

  it('invents nothing for a line that made no claim', () => {
    expect(verdict('providers')).toBeNull()
    expect(verdict('')).toBeNull()
  })
})
