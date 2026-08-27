/**
 * Chart geometry, as functions.
 *
 * Every chart on Insights is drawn from these: CSS bars where a bar is
 * honest, and an SVG path only where the shape is a curve CSS cannot draw.
 * There is still no charting library, for the reason the panel has always
 * given — a dependency and a bundle for shapes that are twenty lines of
 * arithmetic, and a palette rule ("colour means epistemic status, nothing
 * else") that no chart library's defaults respect.
 *
 * The arithmetic lives here rather than inside the components so it can be
 * tested without rendering anything. A log axis that is off by a decade, or a
 * cumulative curve that does not reach 100%, is a chart that lies quietly.
 */

/** A log axis needs whole decades, or its ticks read $0.21, $2.14, $21.40. */
export function decadeBounds(min: number, max: number): { lo: number; hi: number; ticks: number[] } {
  // A floor, because log10(0) is -Infinity and a repo can hold a $0 model.
  const safeMin = Math.max(min, 1e-4)
  const lo = round(Math.pow(10, Math.floor(Math.log10(safeMin))))
  const hi = round(Math.pow(10, Math.ceil(Math.log10(Math.max(max, lo * 10)))))

  const ticks: number[] = []
  // Multiply-and-round rather than accumulate: 0.1 * 10 * 10 drifts to
  // 10.000000000000002, which prints as a tick nobody recognises.
  for (let t = lo; t <= hi * 1.0000001; t = round(t * 10)) ticks.push(t)
  return { lo, hi, ticks }
}

/** Kill float drift at the twelfth digit, where decades accumulate it. */
function round(n: number): number {
  return Number(n.toPrecision(12))
}

/** Where `v` sits on a log axis, as 0–1. Clamped, so nothing renders off-track. */
export function logFraction(v: number, lo: number, hi: number): number {
  if (!(v > lo) || !(hi > lo)) return 0
  const f = (Math.log10(v) - Math.log10(lo)) / (Math.log10(hi) - Math.log10(lo))
  return Math.min(1, Math.max(0, f))
}

/** Running share of the total, one entry per value. */
export function cumulative(values: number[]): number[] {
  const total = values.reduce((a, b) => a + b, 0)
  if (total <= 0) return values.map(() => 0)
  let run = 0
  return values.map((v) => {
    run += v
    return run / total
  })
}

/**
 * How many of `values` (largest first) it takes to reach `target` of the
 * total. The Pareto sentence — "six of twenty-eight tools are half the tax" —
 * is this number, and it is worth more than the chart it labels.
 */
export function coverage(values: number[], target: number): number {
  const running = cumulative(values)
  const i = running.findIndex((f) => f >= target - 1e-9)
  return i === -1 ? values.length : i + 1
}

/** Evenly spaced x for the nth of `count` points across `w`. */
function xAt(i: number, count: number, w: number): number {
  return count <= 1 ? w / 2 : (i / (count - 1)) * w
}

/**
 * A filled area under a series, closed to the baseline.
 *
 * A single point draws as a flat span rather than a dot: one day of data is
 * one day of data, and a lone dot floating in an empty box reads as a
 * rendering failure.
 */
export function areaPath(values: number[], w: number, h: number, ceiling?: number): string {
  if (values.length === 0) return ''
  const max = ceiling ?? Math.max(...values)
  const y = (v: number) => (max > 0 ? h - (v / max) * h : h)

  if (values.length === 1) {
    return `M0 ${y(values[0])} L${w} ${y(values[0])} L${w} ${h} L0 ${h} Z`
  }
  const top = values.map((v, i) => `${i === 0 ? 'M' : 'L'}${xAt(i, values.length, w)} ${y(v)}`)
  return `${top.join(' ')} L${w} ${h} L0 ${h} Z`
}

/** The same series as an open line, for a cumulative curve. */
export function linePath(fractions: number[], w: number, h: number): string {
  if (fractions.length === 0) return ''
  return fractions
    .map((f, i) => `${i === 0 ? 'M' : 'L'}${xAt(i, fractions.length, w)} ${h - f * h}`)
    .join(' ')
}
