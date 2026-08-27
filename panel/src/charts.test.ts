import { describe, expect, it } from 'vitest'
import { areaPath, coverage, cumulative, decadeBounds, linePath, logFraction } from './charts'

describe('decadeBounds', () => {
  it('rounds a real repricing spread out to whole decades', () => {
    // The live spread on this repo: cheapest alternative to dearest.
    const { lo, hi, ticks } = decadeBounds(0.21263571, 42.060045)
    expect(lo).toBe(0.1)
    expect(hi).toBe(100)
    expect(ticks).toEqual([0.1, 1, 10, 100])
  })

  it('never emits a tick that drifted off its decade', () => {
    const { ticks } = decadeBounds(0.001, 5000)
    for (const t of ticks) {
      expect(Math.log10(t)).toBeCloseTo(Math.round(Math.log10(t)), 9)
    }
  })

  it('gives a single value a decade of room rather than a zero-width axis', () => {
    const { lo, hi } = decadeBounds(3, 3)
    expect(hi).toBeGreaterThan(lo)
  })

  it('survives a zero, which log10 does not', () => {
    const { lo, hi } = decadeBounds(0, 0)
    expect(Number.isFinite(lo)).toBe(true)
    expect(hi).toBeGreaterThan(lo)
  })
})

describe('logFraction', () => {
  it('places each decade at an even step', () => {
    expect(logFraction(0.1, 0.1, 100)).toBe(0)
    expect(logFraction(1, 0.1, 100)).toBeCloseTo(1 / 3, 10)
    expect(logFraction(10, 0.1, 100)).toBeCloseTo(2 / 3, 10)
    expect(logFraction(100, 0.1, 100)).toBe(1)
  })

  it('clamps rather than running off the track', () => {
    expect(logFraction(1000, 0.1, 100)).toBe(1)
    expect(logFraction(0.001, 0.1, 100)).toBe(0)
    expect(logFraction(0, 0.1, 100)).toBe(0)
  })
})

describe('cumulative', () => {
  it('ends at exactly 1', () => {
    const running = cumulative([5, 3, 2])
    expect(running[running.length - 1]).toBeCloseTo(1, 12)
    expect(running[0]).toBeCloseTo(0.5, 12)
  })

  it('does not divide by a zero total', () => {
    expect(cumulative([0, 0])).toEqual([0, 0])
    expect(cumulative([])).toEqual([])
  })
})

describe('coverage', () => {
  it('counts how many of the largest items reach the share', () => {
    // 50 + 30 = 80% of 100, so two items cover 80%.
    expect(coverage([50, 30, 15, 5], 0.8)).toBe(2)
    expect(coverage([50, 30, 15, 5], 0.5)).toBe(1)
  })

  it('reaches the target on the last item when the spread is flat', () => {
    expect(coverage([25, 25, 25, 25], 1)).toBe(4)
  })

  it('is not fooled by floating point on an exact boundary', () => {
    // Three thirds sum to 0.99999…; asking for 100% must still be 3, not 4.
    expect(coverage([1, 1, 1], 1)).toBe(3)
  })
})

describe('paths', () => {
  it('closes an area back to the baseline', () => {
    const d = areaPath([1, 2], 100, 50)
    expect(d.startsWith('M0 ')).toBe(true)
    expect(d.endsWith('Z')).toBe(true)
    // The peak sits on the top edge, the trough halfway down.
    expect(d).toContain('L100 0')
  })

  it('draws one day as a span, not a dot', () => {
    const d = areaPath([4], 100, 50)
    expect(d).toContain('L100 0')
    expect(d.endsWith('Z')).toBe(true)
  })

  it('flattens an all-zero series onto the baseline instead of dividing by zero', () => {
    const d = areaPath([0, 0, 0], 100, 50)
    expect(d).not.toContain('NaN')
    expect(d).toContain('M0 50')
  })

  it('shares an x-scale between the area and the curve drawn over it', () => {
    const values = [1, 2, 3]
    const area = areaPath(values, 90, 40)
    const line = linePath(cumulative(values), 90, 40)
    // Both put their middle point at the same x, or the curve would not sit
    // over the columns it describes.
    expect(area).toContain('45 ')
    expect(line).toContain('45 ')
    // A cumulative curve has to finish at the top of the box.
    expect(line.endsWith('90 0')).toBe(true)
  })

  it('returns nothing for nothing', () => {
    expect(areaPath([], 10, 10)).toBe('')
    expect(linePath([], 10, 10)).toBe('')
  })
})
