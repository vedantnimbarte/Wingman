import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  type ApiSchema,
  type ContextReport,
  type CostReport,
  type CostTimeline,
  type Metrics,
  type RouteInfo,
  type RunSummary,
} from './api'
import { money } from './Board'
import {
  arcPath,
  arcs,
  areaPath,
  change,
  coverage,
  cumulative,
  decadeBounds,
  linePath,
  logFraction,
  niceCeil,
} from './charts'
import './insights.css'
import { Output } from './output'
import { navigate } from './router'
import { runClass, runGlyph, runLabel } from './Runs'
import { message } from './state'
import { Empty, Failed, Icon, Loading, PageHead, Pill, type IconName } from './ui'

/**
 * Insights — what this repo costs, how that is changing, and how well the
 * work holds up.
 *
 * Laid out as a dashboard. One period switch drives the top of the page: four
 * figures with their change against the previous period, and a daily spend
 * chart that draws the previous period behind this one, so "is this normal?"
 * is answered by looking. Beside it, a short list of things worth a look, each
 * derived from a number on this page and citing it — nothing is guessed.
 *
 * Two scopes, stated wherever they appear: the daily timeline, metrics and
 * runs are this repository; the per-model totals come from
 * `~/.wingman/usage.json`, which is every repository on this machine.
 *
 * Colour: charts are the accent, which never means a state. Green, amber and
 * red appear only where a verdict is being reported — checks that passed or
 * failed, and how runs ended.
 */

/** Periods offered. Thirty is the default because a month is how spend is asked about. */
const WINDOWS = [7, 30, 90] as const

export function Insights({ project }: { project: string | null }) {
  if (!project) {
    return (
      <div className="view">
        <Failed
          title="Pick a repository first"
          detail="Insights are per repository. Choose one in the sidebar."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  return <Dashboard project={project} />
}

/* ── Data ──────────────────────────────────────────────────────────────── */

type Load<T> = { value: T | null; error: string | null; reload: () => void }

/**
 * One request, its result and its failure. The previous value is kept while a
 * reload is in flight, so switching the period does not blank the page.
 */
function useLoad<T>(fetch: () => Promise<T>, deps: unknown[]): Load<T> {
  const [value, setValue] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [n, setN] = useState(0)
  const run = useCallback(fetch, deps)

  useEffect(() => {
    let live = true
    run()
      .then((v) => {
        if (!live) return
        setValue(v)
        setError(null)
      })
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [run, n])

  return { value, error, reload: () => setN((x) => x + 1) }
}

/** How many runs are priced individually — one request each, so it is capped and says so. */
const RECENT = 10

type PricedRuns = { all: RunSummary[]; priced: { run: RunSummary; usd: number }[] }

async function loadRuns(project: string): Promise<PricedRuns> {
  const all = await api.runs(project)
  const priced = await Promise.all(
    all.slice(0, RECENT).map(async (run) => {
      try {
        return { run, usd: (await api.run(project, run.run_id)).totals.usd }
      } catch {
        return { run, usd: 0 }
      }
    }),
  )
  return { all, priced }
}

/** The fetched timeline split into this period and the one before it. */
type Periods = {
  cur: CostTimeline['days']
  prev: CostTimeline['days']
  timeline: CostTimeline
}

function split(t: CostTimeline | null, days: number): Periods | null {
  if (!t) return null
  // Twice the window was asked for. A short history returns fewer days, in
  // which case the previous period is whatever is left, possibly nothing.
  const cur = t.days.slice(-days)
  const prev = t.days.slice(0, Math.max(0, t.days.length - days)).slice(-days)
  return { cur, prev, timeline: t }
}

const sum = (xs: number[]) => xs.reduce((a, b) => a + b, 0)

/* ── Page ──────────────────────────────────────────────────────────────── */

function Dashboard({ project }: { project: string }) {
  const [days, setDays] = useState<number>(30)

  // Always with `compare`: repricing the same token volume against other
  // models is half the point of the spend section.
  const cost = useLoad(() => api.cost(project, true), [project])
  const timeline = useLoad(() => api.costTimeline(project, days * 2), [project, days])
  const metrics = useLoad(() => api.metrics(project), [project])
  const context = useLoad(() => api.context(project), [project])
  const runs = useLoad(() => loadRuns(project), [project])

  const periods = split(timeline.value, days)

  return (
    <div className="view insights">
      <PageHead
        title="Insights"
        intro="What Wingman costs in this repository, how that is changing, and how often its work passes its own checks."
        actions={
          <div className="ins-period" role="group" aria-label="Period">
            {WINDOWS.map((w) => (
              <button
                key={w}
                type="button"
                aria-pressed={days === w}
                onClick={() => setDays(w)}
              >
                {w} days
              </button>
            ))}
          </div>
        }
      />

      <Kpis periods={periods} days={days} metrics={metrics.value} error={timeline.error} />

      <div className="ins-hero">
        <SpendChart periods={periods} days={days} error={timeline.error} onRetry={timeline.reload} />
        <Suggestions
          cost={cost.value}
          periods={periods}
          days={days}
          metrics={metrics.value}
          context={context.value}
          runs={runs.value}
        />
      </div>

      <Models load={cost} />

      <div className="ins-grid">
        <Alternatives cost={cost.value} />
        <Overhead load={context} />
      </div>

      <div className="ins-grid">
        <Quality load={metrics} />
        <Outcomes runs={runs.value} />
      </div>

      <Reports project={project} />
    </div>
  )
}

/** A titled card. Every block on this page is one. */
function Card({
  title,
  sub,
  aside,
  id,
  className = '',
  children,
}: {
  title: string
  sub?: React.ReactNode
  aside?: React.ReactNode
  id?: string
  className?: string
  children: React.ReactNode
}) {
  return (
    <section className={`ins-card ${className}`} id={id} aria-label={title}>
      <header className="ins-card-head">
        <div>
          <h2>{title}</h2>
          {sub && <p className="ins-card-sub">{sub}</p>}
        </div>
        {aside}
      </header>
      {children}
    </section>
  )
}

/* ── KPIs ──────────────────────────────────────────────────────────────── */

function Kpis({
  periods,
  days,
  metrics,
  error,
}: {
  periods: Periods | null
  days: number
  metrics: Metrics | null
  error: string | null
}) {
  const cur = periods?.cur ?? []
  const prev = periods?.prev ?? []
  const spend = sum(cur.map((d) => d.usd))
  const spendPrev = sum(prev.map((d) => d.usd))
  const turns = sum(cur.map((d) => d.turns))
  const turnsPrev = sum(prev.map((d) => d.turns))
  const per = turns ? spend / turns : 0
  const perPrev = turnsPrev ? spendPrev / turnsPrev : 0
  const vs = `vs previous ${days} days`
  const ready = periods !== null

  return (
    <div className="ins-kpis">
      <Kpi
        icon="coin"
        label="Spent in this repo"
        value={ready ? money(spend) : '—'}
        delta={ready ? change(spend, spendPrev) : null}
        note={error ? 'Could not load the timeline' : `Last ${days} days, ${vs}`}
        spark={cur.map((d) => d.usd)}
      />
      <Kpi
        icon="sessions"
        label="Messages"
        value={ready ? turns.toLocaleString() : '—'}
        delta={ready ? change(turns, turnsPrev) : null}
        note={`Answered by Wingman, ${vs}`}
        spark={cur.map((d) => d.turns)}
      />
      <Kpi
        icon="tasks"
        label="Cost per message"
        value={ready && turns ? money(per) : '—'}
        delta={ready ? change(per, perPrev) : null}
        note="Spend divided by messages"
        spark={cur.map((d) => (d.turns ? d.usd / d.turns : 0))}
      />
      <Kpi
        icon="check"
        label="Checks passed"
        value={rate(metrics?.verified_done_rate ?? null)}
        note={
          metrics && metrics.gated_turns
            ? `${metrics.verified_turns} of ${metrics.gated_turns} checked messages, all time`
            : 'No checked messages yet'
        }
        meter={metrics?.verified_done_rate ?? null}
      />
    </div>
  )
}

function Kpi({
  icon,
  label,
  value,
  delta,
  note,
  spark,
  meter,
}: {
  icon: IconName
  label: string
  value: string
  delta?: number | null
  note: string
  spark?: number[]
  meter?: number | null
}) {
  return (
    <div className="ins-kpi">
      <span className="ins-kpi-label">
        <span className="ins-kpi-icon" aria-hidden="true">
          <Icon name={icon} size={14} />
        </span>
        {label}
      </span>
      <div className="ins-kpi-body">
        <div className="ins-kpi-figure">
          <span className="ins-kpi-value">{value}</span>
          {delta !== undefined && <Delta value={delta} />}
        </div>
        {spark && spark.some((v) => v > 0) && <Spark values={spark} />}
        {meter != null && (
          <span
            className="ins-ring"
            style={{ '--fill': meter } as React.CSSProperties}
            aria-hidden="true"
          />
        )}
      </div>
      <span className="ins-kpi-note">{note}</span>
    </div>
  )
}

/**
 * A change as a small neutral pill. Not green or red: spend going up is not a
 * failure and going down is not a success, so it gets an arrow, not a verdict.
 */
function Delta({ value }: { value: number | null }) {
  if (value === null) return <span className="ins-delta is-none">no earlier data</span>
  const pct = Math.round(value * 100)
  const dir = pct > 0 ? 'up' : pct < 0 ? 'down' : 'flat'
  return (
    <span className={`ins-delta is-${dir}`} title="Change against the previous period">
      {dir === 'up' ? '↑' : dir === 'down' ? '↓' : '→'} {Math.abs(pct)}%
    </span>
  )
}

/** A sparkline: shape only, no axis. The figure beside it carries the number. */
function Spark({ values }: { values: number[] }) {
  const W = 96
  const H = 32
  const peak = Math.max(...values, 0)
  const scaled = values.map((v) => (peak > 0 ? v / peak : 0))
  return (
    <svg className="ins-spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true">
      <path className="ins-spark-fill" d={areaPath(values, W, H)} />
      <path className="ins-spark-line" d={linePath(scaled, W, H)} vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

/* ── The spend chart ───────────────────────────────────────────────────── */

/**
 * Daily spend, this period over the last.
 *
 * The previous period is drawn behind this one, aligned day for day, so a
 * spike reads as a spike only if it is one. Hover, tap or arrow-key along it
 * for a day's figures; the readout is also announced, so the chart is usable
 * without seeing it.
 */
function SpendChart({
  periods,
  days,
  error,
  onRetry,
}: {
  periods: Periods | null
  days: number
  error: string | null
  onRetry: () => void
}) {
  const [style, setStyle] = useState<'bars' | 'line'>('bars')
  const [at, setAt] = useState<number | null>(null)
  const plot = useRef<HTMLDivElement | null>(null)

  if (error)
    return (
      <Card title="Daily spend">
        <Failed title="Couldn't load daily spend" detail={error} action={{ label: 'Try again', onClick: onRetry }} />
      </Card>
    )
  if (!periods) return <Card title="Daily spend"><Loading what="daily spend" /></Card>

  const { cur, prev, timeline } = periods
  const values = cur.map((d) => d.usd)
  // The previous period, right-aligned to this one so day N sits over day N.
  const ghost = cur.map((_, i) => prev[prev.length - cur.length + i]?.usd ?? null)
  const total = sum(values)
  const prevTotal = sum(prev.map((d) => d.usd))
  const top = niceCeil(Math.max(...values, ...ghost.map((v) => v ?? 0), 0))
  const avg = cur.length ? total / cur.length : 0
  const n = values.length

  const W = 720
  const H = 220
  const y = (v: number) => H - (v / top) * H
  const slot = W / Math.max(n, 1)
  const xLine = (i: number) => (n <= 1 ? W / 2 : (i / (n - 1)) * W)
  const xMid = (i: number) => (style === 'bars' ? i * slot + slot / 2 : xLine(i))

  function pick(clientX: number) {
    const box = plot.current?.getBoundingClientRect()
    if (!box || n === 0) return
    const f = Math.min(1, Math.max(0, (clientX - box.left) / box.width))
    setAt(style === 'bars' ? Math.min(n - 1, Math.floor(f * n)) : Math.round(f * (n - 1)))
  }

  function onKey(e: React.KeyboardEvent) {
    if (n === 0) return
    if (e.key === 'ArrowRight') setAt((a) => Math.min(n - 1, (a ?? -1) + 1))
    else if (e.key === 'ArrowLeft') setAt((a) => Math.max(0, (a ?? n) - 1))
    else if (e.key === 'Home') setAt(0)
    else if (e.key === 'End') setAt(n - 1)
    else if (e.key === 'Escape') setAt(null)
    else return
    e.preventDefault()
  }

  const day = at !== null ? cur[at] : null
  const before = at !== null ? ghost[at] : null
  const ticks = [top, top / 2, 0]
  const labels = n ? [0, Math.floor((n - 1) / 2), n - 1] : []

  return (
    <Card
      title="Daily spend"
      sub="This repository's chats and runs, priced by day."
      className="ins-chart-card"
      aside={
        <div className="ins-toggle" role="group" aria-label="Chart style">
          {(['bars', 'line'] as const).map((s) => (
            <button key={s} type="button" aria-pressed={style === s} onClick={() => setStyle(s)}>
              {s === 'bars' ? 'Bars' : 'Line'}
            </button>
          ))}
        </div>
      }
    >
      <div className="ins-legend">
        <span>
          <i className="ins-swatch" aria-hidden="true" />
          Last {days} days <strong>{money(total)}</strong>
        </span>
        <span>
          <i className="ins-swatch is-ghost" aria-hidden="true" />
          Previous {days} days <strong>{prev.length ? money(prevTotal) : '—'}</strong>
        </span>
        <span className="ins-legend-avg">
          <i className="ins-swatch is-avg" aria-hidden="true" />
          Daily average <strong>{money(avg)}</strong>
        </span>
      </div>

      {total === 0 && prevTotal === 0 ? (
        <Empty title={timeline.last_day ? 'Nothing spent in this period' : 'No spend in this repository yet'}>
          {timeline.last_day
            ? `The last day anything was spent here was ${timeline.last_day}. Pick a longer period to see it.`
            : 'Start a chat or a run in this repository and each day it costs anything lands here.'}
        </Empty>
      ) : (
        <div className="ins-chart">
          <div className="ins-yaxis" aria-hidden="true">
            {ticks.map((t) => (
              <span key={t}>{t === 0 ? '$0' : money(t)}</span>
            ))}
          </div>
          <div
            className="ins-plot"
            ref={plot}
            tabIndex={0}
            role="img"
            aria-label={`Daily spend over the last ${days} days, ${money(total)} in total. Use the arrow keys to read each day.`}
            onMouseMove={(e) => pick(e.clientX)}
            onMouseLeave={() => setAt(null)}
            onTouchStart={(e) => pick(e.touches[0].clientX)}
            onTouchMove={(e) => pick(e.touches[0].clientX)}
            onKeyDown={onKey}
            onBlur={() => setAt(null)}
          >
            <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" aria-hidden="true">
              {ticks.map((t) => (
                <line key={t} className="ins-gridline" x1="0" x2={W} y1={y(t)} y2={y(t)} vectorEffect="non-scaling-stroke" />
              ))}
              {style === 'bars' ? (
                <>
                  {ghost.map((v, i) =>
                    v ? (
                      <rect key={`g${i}`} className="ins-ghost-bar" x={i * slot + slot * 0.08} width={slot * 0.84} y={y(v)} height={H - y(v)} />
                    ) : null,
                  )}
                  {values.map((v, i) => (
                    <rect
                      key={i}
                      className={`ins-bar${at === i ? ' is-on' : ''}`}
                      x={i * slot + slot * 0.2}
                      width={slot * 0.6}
                      y={y(v)}
                      height={H - y(v)}
                    />
                  ))}
                </>
              ) : (
                <>
                  <path
                    className="ins-ghost-line"
                    d={linePath(ghost.map((v) => (v ?? 0) / top), W, H)}
                    vectorEffect="non-scaling-stroke"
                  />
                  <path className="ins-area" d={areaPath(values, W, H, top)} />
                  <path className="ins-line" d={linePath(values.map((v) => v / top), W, H)} vectorEffect="non-scaling-stroke" />
                </>
              )}
              <line className="ins-avg" x1="0" x2={W} y1={y(avg)} y2={y(avg)} vectorEffect="non-scaling-stroke" />
            </svg>

            {at !== null && day && (
              <>
                <span className="ins-guide" style={{ left: `${(xMid(at) / W) * 100}%` }} aria-hidden="true" />
                <span
                  className="ins-dot"
                  style={{ left: `${(xMid(at) / W) * 100}%`, top: `${(y(day.usd) / H) * 100}%` }}
                  aria-hidden="true"
                />
                <div
                  className={`ins-tip${xMid(at) > W * 0.6 ? ' is-left' : ''}`}
                  style={{ left: `${(xMid(at) / W) * 100}%` }}
                >
                  <span className="ins-tip-date">{formatDate(day.date, true)}</span>
                  <span className="ins-tip-row">
                    <i className="ins-swatch" aria-hidden="true" />
                    {money(day.usd)}
                    <span>{day.turns} {day.turns === 1 ? 'message' : 'messages'}</span>
                  </span>
                  <span className="ins-tip-row is-ghost">
                    <i className="ins-swatch is-ghost" aria-hidden="true" />
                    {before == null ? '—' : money(before)}
                    <span>previous period</span>
                  </span>
                  <span className="ins-tip-foot">
                    {compactNum(day.input_tokens)} in · {compactNum(day.output_tokens)} out
                    {day.cache_read_tokens > 0 && ` · ${compactNum(day.cache_read_tokens)} from cache`}
                  </span>
                </div>
              </>
            )}
          </div>
          <div className="ins-xaxis" aria-hidden="true">
            {labels.map((i) => (
              <span key={i}>{cur[i] ? formatDate(cur[i].date) : ''}</span>
            ))}
          </div>
          <p className="sr-only" aria-live="polite">
            {day ? `${formatDate(day.date, true)}: ${money(day.usd)}, ${day.turns} messages.` : ''}
          </p>
        </div>
      )}
    </Card>
  )
}

/** `2026-11-04` → "Nov 4" (or "Wed, Nov 4"). Parsed as a calendar date, not a UTC instant. */
function formatDate(iso: string, long = false): string {
  const [y, m, d] = iso.split('-').map(Number)
  if (!y || !m || !d) return iso
  return new Date(y, m - 1, d).toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    ...(long ? { weekday: 'short' } : {}),
  })
}

function compactNum(n: number): string {
  return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(n)
}

/* ── Worth a look ──────────────────────────────────────────────────────── */

export type Suggestion = {
  id: string
  tone: 'is-failed' | 'is-asserted' | 'is-info'
  title: string
  /** The number the suggestion rests on, so it can be checked. */
  basis: string
  action?: { label: string; to: string }
}

/**
 * Things on this page worth a look, ranked.
 *
 * Every rule reads a figure that is on screen and cites it. No rule
 * recommends a model: a cheaper model that needs three attempts is not
 * cheaper, so a price gap is reported as a price gap and nothing more.
 */
export function suggest(input: {
  cost: CostReport | null
  spend: number
  spendPrev: number
  days: number
  unpriced: number
  metrics: Metrics | null
  context: ContextReport | null
  runs: RunSummary[]
}): Suggestion[] {
  const out: Suggestion[] = []
  const { cost, metrics, context } = input

  const failedRuns = input.runs.filter((r) => r.status === 'failed').length
  if (failedRuns > 0)
    out.push({
      id: 'runs',
      tone: 'is-failed',
      title: `${failedRuns} ${failedRuns === 1 ? 'run' : 'runs'} failed`,
      basis: `${failedRuns} of ${input.runs.length} runs in this repository ended in a failure.`,
      action: { label: 'Open runs', to: '/runs' },
    })

  if (metrics && metrics.gated_turns >= 5 && (metrics.verified_done_rate ?? 1) < 0.9) {
    const red = metrics.gated_turns - metrics.verified_turns
    out.push({
      id: 'checks',
      tone: 'is-failed',
      title: `${red} changes failed their checks`,
      basis: `${rate(metrics.verified_done_rate)} of ${metrics.gated_turns} checked messages passed. Below 90% is worth reading why.`,
      action: { label: 'Open chats', to: '/sessions' },
    })
  }

  const weak = metrics?.routing
    .filter((r) => r.total >= 5 && r.pass_rate < 0.5)
    .sort((a, b) => a.pass_rate - b.pass_rate)[0]
  if (weak)
    out.push({
      id: 'routing',
      tone: 'is-asserted',
      title: `${weak.model} struggles with ${weak.task_class} tasks`,
      basis: `It passed ${weak.passed} of ${weak.total} (${rate(weak.pass_rate)}). Another model may be a better default for this kind of work.`,
      action: { label: 'See pass rates', to: '#routing' },
    })

  if (input.unpriced > 0)
    out.push({
      id: 'unpriced',
      tone: 'is-asserted',
      title: `${input.unpriced} messages have no price`,
      basis: 'Their model is not in the pricing table, so the spend shown here is lower than the real cost.',
      action: { label: 'Open settings', to: '/config' },
    })

  const swing = change(input.spend, input.spendPrev)
  if (swing !== null && Math.abs(swing) >= 0.5 && input.spend >= 1)
    out.push({
      id: 'swing',
      tone: 'is-info',
      title: `Spend is ${swing > 0 ? 'up' : 'down'} ${Math.round(Math.abs(swing) * 100)}%`,
      basis: `${money(input.spend)} in the last ${input.days} days, against ${money(input.spendPrev)} in the ${input.days} before.`,
    })

  if (cost && cost.total_usd > 0) {
    const used = new Set(cost.rows.map((r) => r.key))
    const cheapest = cost.comparison
      .filter((c) => !used.has(c.model))
      .sort((a, b) => a.would_cost_usd - b.would_cost_usd)[0]
    if (cheapest && cost.total_usd - cheapest.would_cost_usd >= 1)
      out.push({
        id: 'price',
        tone: 'is-info',
        title: `${cheapest.model} would have cost ${money(cost.total_usd - cheapest.would_cost_usd)} less`,
        basis: `Same tokens repriced: ${money(cheapest.would_cost_usd)} against ${money(cost.total_usd)} paid. A price, not a recommendation.`,
        action: { label: 'Compare models', to: '#alternatives' },
      })
  }

  const tool = context ? [...context.tools].sort((a, b) => b.tokens - a.tokens)[0] : undefined
  if (context && tool && context.first_turn_tokens > 0 && tool.tokens / context.first_turn_tokens >= 0.15)
    out.push({
      id: 'tool',
      tone: 'is-info',
      title: `${tool.name} is ${Math.round((tool.tokens / context.first_turn_tokens) * 100)}% of every message's overhead`,
      basis: `Its description costs ${tool.tokens.toLocaleString()} tokens before anything is typed. Turning off tools you never use makes every message cheaper.`,
      action: { label: 'See overhead', to: '#overhead' },
    })

  return out
}

function Suggestions({
  cost,
  periods,
  days,
  metrics,
  context,
  runs,
}: {
  cost: CostReport | null
  periods: Periods | null
  days: number
  metrics: Metrics | null
  context: ContextReport | null
  runs: PricedRuns | null
}) {
  const list = useMemo(
    () =>
      suggest({
        cost,
        spend: sum(periods?.cur.map((d) => d.usd) ?? []),
        spendPrev: sum(periods?.prev.map((d) => d.usd) ?? []),
        days,
        unpriced: periods?.timeline.unpriced_turns ?? 0,
        metrics,
        context,
        runs: runs?.all ?? [],
      }),
    [cost, periods, days, metrics, context, runs],
  )
  const [all, setAll] = useState(false)
  const shown = all ? list : list.slice(0, 3)

  function go(to: string) {
    if (to.startsWith('#')) document.getElementById(to.slice(1))?.scrollIntoView({ behavior: 'smooth', block: 'start' })
    else navigate(to)
  }

  return (
    <Card
      title="Worth a look"
      sub="Worked out from the numbers on this page."
      className="ins-suggest"
      aside={list.length > 0 ? <span className="count">{list.length}</span> : undefined}
    >
      {list.length === 0 ? (
        <div className="ins-suggest-empty">
          <span className="ins-ok" aria-hidden="true">
            <Icon name="check" />
          </span>
          <p>Nothing stands out. Spend is steady, checks are passing and every message is priced.</p>
        </div>
      ) : (
        <ul className="ins-suggest-list">
          {shown.map((s) => (
            <li key={s.id} className={`ins-tip-card ${s.tone}`}>
              <span className={`lamp ${s.tone === 'is-info' ? '' : s.tone}`} aria-hidden="true" />
              <div>
                <p className="ins-tip-title">{s.title}</p>
                <p className="ins-tip-basis">{s.basis}</p>
                {s.action && (
                  <button type="button" className="button button-sm" onClick={() => go(s.action!.to)}>
                    {s.action.label}
                  </button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
      {list.length > 3 && (
        <button type="button" className="button button-quiet button-sm" onClick={() => setAll((a) => !a)}>
          {all ? 'Show fewer' : `Show ${list.length - 3} more`}
        </button>
      )}
    </Card>
  )
}

/* ── Spend by model ────────────────────────────────────────────────────── */

/**
 * Accent density for the nth of `count` series. The ramp stops at 0.3 rather
 * than running to zero: a slice nobody can see is not in the chart.
 */
function tone(i: number, count: number): number {
  if (count <= 1) return 1
  return 1 - (i / (count - 1)) * 0.7
}

type ModelRow = CostReport['rows'][number] & { share: number; tokens: number }

function Models({ load }: { load: Load<CostReport> }) {
  const [metric, setMetric] = useState<'usd' | 'tokens'>('usd')
  const [hover, setHover] = useState<string | null>(null)
  const report = load.value

  const sub = (
    <>
      Every repository on this machine, from <code>~/.wingman/usage.json</code>.
    </>
  )

  if (load.error)
    return (
      <Card title="Spend by model" sub={sub}>
        <Failed title="Couldn't load spend by model" detail={load.error} action={{ label: 'Try again', onClick: load.reload }} />
      </Card>
    )
  if (!report) return <Card title="Spend by model" sub={sub}><Loading what="spend by model" /></Card>
  if (report.rows.length === 0)
    return (
      <Card title="Spend by model" sub={sub}>
        <Empty title="Nothing spent yet">
          Spend appears here once Wingman answers a message or carries out a run.
        </Empty>
      </Card>
    )

  const tokensOf = (r: CostReport['rows'][number]) => r.input_tokens + r.output_tokens + r.cache_read_tokens + r.cache_write_tokens
  const totalTokens = sum(report.rows.map(tokensOf))
  const rows: ModelRow[] = report.rows.map((r) => ({
    ...r,
    tokens: tokensOf(r),
    share: metric === 'usd' ? (report.total_usd ? r.usd / report.total_usd : 0) : totalTokens ? tokensOf(r) / totalTokens : 0,
  }))
  const ranked = [...rows].sort((a, b) => b.share - a.share)
  const slices = arcs(ranked.map((r) => r.share))
  const focus = ranked.find((r) => r.key === hover)

  const cacheRead = sum(report.rows.map((r) => r.cache_read_tokens))
  const cacheWrite = sum(report.rows.map((r) => r.cache_write_tokens))
  const fresh = sum(report.rows.map((r) => r.input_tokens))
  const input = fresh + cacheRead + cacheWrite

  return (
    <Card
      title="Spend by model"
      sub={sub}
      className="ins-models"
      aside={
        <div className="ins-toggle" role="group" aria-label="Measure">
          <button type="button" aria-pressed={metric === 'usd'} onClick={() => setMetric('usd')}>
            Spend
          </button>
          <button type="button" aria-pressed={metric === 'tokens'} onClick={() => setMetric('tokens')}>
            Tokens
          </button>
        </div>
      }
    >
      <div className="ins-models-body">
        <div className="ins-donut-wrap">
          <svg
            className="ins-donut"
            viewBox="0 0 120 120"
            role="img"
            aria-label={ranked.map((r) => `${r.key}: ${Math.round(r.share * 100)}%`).join('; ')}
          >
            {ranked.map((r, i) =>
              r.share > 0 ? (
                <path
                  key={r.key}
                  d={arcPath(slices[i][0], slices[i][1], 60, 40, 56)}
                  className={`ins-slice${hover && hover !== r.key ? ' is-dim' : ''}`}
                  style={{ '--tone': tone(i, ranked.length) } as React.CSSProperties}
                  onMouseEnter={() => setHover(r.key)}
                  onMouseLeave={() => setHover(null)}
                />
              ) : null,
            )}
          </svg>
          <div className="ins-donut-center" aria-hidden="true">
            <strong>
              {focus
                ? `${Math.round(focus.share * 100)}%`
                : metric === 'usd'
                  ? money(report.total_usd)
                  : compactNum(totalTokens)}
            </strong>
            <span className="truncate">{focus ? focus.key : metric === 'usd' ? 'total spend' : 'total tokens'}</span>
          </div>
        </div>

        <SortTable<ModelRow>
          rows={rows}
          rowKey={(r) => r.key}
          initial={{ key: 'usd', dir: 'desc' }}
          onHover={setHover}
          hovered={hover}
          columns={[
            {
              key: 'key',
              label: 'Model',
              value: (r) => r.key,
              render: (r) => (
                <span className="ins-model">
                  <i
                    className="ins-key"
                    style={{ '--tone': tone(ranked.indexOf(r), ranked.length) } as React.CSSProperties}
                    aria-hidden="true"
                  />
                  {r.key}
                </span>
              ),
            },
            { key: 'usd', label: 'Spend', value: (r) => r.usd, render: (r) => money(r.usd), numeric: true },
            {
              key: 'share',
              label: 'Share',
              value: (r) => r.share,
              render: (r) => `${Math.round(r.share * 100)}%`,
              numeric: true,
            },
            { key: 'input', label: 'Input', value: (r) => r.input_tokens, render: (r) => compactNum(r.input_tokens), numeric: true },
            { key: 'output', label: 'Output', value: (r) => r.output_tokens, render: (r) => compactNum(r.output_tokens), numeric: true },
            {
              key: 'cache',
              label: 'From cache',
              value: (r) => r.cache_read_tokens,
              render: (r) => (r.cache_read_tokens ? compactNum(r.cache_read_tokens) : '—'),
              numeric: true,
            },
          ]}
        />
      </div>

      {input > 0 && (
        <div className="ins-input-mix">
          <div className="ins-mix-bar" role="img" aria-label={`${fresh.toLocaleString()} fresh input tokens, ${cacheRead.toLocaleString()} read from cache, ${cacheWrite.toLocaleString()} written to cache`}>
            <span style={{ flexGrow: fresh }} />
            {cacheRead > 0 && <span className="is-cached" style={{ flexGrow: cacheRead }} />}
            {cacheWrite > 0 && <span className="is-write" style={{ flexGrow: cacheWrite }} />}
          </div>
          <p>
            {cacheRead > 0 ? (
              <>
                <strong>{Math.round((cacheRead / input) * 100)}%</strong> of input came from cache,
                which costs far less than fresh input. That is why this total can come in under your
                provider's list price.
              </>
            ) : (
              <>No input came from cache yet, so every input token was billed at full price.</>
            )}
          </p>
        </div>
      )}
    </Card>
  )
}

/* ── Sortable table ────────────────────────────────────────────────────── */

type Column<T> = {
  key: string
  label: string
  value: (r: T) => string | number
  render?: (r: T) => React.ReactNode
  numeric?: boolean
}

function SortTable<T>({
  rows,
  columns,
  rowKey,
  initial,
  onHover,
  hovered,
}: {
  rows: T[]
  columns: Column<T>[]
  rowKey: (r: T) => string
  initial: { key: string; dir: 'asc' | 'desc' }
  onHover?: (key: string | null) => void
  hovered?: string | null
}) {
  const [sort, setSort] = useState(initial)
  const col = columns.find((c) => c.key === sort.key) ?? columns[0]
  const sorted = [...rows].sort((a, b) => {
    const x = col.value(a)
    const y = col.value(b)
    const cmp = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y))
    return sort.dir === 'asc' ? cmp : -cmp
  })

  return (
    <div className="ins-table-wrap">
      <table className="ins-table">
        <thead>
          <tr>
            {columns.map((c) => {
              const on = c.key === sort.key
              return (
                <th
                  key={c.key}
                  scope="col"
                  className={c.numeric ? 'is-num' : undefined}
                  aria-sort={on ? (sort.dir === 'asc' ? 'ascending' : 'descending') : 'none'}
                >
                  <button
                    type="button"
                    onClick={() =>
                      setSort({
                        key: c.key,
                        dir: on ? (sort.dir === 'asc' ? 'desc' : 'asc') : c.numeric ? 'desc' : 'asc',
                      })
                    }
                  >
                    {c.label}
                    <span className="ins-sort" aria-hidden="true">
                      {on ? (sort.dir === 'asc' ? '↑' : '↓') : '↕'}
                    </span>
                  </button>
                </th>
              )
            })}
          </tr>
        </thead>
        <tbody>
          {sorted.map((r) => (
            <tr
              key={rowKey(r)}
              className={hovered === rowKey(r) ? 'is-hover' : undefined}
              onMouseEnter={() => onHover?.(rowKey(r))}
              onMouseLeave={() => onHover?.(null)}
            >
              {columns.map((c) => (
                <td key={c.key} className={c.numeric ? 'is-num' : undefined}>
                  {c.render ? c.render(r) : c.value(r)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

/* ── Other models ──────────────────────────────────────────────────────── */

/**
 * Every priced model on one log axis, with a line at what was actually paid.
 * Cheaper models stop short of the line, dearer ones cross it. The axis is
 * logarithmic and says so: the cheap end of a 200× spread is otherwise a smear.
 */
function Alternatives({ cost }: { cost: CostReport | null }) {
  const sub = 'The same tokens, priced on every model Wingman knows.'
  if (!cost) return <Card title="Same work, other models" sub={sub} id="alternatives"><Loading what="prices" /></Card>

  // The comparison includes the model already in use, priced at exactly what
  // was paid; charting it beside "you paid" is the same number twice.
  const used = new Set(cost.rows.map((r) => r.key))
  const alternatives = cost.comparison.filter((c) => !used.has(c.model))
  if (alternatives.length === 0 || !(cost.total_usd > 0))
    return (
      <Card title="Same work, other models" sub={sub} id="alternatives">
        <p className="ins-muted">There is nothing to compare yet.</p>
      </Card>
    )

  const bars = [
    { model: cost.rows.length === 1 ? cost.rows[0].key : 'You paid', usd: cost.total_usd, actual: true },
    ...alternatives.map((c) => ({ model: c.model, usd: c.would_cost_usd, actual: false })),
  ].sort((a, b) => a.usd - b.usd)

  const { lo, hi, ticks } = decadeBounds(Math.min(...bars.map((b) => b.usd)), Math.max(...bars.map((b) => b.usd)))
  const datum = logFraction(cost.total_usd, lo, hi)

  return (
    <Card title="Same work, other models" sub={sub} id="alternatives">
      <div className="ladder">
        <span className="ladder-datum" style={{ '--at': datum } as never} aria-hidden="true">
          <span className="ladder-datum-tag figure">{money(cost.total_usd)}</span>
        </span>
        {bars.map((b) => (
          <div key={b.model} className={`ladder-row${b.actual ? ' is-actual' : ''}`}>
            <span className="figure ladder-label truncate" title={b.model}>
              {b.model}
            </span>
            <span className="ladder-track">
              <span className="ladder-fill" style={{ width: `${logFraction(b.usd, lo, hi) * 100}%` }} />
            </span>
            <span className="figure ladder-value">{money(b.usd)}</span>
          </div>
        ))}
        <div className="ladder-axis" aria-hidden="true">
          {ticks.map((t) => (
            <span key={t} className="ladder-tick figure" style={{ '--at': logFraction(t, lo, hi) } as never}>
              {t < 1 ? `$${t.toFixed(2)}` : `$${t.toLocaleString()}`}
            </span>
          ))}
        </div>
      </div>
      <p className="ins-foot">
        The line is what you paid. Each tick is ten times the last. A cheaper model that needs more
        attempts is not really cheaper — check its pass rate below before switching.
      </p>
    </Card>
  )
}

/* ── Per-message overhead ──────────────────────────────────────────────── */

/** The share of the overhead the Pareto's dashed line marks. */
const PARETO = 0.8

function Overhead({ load }: { load: Load<ContextReport> }) {
  const [all, setAll] = useState(false)
  const sub = 'What every message costs before you type anything.'
  const report = load.value
  if (load.error)
    return (
      <Card title="Fixed cost per message" sub={sub} id="overhead">
        <Failed title="Couldn't load the overhead" detail={load.error} action={{ label: 'Try again', onClick: load.reload }} />
      </Card>
    )
  if (!report) return <Card title="Fixed cost per message" sub={sub} id="overhead"><Loading what="the overhead" /></Card>

  // Largest first: the Pareto and the "top N" both depend on it, so it is not
  // left to whatever order the server happened to send.
  const tools = [...report.tools].sort((a, b) => b.tokens - a.tokens)
  const tokens = tools.map((t) => t.tokens)
  const needed = coverage(tokens, PARETO)
  const shown = all ? tools : tools.slice(0, 5)
  const widest = tools[0]?.tokens ?? 1
  const promptShare = report.first_turn_tokens ? report.system_prompt_tokens / report.first_turn_tokens : 0

  return (
    <Card title="Fixed cost per message" sub={sub} id="overhead">
      <p className="ins-big">
        {report.first_turn_tokens.toLocaleString()} <span>tokens</span>
      </p>
      <div className="ins-mix-bar" role="img" aria-label={`${report.system_prompt_tokens} tokens of instructions, ${report.tool_schema_tokens} of tool descriptions`}>
        <span style={{ flexGrow: report.system_prompt_tokens }} />
        <span className="is-write" style={{ flexGrow: report.tool_schema_tokens }} />
      </div>
      <div className="ins-split">
        <span>
          <i className="ins-key" style={{ '--tone': 1 } as React.CSSProperties} aria-hidden="true" />
          Instructions <strong>{Math.round(promptShare * 100)}%</strong>
        </span>
        <span>
          <i className="ins-key" style={{ '--tone': 0.45 } as React.CSSProperties} aria-hidden="true" />
          {report.tool_count} tool descriptions <strong>{Math.round((1 - promptShare) * 100)}%</strong>
        </span>
      </div>

      <h3 className="ins-sub-head">Which tools cost the most</h3>
      <Pareto values={tokens} names={tools.map((t) => t.name)} />
      <p className="ins-foot">
        <strong>
          {needed} of {report.tools.length} tools
        </strong>{' '}
        make up {Math.round(PARETO * 100)}% of the tool cost (the dashed line). Turning off tools
        outside that group barely changes it.
      </p>
      <div className="bars">
        {shown.map((t) => (
          <div key={t.name} className="bar-row">
            <span className="figure bar-label truncate">{t.name}</span>
            <span className="bar-track">
              <span className="bar-fill" style={{ width: `${widest > 0 ? (t.tokens / widest) * 100 : 0}%` }} />
            </span>
            <span className="figure bar-value">{Math.round(t.tokens).toLocaleString()}</span>
          </div>
        ))}
      </div>
      {report.tools.length > 5 && (
        <button type="button" className="button button-quiet button-sm" onClick={() => setAll((a) => !a)}>
          {all ? 'Show fewer' : `Show all ${report.tools.length} tools`}
        </button>
      )}
    </Card>
  )
}

/**
 * Ranked columns with the running share drawn over them. The columns answer
 * "which", the curve "how many before it stops mattering".
 */
function Pareto({ values, names }: { values: number[]; names: string[] }) {
  const W = 720
  const H = 120
  const peak = Math.max(...values, 1)
  const running = cumulative(values)
  const slot = W / Math.max(values.length, 1)
  const barW = Math.max(slot - 4, 1)

  return (
    <svg className="pareto" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={`${values.length} tools by token cost, largest first, with their running share`}>
      <line className="pareto-marker" x1="0" y1={H - PARETO * H} x2={W} y2={H - PARETO * H} vectorEffect="non-scaling-stroke" />
      {values.map((v, i) => (
        <rect key={names[i]} className="pareto-bar" x={i * slot} y={H - (v / peak) * H} width={barW} height={(v / peak) * H} rx="2">
          <title>{`${names[i]} · ${Math.round(v)} tokens · ${Math.round(running[i] * 100)}% so far`}</title>
        </rect>
      ))}
      <path className="pareto-curve" d={linePath(running, W, H)} vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

/* ── Quality ───────────────────────────────────────────────────────────── */

/** A nullable rate as a whole percentage; an absent rate is a dash, never 0%. */
export function rate(x: number | null): string {
  return x == null ? '—' : `${Math.round(x * 100)}%`
}

type RouteRow = Metrics['routing'][number]

function Quality({ load }: { load: Load<Metrics> }) {
  const sub = "From this repository's chats, runs and terminal sessions."
  const m = load.value
  if (load.error)
    return (
      <Card title="How well it works" sub={sub}>
        <Failed title="Couldn't load quality figures" detail={load.error} action={{ label: 'Try again', onClick: load.reload }} />
      </Card>
    )
  if (!m) return <Card title="How well it works" sub={sub}><Loading what="quality figures" /></Card>
  if (m.turns === 0)
    return (
      <Card title="How well it works" sub={sub}>
        <Empty title="No messages yet">
          These fill in as Wingman answers messages in this repository. Nothing is estimated.
        </Empty>
      </Card>
    )

  const ttft = m.time_to_first_token_ms
  const red = m.gated_turns - m.verified_turns
  const secs = (ms: number | null) => (ms == null ? '—' : ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`)

  return (
    <Card title="How well it works" sub={sub} className="ins-quality">
      <dl className="ins-stats">
        <div>
          <dt>First reply in</dt>
          <dd>{secs(ttft.median)}</dd>
          <span>Typical · slowest 10% {secs(ttft.p90)}</span>
        </div>
        <div>
          <dt>Tokens per finished task</dt>
          <dd>{m.tokens_per_completed_task == null ? '—' : compactNum(m.tokens_per_completed_task)}</dd>
          <span>
            {m.completed_turns.toLocaleString()} of {m.turns.toLocaleString()} messages finished
          </span>
        </div>
      </dl>

      {m.gated_turns > 0 && (
        <>
          <h3 className="ins-sub-head">Changes that were checked</h3>
          <div className="ins-verdict" role="img" aria-label={`${m.verified_turns} passed, ${red} failed`}>
            {m.verified_turns > 0 && <span className="is-proven" style={{ flexGrow: m.verified_turns }} />}
            {red > 0 && <span className="is-failed" style={{ flexGrow: red }} />}
          </div>
          <div className="ins-split">
            <span className="is-proven dot">{m.verified_turns} passed</span>
            <span className="is-failed dot">{red} failed</span>
            <span className="ins-muted">Messages that changed nothing are not checked.</span>
          </div>
        </>
      )}

      <h3 className="ins-sub-head" id="routing">
        Pass rate by model and task type
      </h3>
      {m.routing.length === 0 ? (
        <p className="ins-muted">Nothing recorded yet. A row appears once a checked change finishes.</p>
      ) : (
        <SortTable<RouteRow>
          rows={m.routing}
          rowKey={(r) => `${r.task_class}/${r.model}`}
          initial={{ key: 'rate', dir: 'desc' }}
          columns={[
            { key: 'task', label: 'Task type', value: (r) => r.task_class, render: (r) => humanTask(r.task_class) },
            { key: 'model', label: 'Model', value: (r) => r.model },
            { key: 'total', label: 'Tries', value: (r) => r.total, numeric: true },
            {
              key: 'rate',
              label: 'Passed',
              value: (r) => r.pass_rate,
              numeric: true,
              render: (r) => (
                <span className="ins-rate">
                  <span className="ins-rate-track" aria-hidden="true">
                    <span style={{ width: `${r.pass_rate * 100}%` }} />
                  </span>
                  {rate(r.pass_rate)}
                  <span className="ins-muted">
                    {' '}
                    {r.passed}/{r.total}
                  </span>
                </span>
              ),
            },
          ]}
        />
      )}
    </Card>
  )
}

/** `test-fix` → "Test fix". */
function humanTask(t: string): string {
  const w = t.replace(/[-_]+/g, ' ')
  return w ? w[0].toUpperCase() + w.slice(1) : t
}

/* ── Runs ──────────────────────────────────────────────────────────────── */

function Outcomes({ runs }: { runs: PricedRuns | null }) {
  const sub = 'How runs in this repository ended, and what the newest ones cost.'
  if (!runs) return <Card title="Runs" sub={sub}><Loading what="runs" /></Card>
  const { all, priced } = runs
  if (all.length === 0)
    return (
      <Card title="Runs" sub={sub}>
        <Empty title="No runs yet" action={{ label: 'Go to the board', onClick: () => navigate('/board') }}>
          Press Start on a board card and Wingman plans and carries out the work as a run.
        </Empty>
      </Card>
    )

  const done = all.filter((r) => r.status === 'done').length
  const failed = all.filter((r) => r.status === 'failed' || r.status === 'aborted').length
  const open = all.length - done - failed
  const ceiling = Math.max(...priced.map((r) => r.usd), 0.0001)

  return (
    <Card title="Runs" sub={sub} className="ins-runs">
      <div className="ins-verdict" role="img" aria-label={`${done} done, ${open} in progress, ${failed} failed or stopped`}>
        {done > 0 && <span className="is-proven" style={{ flexGrow: done }} />}
        {open > 0 && <span className="is-asserted" style={{ flexGrow: open }} />}
        {failed > 0 && <span className="is-failed" style={{ flexGrow: failed }} />}
      </div>
      <div className="ins-split">
        <span className="is-proven dot">{done} done</span>
        <span className="is-asserted dot">{open} in progress</span>
        <span className="is-failed dot">{failed} failed or stopped</span>
      </div>

      <ul className="ins-run-list">
        {priced.map(({ run, usd }) => (
          <li key={run.run_id}>
            <button type="button" onClick={() => navigate(`/runs/${run.run_id}`)} title={run.goal}>
              <span className="ins-run-goal truncate">{run.goal}</span>
              <Pill status={runClass(run.status)} glyph={runGlyph(run.status)}>
                {runLabel(run.status)}
              </Pill>
              <span className="ins-run-bar" aria-hidden="true">
                <span style={{ width: `${(usd / ceiling) * 100}%` }} />
              </span>
              <span className="ins-run-usd">{usd > 0 ? money(usd) : '—'}</span>
            </button>
          </li>
        ))}
      </ul>
      {all.length > RECENT && (
        <p className="ins-foot">The newest {RECENT} of {all.length} runs are priced here.</p>
      )}
    </Card>
  )
}

/* ── More reports ──────────────────────────────────────────────────────── */

/**
 * Every other read route, listed from `GET /v1/schema` — generated from the
 * server's own route table, so a report added to the CLI appears here without
 * this page changing. Folded away: it is a toolbox, not a dashboard.
 */
function Reports({ project }: { project: string }) {
  const [schema, setSchema] = useState<ApiSchema | null>(null)
  const [open, setOpen] = useState<string | null>(null)
  const [result, setResult] = useState<unknown>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let live = true
    api
      .apiSchema()
      .then((s) => live && setSchema(s))
      .catch(() => {
        /* Optional furniture: the dashboard above is the report. */
      })
    return () => {
      live = false
    }
  }, [])

  if (!schema) return null

  // Read-only, project-scoped, nothing left to fill in, and without a screen
  // of its own elsewhere in the panel.
  const own = ['/cost', '/context', '/metrics', '/pilot', '/sessions', '/diff', '/explain', '/review', '/attest']
  const routes = schema.routes.filter(
    (r) =>
      r.method === 'GET' &&
      r.path.includes('{project}') &&
      !r.path.includes('{run}') &&
      !r.path.includes('{id}') &&
      !own.some((o) => r.path.includes(o)),
  )
  if (routes.length === 0) return null

  async function run(r: RouteInfo) {
    const tail = r.path.split('{project}/')[1]
    setOpen(tail)
    setResult(null)
    setError(null)
    setBusy(true)
    try {
      setResult(await api.report(project, tail))
    } catch (e) {
      setError(message(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <details className="ins-card ins-reports">
      <summary>
        <span>
          <h2>More reports</h2>
          <span className="ins-card-sub">
            {routes.length} more reports Wingman can run for this repository
          </span>
        </span>
        <Icon name="expand" size={14} />
      </summary>
      <div className="report-grid">
        {routes.map((r) => {
          const tail = r.path.split('{project}/')[1]
          const running = open === tail && busy
          return (
            <button
              key={r.path}
              type="button"
              className={`report-card${open === tail ? ' is-open' : ''}`}
              aria-pressed={open === tail}
              onClick={() => void run(r)}
            >
              <span className="report-name">{humanTask(tail.replace('/', ' '))}</span>
              <span className="muted report-about">{r.about ?? r.returns ?? ''}</span>
              <span className="report-action">{running ? 'Running…' : open === tail ? 'Shown below' : 'Run'}</span>
            </button>
          )
        })}
      </div>
      {error && (
        <p className="is-failed dot figure" role="alert">
          {error}
        </p>
      )}
      {open && result != null && (
        <div className="ins-report-out">
          <h3 className="ins-sub-head">{humanTask(open.replace('/', ' '))}</h3>
          <Output value={result} />
        </div>
      )}
    </details>
  )
}
