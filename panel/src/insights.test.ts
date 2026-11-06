import { describe, expect, it } from 'vitest'
import type { Metrics } from './api'
import { suggest } from './Insights'

const quiet = {
  cost: null,
  spend: 10,
  spendPrev: 10,
  days: 30,
  unpriced: 0,
  metrics: null,
  context: null,
  runs: [],
}

const metrics = (over: Partial<Metrics>): Metrics => ({
  sessions: 1,
  turns: 10,
  completed_turns: 10,
  time_to_first_token_ms: { median: null, p90: null, samples: 0 },
  total_tokens: 0,
  tokens_per_completed_task: null,
  gated_turns: 0,
  verified_turns: 0,
  verified_done_rate: null,
  sessions_with_receipt: 0,
  sessions_verified: 0,
  session_verified_done_rate: null,
  routing: [],
  ...over,
})

describe('suggest', () => {
  it('says nothing when nothing stands out', () => {
    expect(suggest(quiet)).toEqual([])
  })

  it('puts failures first and cites the figures behind each card', () => {
    const out = suggest({
      ...quiet,
      unpriced: 4,
      metrics: metrics({ gated_turns: 20, verified_turns: 15, verified_done_rate: 0.75 }),
      runs: [{ run_id: 'r', goal: 'g', status: 'failed', done: 0, total: 1, terminal: true }],
    })
    expect(out.map((s) => s.id)).toEqual(['runs', 'checks', 'unpriced'])
    expect(out[1].basis).toContain('75% of 20')
  })

  it('reports a spend swing only when it is large and not trivial', () => {
    expect(suggest({ ...quiet, spend: 30, spendPrev: 10 }).map((s) => s.id)).toEqual(['swing'])
    expect(suggest({ ...quiet, spend: 0.3, spendPrev: 0.1 })).toEqual([])
    expect(suggest({ ...quiet, spend: 12, spendPrev: 10 })).toEqual([])
  })

  it('ignores a weak pass rate over too few tries to mean anything', () => {
    const routing = [{ task_class: 'docs', model: 'm', passed: 0, total: 2, pass_rate: 0 }]
    expect(suggest({ ...quiet, metrics: metrics({ routing }) })).toEqual([])
  })
})
