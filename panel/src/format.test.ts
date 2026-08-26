import { describe, expect, it } from 'vitest'
import { glyph, money, duration } from './Board'
import { elapsedSecs, ordered } from './Runs'
import type { Task, TaskStatus } from './api'

/**
 * The panel's pure logic. Rendering is left to the browser checks; this covers
 * the parts that can be wrong without looking wrong — a dependency order, a
 * rounded figure, a status that silently falls through to a default.
 */

function task(id: string, deps: string[] = [], status: TaskStatus = 'pending'): Task {
  return {
    id,
    role: 'developer',
    title: id,
    goal: '',
    deps,
    writes: [],
    acceptance: [],
    reversibility: 'reversible',
    reversibility_reason: null,
    status,
    agent: null,
    worktree: null,
    usd: 0,
    commits: [],
    outcome: null,
    started_at: null,
    ended_at: null,
    attempts: 0,
  }
}

describe('ordered', () => {
  it('puts a dependency before whatever needs it', () => {
    const out = ordered([task('t3', ['t1', 't2']), task('t1'), task('t2', ['t1'])])
    expect(out.map((o) => o.task.id)).toEqual(['t1', 't2', 't3'])
    expect(out.map((o) => o.depth)).toEqual([0, 1, 2])
  })

  it('uses the longest path, so a task sits below its whole chain', () => {
    // t4 depends on t1 directly and on t3 through a chain. Shortest-path depth
    // would put it at 1, beside t2, above the chain it is actually waiting on.
    const out = ordered([
      task('t1'),
      task('t2', ['t1']),
      task('t3', ['t2']),
      task('t4', ['t1', 't3']),
    ])
    const depth = Object.fromEntries(out.map((o) => [o.task.id, o.depth]))
    expect(depth).toEqual({ t1: 0, t2: 1, t3: 2, t4: 3 })
  })

  it('ignores dependencies on tasks that are not in the run', () => {
    const out = ordered([task('t2', ['ghost'])])
    expect(out).toEqual([{ task: out[0].task, depth: 0 }])
  })

  it('terminates on a dependency cycle instead of hanging the tab', () => {
    // Malformed state.json rather than an expected case — pilot's planner
    // rejects cycles. An unbounded walk here would be a frozen browser.
    const out = ordered([task('a', ['b']), task('b', ['a'])])
    expect(out).toHaveLength(2)
    expect(out.every((o) => Number.isFinite(o.depth))).toBe(true)
  })

  it('handles an empty plan', () => {
    expect(ordered([])).toEqual([])
  })
})

describe('elapsedSecs', () => {
  const started = '2026-08-21T20:00:00+00:00'
  const now = Date.parse('2026-08-21T20:10:00+00:00')

  it('uses the recorded end when there is one', () => {
    const t = { ...task('t1'), started_at: started, ended_at: '2026-08-21T20:05:00+00:00' }
    expect(elapsedSecs(t, true, now)).toBe(300)
    // A finished task reports the same figure whether or not the run is over.
    expect(elapsedSecs(t, false, now)).toBe(300)
  })

  it('counts up while the run is still going', () => {
    const t = { ...task('t1'), started_at: started }
    expect(elapsedSecs(t, false, now)).toBe(600)
  })

  it('does not keep counting after the run has finished', () => {
    // The regression this exists for: a task left in `review` on a failed run
    // has no `ended_at`, and counting from `now` reported 68h of "elapsed
    // work" for a run that died in minutes.
    const t = { ...task('t1'), started_at: started }
    expect(elapsedSecs(t, true, now)).toBeNull()
  })

  it('reports nothing for a task that never started', () => {
    expect(elapsedSecs(task('t1'), false, now)).toBeNull()
  })
})

describe('money', () => {
  it('rounds to cents', () => {
    expect(money(1.0440734999999997)).toBe('$1.04')
    expect(money(0)).toBe('$0.00')
  })

  it('does not round real spend down to free', () => {
    // $0.004 of spend is not $0.00. Saying it is makes a run look free.
    expect(money(0.004)).toBe('<$0.01')
    expect(money(0.01)).toBe('$0.01')
  })

  it('refuses to invent a figure', () => {
    expect(money(Number.NaN)).toBe('—')
  })
})

describe('duration', () => {
  it('matches the board TUI overlay', () => {
    expect(duration(45)).toBe('45s')
    expect(duration(187)).toBe('3m07s')
    expect(duration(3840)).toBe('1h04m')
  })

  it('pads so a column of times stays aligned', () => {
    expect(duration(61)).toBe('1m01s')
    expect(duration(3601)).toBe('1h00m')
  })
})

describe('glyph', () => {
  it('gives every status its own mark, so colour is never the only signal', () => {
    const statuses: TaskStatus[] = [
      'pending',
      'todo',
      'in_progress',
      'review',
      'done',
      'failed',
      'blocked',
    ]
    for (const s of statuses) expect(glyph(s)).not.toBe('')
    // The three that matter must be distinguishable from each other.
    expect(new Set(['done', 'failed', 'blocked'].map((s) => glyph(s as TaskStatus))).size).toBe(3)
  })
})
