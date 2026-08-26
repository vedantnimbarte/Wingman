import { describe, expect, it, vi } from 'vitest'
import { drainFrames, type TurnEvent } from './api'
import { apply, type Turn } from './Sessions'

/**
 * Streaming a turn is where a bug is least visible: a dropped frame looks like
 * the model simply said less, and a mis-buffered chunk looks like a truncated
 * answer. Both are tested here rather than trusted.
 */

const frame = (o: unknown) => `event: x\ndata: ${JSON.stringify(o)}\n\n`

describe('drainFrames', () => {
  it('reads complete frames and keeps the remainder', () => {
    const { events, rest } = drainFrames(
      frame({ type: 'text_delta', text: 'a' }) + 'data: {"type":"partial',
    )
    expect(events).toEqual([{ type: 'text_delta', text: 'a' }])
    expect(rest).toBe('data: {"type":"partial')
  })

  it('reassembles a frame split across chunks', () => {
    // The failure this guards: a chunk boundary mid-JSON. Parsing the first
    // half would truncate the message; dropping it would lose it entirely.
    const whole = frame({ type: 'text_delta', text: 'hello world' })
    const cut = Math.floor(whole.length / 2)

    const first = drainFrames(whole.slice(0, cut))
    expect(first.events).toEqual([])

    const second = drainFrames(first.rest + whole.slice(cut))
    expect(second.events).toEqual([{ type: 'text_delta', text: 'hello world' }])
    expect(second.rest).toBe('')
  })

  it('reads several frames arriving in one chunk', () => {
    const { events } = drainFrames(
      frame({ type: 'tool_start', id: 't1', name: 'read_file', input: {} }) +
        frame({ type: 'text_delta', text: 'ok' }),
    )
    expect(events.map((e) => e.type)).toEqual(['tool_start', 'text_delta'])
  })

  it('ignores keepalive comments and event-name lines', () => {
    const { events, rest } = drainFrames(`:keepalive\n\n${frame({ type: 'turn_complete' })}`)
    expect(events).toEqual([{ type: 'turn_complete' }])
    expect(rest).toBe('')
  })

  it('tolerates a data line with no space after the colon', () => {
    const { events } = drainFrames('data:{"type":"turn_complete"}\n\n')
    expect(events).toEqual([{ type: 'turn_complete' }])
  })

  it('forwards a non-JSON payload as a log rather than dropping it', () => {
    // The server forwards the child's stray stdout this way. Swallowing it is
    // how a complaint from the agent goes unnoticed.
    const { events } = drainFrames('data: warning: rustc not found\n\n')
    expect(events).toEqual([{ type: 'log', raw: 'warning: rustc not found' }])
  })

  it('returns nothing for an empty buffer', () => {
    expect(drainFrames('')).toEqual({ events: [], rest: '' })
  })
})

describe('apply', () => {
  /** Drive `apply` over a list of events and return the resulting turn. */
  function fold(events: TurnEvent[], start?: Turn) {
    let turn: Turn = start ?? { state: 'streaming', text: '', thinking: '', tools: [] }
    const setTurn = ((fn: (p: Turn) => Turn) => {
      turn = fn(turn)
    }) as React.Dispatch<React.SetStateAction<Turn>>
    const setVerification = vi.fn()
    for (const e of events) apply(e, setTurn, setVerification)
    return { turn, setVerification }
  }

  it('accumulates text deltas in order', () => {
    const { turn } = fold([
      { type: 'text_delta', text: 'Hello' },
      { type: 'text_delta', text: ', ' },
      { type: 'text_delta', text: 'world' },
    ])
    expect(turn).toMatchObject({ text: 'Hello, world' })
  })

  it('keeps thinking apart from the answer', () => {
    // Reasoning is the model's working-out. Folding it into `text` would put
    // it in the reply.
    const { turn } = fold([
      { type: 'thinking_delta', text: 'let me check' },
      { type: 'text_delta', text: 'Done.' },
    ])
    expect(turn).toMatchObject({ thinking: 'let me check', text: 'Done.' })
  })

  it('matches a tool result to the call that produced it', () => {
    const { turn } = fold([
      { type: 'tool_start', id: 'a', name: 'read_file', input: {} },
      { type: 'tool_start', id: 'b', name: 'run_shell', input: {} },
      { type: 'tool_result', id: 'b', output: 'boom', is_error: true },
    ])
    expect(turn.state).toBe('streaming')
    if (turn.state !== 'streaming') return
    expect(turn.tools).toEqual([
      { id: 'a', name: 'read_file' },
      { id: 'b', name: 'run_shell', output: 'boom', failed: true },
    ])
  })

  it('reports verification separately from the turn', () => {
    const { setVerification } = fold([
      { type: 'verification', passed: false, summary: '1 test failed' },
    ])
    expect(setVerification).toHaveBeenCalledWith({ passed: false, summary: '1 test failed' })
  })

  it('ignores events once the turn is no longer streaming', () => {
    // A late frame arriving after an abort must not resurrect a finished turn.
    const { turn } = fold([{ type: 'text_delta', text: 'late' }], { state: 'idle' })
    expect(turn).toEqual({ state: 'idle' })
  })
})
