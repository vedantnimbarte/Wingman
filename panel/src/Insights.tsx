import { useCallback, useEffect, useState } from 'react'
import {
  api,
  isTextOutput,
  type ApiSchema,
  type ContextReport,
  type CostReport,
  type CostTimeline,
  type RouteInfo,
  type RunSummary,
} from './api'
import { money } from './Board'
import { areaPath, coverage, cumulative, decadeBounds, linePath, logFraction } from './charts'
import { navigate } from './router'
import { runClass, runGlyph } from './Runs'
import { message } from './state'
import { Empty, Failed, Loading, PageHead } from './ui'

/**
 * Insights — what this repo costs, what it would have cost elsewhere, and
 * what every turn pays before you type.
 *
 * Four charts, each answering a question the figures alone do not:
 *
 * 1. **Composition** — of the total, how much is which model, and how much of
 *    the input was cache rather than fresh tokens.
 * 2. **The repricing ladder** — the page's signature. Every alternative model
 *    on one log axis with a *datum line* at what you actually paid. Cheaper
 *    models stop short of the line; dearer ones cross it. The crossing is the
 *    whole argument, and it is made without a single hue.
 * 3. **Spend over time** — from `GET /cost/timeline`, which prices this
 *    repo's session transcripts by day.
 * 4. **The per-turn tax** — 28 tool schemas as a Pareto, so "how many tools
 *    are most of the tax" is answerable by looking.
 *
 * The palette rule holds throughout: colour encodes epistemic status and
 * nothing else. Every chart here is drawn in ink and the neutrals, and the
 * three status hues appear in exactly one place — run outcomes, where a
 * verdict is what is being reported. Nothing on this page is coloured because
 * it is a chart.
 */
export function Insights({ project }: { project: string | null }) {
  if (!project) {
    return (
      <div className="view">
        <Failed
          title="No project selected"
          detail="These reports are per-repo. Pick one in the header."
          action={{ label: 'Go to Overview', onClick: () => navigate('/') }}
        />
      </div>
    )
  }
  return (
    <div className="view">
      <PageHead
        eyebrow="Insights"
        title="What this repo has cost"
        intro="Real spend, the same token volume repriced against ten other models, and the tax every turn pays before you have typed anything."
      />
      <Spend project={project} />
      <Timeline project={project} />
      <Outcomes project={project} />
      <Context project={project} />
      <Reports project={project} />
    </div>
  )
}

/* ── Spend ─────────────────────────────────────────────────────────────── */

/**
 * The headline, what it is made of, and what it would have cost elsewhere.
 *
 * One caveat is stated on screen rather than buried here: `wingman cost`
 * reads `~/.wingman/usage.json`, which is machine-wide and has no project in
 * it. Labelling this figure with the repo's name would be wrong, so it says
 * what it actually measures.
 */
function Spend({ project }: { project: string }) {
  const [report, setReport] = useState<CostReport | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      // Always with `compare`: repricing the same token volume against other
      // models is the point of the report, not an extra.
      setReport(await api.cost(project, true))
      setError(null)
    } catch (e) {
      setError(message(e))
    }
  }, [project])

  useEffect(() => {
    void load()
  }, [load])

  if (error)
    return (
      <Failed
        title="Could not read cost"
        detail={error}
        action={{ label: 'Try again', onClick: () => void load() }}
      />
    )
  if (!report) return <Loading what="cost" />

  if (report.rows.length === 0) {
    return (
      <Empty title="Nothing spent yet">
        Cost accrues as you run turns and pilot runs. Start a conversation or dispatch a card and
        this page fills in.
      </Empty>
    )
  }

  const cacheRead = report.rows.reduce((n, r) => n + r.cache_read_tokens, 0)
  const cacheWrite = report.rows.reduce((n, r) => n + r.cache_write_tokens, 0)
  const freshInput = report.rows.reduce((n, r) => n + r.input_tokens, 0)

  return (
    <section className="panel">
      <span className="eyebrow">Total spend · every project on this machine</span>
      <p className="headline">{money(report.total_usd)}</p>
      <p className="section-intro">
        {report.total_input_tokens.toLocaleString()} tokens in ·{' '}
        {report.total_output_tokens.toLocaleString()} out, across{' '}
        {report.rows.length === 1 ? 'one model' : `${report.rows.length} models`}. This is{' '}
        <code>~/.wingman/usage.json</code>, which counts every repo — the chart below is this one.
      </p>

      <Composition report={report} />

      <h3 className="chart-head">What the input was made of</h3>
      <Tokens fresh={freshInput} cacheRead={cacheRead} cacheWrite={cacheWrite} />

      <Ladder report={report} />
    </section>
  )
}

/**
 * The total as one bar, split by model.
 *
 * Segments are separated by ink density and a hairline, never a hue. Two
 * models is a bar that could have been a sentence; five is a bar that could
 * not, and the same drawing has to work for both.
 */
function Composition({ report }: { report: CostReport }) {
  const rows = [...report.rows].sort((a, b) => b.usd - a.usd)
  const total = report.total_usd
  if (!(total > 0)) return null

  return (
    <>
      <h3 className="chart-head">Where it went</h3>
      <div
        className="stack"
        role="img"
        aria-label={rows
          .map((r) => `${r.key}: ${money(r.usd)}, ${Math.round((r.usd / total) * 100)}%`)
          .join('; ')}
      >
        {rows.map((r, i) => (
          <span
            key={r.key}
            className="stack-seg"
            // Rank, as ink density. The dearest model is solid; each one
            // after it steps back, bottoming out well above the track so the
            // last segment is still a segment and not the empty bar.
            style={{ width: `${(r.usd / total) * 100}%`, '--tone': tone(i, rows.length) } as never}
            title={`${r.key} · ${money(r.usd)}`}
          />
        ))}
      </div>
      {/* The legend and the per-model ledger are the same rows: a swatch ties
          each line to its segment, and the exact figures stay on the rail
          rather than hiding in a tooltip. A chart that replaces the numbers
          with a shape has taken something away. */}
      <div className="rows">
        {rows.map((r, i) => (
          <div key={r.key} className="row">
            <span className="legend-item">
              <span className="legend-key" style={{ '--tone': tone(i, rows.length) } as never} />
              <span className="figure truncate">{r.key}</span>
            </span>
            <span className="figure">
              {money(r.usd)} <span className="muted">{Math.round((r.usd / total) * 100)}%</span>
              <br />
              <span className="muted">
                {r.input_tokens.toLocaleString()} in · {r.output_tokens.toLocaleString()} out
                {r.cache_read_tokens > 0 && ` · ${r.cache_read_tokens.toLocaleString()} cache read`}
                {r.cache_write_tokens > 0 &&
                  ` · ${r.cache_write_tokens.toLocaleString()} cache write`}
              </span>
            </span>
          </div>
        ))}
      </div>
    </>
  )
}

/**
 * Ink density for the nth of `count` series, as an opacity.
 *
 * The ramp stops at 0.35 rather than running to zero: a segment nobody can
 * see is a segment that is not in the chart.
 */
function tone(i: number, count: number): number {
  if (count <= 1) return 1
  return 1 - (i / (count - 1)) * 0.65
}

/**
 * Input, split into what was billed as fresh and what came from cache.
 *
 * Cache traffic was on the wire from the first release and rendered nowhere.
 * It is the number that reconciles this page with the invoice: a repo that is
 * 80% cache reads pays a fraction of what its input count implies. Cached
 * tokens are hatched rather than tinted — the one texture on the page, and it
 * means "not fresh" rather than decorating a series.
 */
function Tokens({
  fresh,
  cacheRead,
  cacheWrite,
}: {
  fresh: number
  cacheRead: number
  cacheWrite: number
}) {
  const total = fresh + cacheRead + cacheWrite
  if (total === 0) return null
  const pct = (n: number) => `${(n / total) * 100}%`

  return (
    <>
      <div className="stack" role="img" aria-label={`${fresh.toLocaleString()} fresh input tokens, ${cacheRead.toLocaleString()} read from cache, ${cacheWrite.toLocaleString()} written to cache`}>
        <span className="stack-seg" style={{ width: pct(fresh), '--tone': 1 } as never} title={`fresh input · ${fresh.toLocaleString()}`} />
        {cacheRead > 0 && (
          <span className="stack-seg is-cached" style={{ width: pct(cacheRead) } as never} title={`cache read · ${cacheRead.toLocaleString()}`} />
        )}
        {cacheWrite > 0 && (
          <span className="stack-seg" style={{ width: pct(cacheWrite), '--tone': 0.4 } as never} title={`cache write · ${cacheWrite.toLocaleString()}`} />
        )}
      </div>
      <p className="section-intro">
        {cacheRead > 0 ? (
          <>
            {cacheRead.toLocaleString()} of {total.toLocaleString()} input tokens came from cache
            {cacheWrite > 0 && `, and ${cacheWrite.toLocaleString()} were written to it`}. Cache
            reads bill well below fresh input on every provider that offers them, which is why the
            total above and the invoice can disagree in your favour.
          </>
        ) : (
          <>
            No cache traffic yet — every one of these {total.toLocaleString()} input tokens was
            billed fresh. Prompt caching is where a repeated system prompt stops being charged like
            a new one.
          </>
        )}
      </p>
    </>
  )
}

/**
 * The signature: every priced model on one log axis, with a datum line at
 * what this machine actually paid.
 *
 * A linear axis cannot carry this data — the live spread runs $0.21 to $42.06,
 * so the eight cheapest models collapse into a smear against the dearest. The
 * axis is logarithmic and *says so*, with a tick per decade, because a log
 * axis that does not announce itself is a chart that overstates the cheap end.
 *
 * The datum is the point. Cheaper models stop short of the line, dearer ones
 * cross it, and which side a model falls on is the only thing anyone reads
 * this chart to learn.
 */
function Ladder({ report }: { report: CostReport }) {
  // The comparison list includes the model you are already on, and its figure
  // is your actual spend to the last decimal — the server derives both from
  // the same token volume. Charting it beside the "you" bar is the same number
  // twice, so it is dropped rather than labelled.
  const used = new Set(report.rows.map((r) => r.key))
  const alternatives = report.comparison.filter((c) => !used.has(c.model))
  if (alternatives.length === 0) return null

  const bars = [
    // Named after the model only when there was one. With two in the rows
    // above, labelling the total with the first one reads as that model's
    // spend and is off by the other model's.
    {
      model: report.rows.length === 1 ? report.rows[0].key : 'this machine',
      usd: report.total_usd,
      actual: true,
    },
    ...alternatives.map((c) => ({ model: c.model, usd: c.would_cost_usd, actual: false })),
  ].sort((a, b) => a.usd - b.usd)

  const { lo, hi, ticks } = decadeBounds(
    Math.min(...bars.map((b) => b.usd)),
    Math.max(...bars.map((b) => b.usd)),
  )
  const datum = logFraction(report.total_usd, lo, hi)
  const cheapest = Math.min(...alternatives.map((c) => c.would_cost_usd))
  const saving = report.total_usd - cheapest

  return (
    <>
      <h2 className="section-head">What the same work would have cost</h2>
      <p className="section-intro">
        Your real token volume, repriced against every model Wingman knows. Only a
        provider-agnostic agent can show you this — and it is a price comparison, not a
        recommendation: a cheaper model that needs three attempts is not cheaper.
      </p>

      <div className="ladder">
        {/* One hairline through every row, at what was actually paid. */}
        <span className="ladder-datum" style={{ '--at': datum } as never} aria-hidden="true">
          <span className="ladder-datum-tag figure">{money(report.total_usd)}</span>
        </span>

        {bars.map((b) => {
          const width = logFraction(b.usd, lo, hi)
          return (
            <div key={b.model} className={`ladder-row${b.actual ? ' is-actual' : ''}`}>
              <span className="figure ladder-label truncate" title={b.model}>
                {b.model}
              </span>
              <span className="ladder-track">
                <span className="ladder-fill" style={{ width: `${width * 100}%` }} />
              </span>
              <span className="figure ladder-value">{money(b.usd)}</span>
            </div>
          )
        })}

        <div className="ladder-axis" aria-hidden="true">
          {ticks.map((t) => (
            <span key={t} className="ladder-tick figure" style={{ '--at': logFraction(t, lo, hi) } as never}>
              {/* Two decimals below a dollar, none above: the axis follows
                  `money()`'s convention at the end where cents are the unit,
                  and "$0.1" on a ledger is a typo nobody reads as ten cents. */}
              {t < 1 ? `$${t.toFixed(2)}` : `$${t.toLocaleString()}`}
            </span>
          ))}
        </div>
      </div>

      <p className="section-intro">
        Log scale — each tick is ten times the last, or the cheap end of a 200× spread would be
        invisible. The line is what you paid.
        {saving > 0 && (
          <>
            {' '}
            The cheapest model priced here would have been{' '}
            <strong className="figure">{money(saving)}</strong> less for the identical token volume.
          </>
        )}
      </p>
    </>
  )
}

/* ── Over time ─────────────────────────────────────────────────────────── */

/** Windows offered. Thirty is the default because a month is how spend is asked about. */
const WINDOWS = [7, 30, 90] as const

/**
 * Spend per day, from this repo's transcripts.
 *
 * This is the one number on the page that is actually scoped to the project,
 * and it will not match the total above — that one is machine-wide. Both say
 * which they are, every time they appear, rather than leaving the reader to
 * discover the discrepancy by arithmetic.
 */
function Timeline({ project }: { project: string }) {
  const [data, setData] = useState<CostTimeline | null>(null)
  const [days, setDays] = useState<number>(30)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let live = true
    api
      .costTimeline(project, days)
      .then((d) => live && setData(d))
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [project, days])

  if (error) return <p className="is-failed dot figure">{error}</p>
  if (!data) return null

  const values = data.days.map((d) => d.usd)
  const peak = Math.max(...values, 0)
  const busiest = data.days[values.indexOf(peak)]

  return (
    <section className="panel">
      <div className="chart-bar">
        <div>
          <h2 className="section-head">Spend over time</h2>
          <p className="section-intro">
            This repo's sessions, priced by day — not the machine-wide figure above.
          </p>
        </div>
        <div className="segmented" role="group" aria-label="Window">
          {WINDOWS.map((w) => (
            <button
              key={w}
              type="button"
              className={`segmented-option${days === w ? ' is-on' : ''}`}
              aria-pressed={days === w}
              onClick={() => setDays(w)}
            >
              {w}d
            </button>
          ))}
        </div>
      </div>

      {data.window_usd > 0 ? (
        <>
          <p className="figure chart-figure">
            {money(data.window_usd)}
            <span className="muted">
              {' '}
              over {days} days · {data.total_turns} turns in {data.sessions}{' '}
              {data.sessions === 1 ? 'session' : 'sessions'}
            </span>
          </p>
          <Area values={values} label={`Daily spend over the last ${days} days`} />
          <div className="axis-dates figure muted">
            <span>{data.days[0]?.date}</span>
            {peak > 0 && busiest && (
              <span className="axis-peak">
                peak {money(peak)} on {busiest.date}
              </span>
            )}
            <span>{data.days[data.days.length - 1]?.date}</span>
          </div>
        </>
      ) : (
        <Empty title={data.last_day ? 'Nothing spent in this window' : 'No sessions in this repo yet'}>
          {data.last_day ? (
            <>
              The last session that cost anything here was on{' '}
              <span className="figure">{data.last_day}</span>. Widen the window to see it.
            </>
          ) : (
            <>
              This chart reads <code>.wingman/sessions</code>. Start a conversation in this repo and
              each turn lands here the moment it is billed.
            </>
          )}
        </Empty>
      )}

      {data.unpriced_turns > 0 && (
        <p className="section-intro">
          {data.unpriced_turns} {data.unpriced_turns === 1 ? 'turn is' : 'turns are'} missing from
          this total: their model has no entry in the pricing table, and guessing a price would be
          worse than saying so.
        </p>
      )}
    </section>
  )
}

/**
 * The daily series as a filled area.
 *
 * Geometry only — every label lives in HTML beside it. Text inside a `viewBox`
 * scales with the box, so a chart that is legible on a laptop sets 6px type on
 * a phone. Keeping the SVG to paths means it can stretch to any width.
 */
function Area({ values, label }: { values: number[]; label: string }) {
  const W = 720
  const H = 120
  // The fill and the line over it are the same series: one closed to the
  // baseline, one left open. `linePath` takes 0–1, so the values are scaled
  // against the same peak the area uses.
  const peak = Math.max(...values, 0)
  const scaled = values.map((v) => (peak > 0 ? v / peak : 0))
  return (
    <svg className="area" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={label}>
      <path className="area-fill" d={areaPath(values, W, H)} />
      <path className="area-line" d={linePath(scaled, W, H)} vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

/* ── Outcomes ──────────────────────────────────────────────────────────── */

/** How many runs are priced individually. Named because the cap is stated. */
const RECENT = 10

/**
 * Pilot runs: how they ended, and what each one cost.
 *
 * The one section on this page where the status hues are earned — a run that
 * failed is a verdict, not a series. Spend still gets typography.
 *
 * **The cap is deliberate and stated on screen.** One request per run means
 * pricing a repo with two hundred runs would be two hundred requests to draw a
 * bar chart. Ten is enough to see a trend; silently truncating to ten and
 * calling it "spend by run" would be the dishonest version.
 */
function Outcomes({ project }: { project: string }) {
  const [rows, setRows] = useState<{ run: RunSummary; usd: number }[] | null>(null)
  const [all, setAll] = useState<RunSummary[]>([])

  useEffect(() => {
    let live = true
    void (async () => {
      try {
        const runs = await api.runs(project)
        if (!live) return
        setAll(runs)
        const priced = await Promise.all(
          runs.slice(0, RECENT).map(async (run) => {
            try {
              const state = await api.run(project, run.run_id)
              return { run, usd: state.totals.usd }
            } catch {
              return { run, usd: 0 }
            }
          }),
        )
        if (live) setRows(priced)
      } catch {
        // The section is an extra. Spend above is the report.
        if (live) setRows([])
      }
    })()
    return () => {
      live = false
    }
  }, [project])

  if (!rows || rows.length === 0) return null

  const ceiling = Math.max(...rows.map((r) => r.usd), 0.0001)
  const done = all.filter((r) => r.status === 'done').length
  const failed = all.filter((r) => r.status === 'failed' || r.status === 'aborted').length
  const open = all.length - done - failed

  return (
    <section className="panel">
      <h2 className="section-head">Runs, by outcome and spend</h2>

      <div className="stack" role="img" aria-label={`${done} done, ${failed} failed, ${open} still running`}>
        {done > 0 && <span className="stack-seg is-proven" style={{ width: `${(done / all.length) * 100}%` }} title={`${done} done`} />}
        {open > 0 && <span className="stack-seg is-asserted" style={{ width: `${(open / all.length) * 100}%` }} title={`${open} running`} />}
        {failed > 0 && <span className="stack-seg is-failed" style={{ width: `${(failed / all.length) * 100}%` }} title={`${failed} failed`} />}
      </div>
      <p className="section-intro">
        {all.length} {all.length === 1 ? 'run' : 'runs'} — <span className="is-proven dot">{done} done</span>,{' '}
        <span className="is-asserted dot">{open} still going</span>,{' '}
        <span className="is-failed dot">{failed} failed or aborted</span>.
        {all.length > RECENT && ` The newest ${RECENT} are priced below; each one is a separate read.`}
      </p>

      <div className="bars">
        {rows.map(({ run, usd }) => (
          <div key={run.run_id} className="bar-row">
            <button
              type="button"
              className="figure bar-label bar-link truncate"
              onClick={() => navigate(`/runs/${run.run_id}`)}
              title={run.goal}
            >
              <span className={`glyph ${runClass(run.status)}`} aria-hidden="true">
                {runGlyph(run.status)}
              </span>
              {run.goal}
            </button>
            <span className="bar-track">
              <span className="bar-fill" style={{ width: `${(usd / ceiling) * 100}%` }} />
            </span>
            <span className="figure bar-value">{usd > 0 ? money(usd) : '—'}</span>
          </div>
        ))}
      </div>
    </section>
  )
}

/* ── Context ───────────────────────────────────────────────────────────── */

/** How much of the tax the dashed marker calls out. */
const PARETO = 0.8

function Context({ project }: { project: string }) {
  const [report, setReport] = useState<ContextReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [all, setAll] = useState(false)

  useEffect(() => {
    let live = true
    api
      .context(project)
      .then((r) => live && setReport(r))
      .catch((e: unknown) => live && setError(message(e)))
    return () => {
      live = false
    }
  }, [project])

  if (error) return <p className="is-failed dot figure">{error}</p>
  if (!report) return null

  const shown = all ? report.tools : report.tools.slice(0, 8)
  const widest = report.tools[0]?.tokens ?? 1
  const tokens = report.tools.map((t) => t.tokens)
  const needed = coverage(tokens, PARETO)

  return (
    <section className="panel">
      <h2 className="section-head">The per-turn tax</h2>
      <p className="section-intro">
        What every turn pays before you type anything. Most agents never tell you this number.
      </p>

      <p className="figure chart-figure">
        {report.first_turn_tokens.toLocaleString()}
        <span className="muted"> tokens on the first turn</span>
      </p>
      <div className="stack" role="img" aria-label={`${report.system_prompt_tokens} tokens of system prompt, ${report.tool_schema_tokens} of tool schemas`}>
        <span
          className="stack-seg"
          style={{ width: `${(report.system_prompt_tokens / report.first_turn_tokens) * 100}%`, '--tone': 1 } as never}
          title={`system prompt · ${report.system_prompt_tokens.toLocaleString()}`}
        />
        <span
          className="stack-seg"
          style={{ width: `${(report.tool_schema_tokens / report.first_turn_tokens) * 100}%`, '--tone': 0.55 } as never}
          title={`tool schemas · ${report.tool_schema_tokens.toLocaleString()}`}
        />
      </div>
      <div className="legend">
        <span className="legend-item">
          <span className="legend-key" style={{ '--tone': 1 } as never} />
          <span className="figure">system prompt</span>
          <span className="figure legend-value">{report.system_prompt_tokens.toLocaleString()}</span>
        </span>
        <span className="legend-item">
          <span className="legend-key" style={{ '--tone': 0.55 } as never} />
          <span className="figure">
            tool schemas <span className="muted">· {report.tool_count} tools</span>
          </span>
          <span className="figure legend-value">{report.tool_schema_tokens.toLocaleString()}</span>
        </span>
      </div>

      <h3 className="chart-head">Which tools are the tax</h3>
      <Pareto values={tokens} names={report.tools.map((t) => t.name)} />
      <p className="section-intro">
        Columns are tools, tallest first; the curve is the running share of the schema tax and the
        dashed line is {Math.round(PARETO * 100)}% of it.{' '}
        <strong className="figure">
          {needed} of {report.tools.length} tools
        </strong>{' '}
        account for that much — cutting anything below the crossing barely moves the number.
      </p>

      <div className="bars">
        {shown.map((t) => (
          <div key={t.name} className="bar-row">
            <span className="figure bar-label truncate">{t.name}</span>
            <span className="bar-track">
              <span
                className="bar-fill"
                style={{ width: `${widest > 0 ? (t.tokens / widest) * 100 : 0}%` }}
              />
            </span>
            <span className="figure bar-value">{t.tokens}</span>
          </div>
        ))}
      </div>
      {report.tools.length > shown.length && (
        <button type="button" className="button button-quiet" onClick={() => setAll(true)}>
          Show all {report.tools.length}
        </button>
      )}
    </section>
  )
}

/**
 * Ranked columns with the cumulative share drawn over them.
 *
 * The columns answer "which", the curve answers "how many before it stops
 * mattering" — the second question is why this is a Pareto and not another
 * ranked bar list. Both share one x-scale, so the crossing sits over the
 * column that causes it.
 */
function Pareto({ values, names }: { values: number[]; names: string[] }) {
  const W = 720
  const H = 150
  const peak = Math.max(...values, 1)
  const running = cumulative(values)
  const slot = W / Math.max(values.length, 1)
  const barW = Math.max(slot - 3, 1)

  return (
    <svg className="pareto" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={`${values.length} tool schemas by token cost, largest first, with their cumulative share`}>
      <line className="pareto-marker" x1="0" y1={H - PARETO * H} x2={W} y2={H - PARETO * H} vectorEffect="non-scaling-stroke" />
      {values.map((v, i) => (
        <rect
          key={names[i]}
          className="pareto-bar"
          x={i * slot}
          y={H - (v / peak) * H}
          width={barW}
          height={(v / peak) * H}
        >
          <title>{`${names[i]} · ${v} tokens · ${Math.round(running[i] * 100)}% cumulative`}</title>
        </rect>
      ))}
      <path className="pareto-curve" d={linePath(running, W, H)} vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

/* ── The long tail ─────────────────────────────────────────────────────── */

/**
 * Every other read route, listed from `GET /v1/schema`.
 *
 * The route table is generated from the table the server dispatches on and
 * "cannot drift from the implementation — they are the same array", so this
 * list gains a report the day one is added, with the description the table
 * already carries.
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
        /* The reports list is optional furniture; spend above is the report. */
      })
    return () => {
      live = false
    }
  }, [])

  if (!schema) return null

  // Read-only, project-scoped, no path parameter left to fill, and not one of
  // the surfaces that already has a screen of its own — which now includes
  // diff, explain, review and attest, since Changes gives all four a home with
  // their arguments attached.
  const own = [
    '/cost',
    '/context',
    '/pilot',
    '/sessions',
    '/diff',
    '/explain',
    '/review',
    '/attest',
  ]
  const routes = schema.routes.filter(
    (r) =>
      r.method === 'GET' &&
      r.path.includes('{project}') &&
      !r.path.includes('{run}') &&
      !r.path.includes('{id}') &&
      !own.some((o) => r.path.includes(o)),
  )

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
    <section className="panel">
      <h2 className="section-head">Reports</h2>
      <p className="section-intro">
        Listed from <code>GET /v1/schema</code>, which is generated from the server's own route
        table — so a report added to the CLI appears here without this page changing. Each one runs
        the same subcommand a terminal would.
      </p>

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
              <span className="figure report-name">{tail}</span>
              <span className="muted report-about">{r.about ?? r.returns ?? ''}</span>
              <span className="figure report-action">
                {running ? 'running…' : open === tail ? 'shown below' : 'Run'}
              </span>
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
        <>
          <h3 className="chart-head figure">{open}</h3>
          <Output value={result} />
        </>
      )}
    </section>
  )
}

/**
 * Render whatever a table route returned.
 *
 * The routes promise exactly this: output that parses as JSON comes back as
 * JSON, anything else as `{stdout, stderr, exit}` which "is honest about being
 * text". So this checks which it got instead of assuming.
 *
 * Shared with the Changes screen and the Overview's maintenance list, so a
 * command's output looks the same wherever it is run from.
 */
export function Output({ value }: { value: unknown }) {
  if (isTextOutput(value)) {
    return (
      <>
        {value.exit !== 0 && <p className="is-failed dot figure">exited {value.exit}</p>}
        {value.stdout ? <Report text={value.stdout} /> : <pre className="report figure">(no output)</pre>}
        {value.stderr.trim() && <pre className="report figure is-failed">{value.stderr}</pre>}
      </>
    )
  }
  return <pre className="report figure">{JSON.stringify(value, null, 2)}</pre>
}

/**
 * A command's stdout, with its verdicts carrying the palette.
 *
 * `doctor` is the report the README brags about — which containment is
 * actually active on this machine — and it rendered as an undifferentiated
 * block of terminal text. It already marks each line with a glyph, so this
 * gives those lines the hue that glyph already means. Nothing is *invented*:
 * a line with no verdict marker gets no colour, which is the same rule the
 * rest of the panel follows.
 */
function Report({ text }: { text: string }) {
  return (
    <pre className="report figure">
      {text.split('\n').map((line, i) => {
        const cls = verdict(line)
        return (
          <span key={i} className={cls ?? undefined}>
            {line}
            {'\n'}
          </span>
        )
      })}
    </pre>
  )
}

/** The three states, from the glyphs the CLI already prints. */
export function verdict(line: string): string | null {
  const t = line.trimStart()
  if (/^[✓✔]/.test(t)) return 'is-proven'
  if (/^[✗✕✘×]/.test(t)) return 'is-failed'
  if (/^[⚠!]/.test(t)) return 'is-asserted'
  return null
}
