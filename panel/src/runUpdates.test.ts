import { describe, expect, it } from 'vitest'
import { runUpdate, splitRunUpdates, withRunUpdates, RUN_UPDATES_HEADER } from './runUpdates'

describe('run updates in chat', () => {
  it('keeps only this project’s finished, approval and died events', () => {
    const done = { event: 'run.finished', project: 'repo', status: 'done', text: 'Run finished: "x" (r1)' }
    expect(runUpdate(done, 'repo')).toEqual({ text: 'Run finished: "x" (r1)', tone: 'is-proven' })
    expect(runUpdate({ ...done, status: 'failed' }, 'repo')?.tone).toBe('is-failed')
    expect(runUpdate({ event: 'run.died', project: 'repo', text: 'gone' }, 'repo')?.tone).toBe('is-failed')
    expect(runUpdate({ event: 'run.awaiting_approval', project: 'repo', text: 'a' }, 'repo')?.tone).toBe('is-asserted')
    expect(runUpdate(done, 'other')).toBeNull()
    expect(runUpdate({ event: 'run.started', project: 'repo' }, 'repo')).toBeNull()
  })

  it('frames updates as data and splits them back out for display', () => {
    const sent = withRunUpdates(['Run finished: "x" (r1)', 'Run stopped (r2)'], 'what now?')
    expect(sent.startsWith(RUN_UPDATES_HEADER)).toBe(true)
    expect(splitRunUpdates(sent)).toEqual({
      updates: ['Run finished: "x" (r1)', 'Run stopped (r2)'],
      prompt: 'what now?',
    })
    expect(withRunUpdates([], 'plain')).toBe('plain')
    expect(splitRunUpdates('plain')).toEqual({ updates: [], prompt: 'plain' })
  })
})
