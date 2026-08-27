import { Fragment, type ReactNode } from 'react'

/**
 * Just enough markdown for what an agent actually writes back.
 *
 * The transcript rendered assistant text into a `<p>`, so every fenced code
 * block arrived as literal backticks and every list as a column of hyphens —
 * the single most visible gap in the panel, in the one view people read most.
 *
 * Written here rather than pulled from a package for the reason the icons
 * were: this is ~140 lines against a parser, a sanitiser and their transitive
 * trees, in a bundle a Rust binary embeds. The subset is chosen from what
 * models emit — headings, fences, lists, quotes, inline code, emphasis, links
 * — and anything outside it renders as its own source text, which is what the
 * `<p>` did for everything.
 *
 * **Nothing here produces HTML.** The output is React elements built from
 * parsed pieces, so there is no `dangerouslySetInnerHTML` and no sanitiser to
 * get wrong. Tool output is model-influenced text, and a renderer for it that
 * takes the HTML route is one prompt away from being an injection surface.
 */

export type Block =
  | { kind: 'p'; text: string }
  | { kind: 'heading'; level: 1 | 2 | 3; text: string }
  | { kind: 'code'; lang: string | null; text: string }
  | { kind: 'list'; ordered: boolean; items: string[] }
  | { kind: 'quote'; text: string }
  | { kind: 'rule' }

const FENCE = /^\s*(?:```|~~~)\s*([\w+-]*)\s*$/
const HEADING = /^(#{1,3})\s+(.*)$/
const BULLET = /^\s*[-*+]\s+(.*)$/
const NUMBERED = /^\s*\d+[.)]\s+(.*)$/
const QUOTE = /^\s*>\s?(.*)$/
const RULE = /^\s*(?:---+|\*\*\*+|___+)\s*$/

/**
 * Split source into blocks.
 *
 * Exported and pure so the awkward cases have somewhere to be tested: an
 * unterminated fence (a stream cut mid-block, which happens on every turn
 * while it is still arriving), a fence containing what looks like a heading,
 * and a list interrupted by a blank line.
 */
export function parse(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const out: Block[] = []
  let i = 0

  while (i < lines.length) {
    const line = lines[i]

    const fence = FENCE.exec(line)
    if (fence) {
      const lang = fence[1] || null
      const body: string[] = []
      i++
      // An unterminated fence is the normal state of a streaming answer, so it
      // closes at end-of-input rather than falling back to paragraphs — which
      // would make the block flicker from code to prose and back on every
      // chunk that arrives.
      while (i < lines.length && !FENCE.test(lines[i])) body.push(lines[i++])
      if (i < lines.length) i++
      out.push({ kind: 'code', lang, text: body.join('\n') })
      continue
    }

    if (!line.trim()) {
      i++
      continue
    }

    if (RULE.test(line)) {
      out.push({ kind: 'rule' })
      i++
      continue
    }

    const heading = HEADING.exec(line)
    if (heading) {
      out.push({
        kind: 'heading',
        level: heading[1].length as 1 | 2 | 3,
        text: heading[2].trim(),
      })
      i++
      continue
    }

    if (QUOTE.test(line)) {
      const body: string[] = []
      while (i < lines.length && QUOTE.test(lines[i])) body.push(QUOTE.exec(lines[i++])![1])
      out.push({ kind: 'quote', text: body.join('\n').trim() })
      continue
    }

    const ordered = NUMBERED.test(line)
    if (ordered || BULLET.test(line)) {
      const items: string[] = []
      const re = ordered ? NUMBERED : BULLET
      // A list ends at the first line that is not an item of the *same* kind:
      // a bulleted list under a numbered one is two lists, not one with odd
      // markers.
      while (i < lines.length && re.test(lines[i])) items.push(re.exec(lines[i++])![1].trim())
      out.push({ kind: 'list', ordered, items })
      continue
    }

    // A paragraph runs until a blank line or anything that starts a block.
    const body: string[] = []
    while (
      i < lines.length &&
      lines[i].trim() &&
      !FENCE.test(lines[i]) &&
      !HEADING.test(lines[i]) &&
      !QUOTE.test(lines[i]) &&
      !RULE.test(lines[i]) &&
      !BULLET.test(lines[i]) &&
      !NUMBERED.test(lines[i])
    ) {
      body.push(lines[i++])
    }
    out.push({ kind: 'p', text: body.join('\n') })
  }

  return out
}

/**
 * `javascript:` in a link is the one way a text renderer becomes an execution
 * surface. Only the three schemes anyone actually writes are followed;
 * anything else renders as its own text, visibly, rather than as a dead link
 * that looks live.
 */
export function safeHref(href: string): string | null {
  const trimmed = href.trim()
  return /^(https?:\/\/|mailto:|\/)/i.test(trimmed) ? trimmed : null
}

const INLINE =
  /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*\n]+\*)|(_[^_\n]+_)|(\[[^\]\n]*\]\([^)\s]+\))/

/** Inline spans, as React nodes. Recursion is bounded by the string shrinking. */
export function inline(text: string, key = 0): ReactNode[] {
  const out: ReactNode[] = []
  let rest = text
  let n = key

  for (;;) {
    const m = INLINE.exec(rest)
    if (!m || m.index === undefined) break
    if (m.index > 0) out.push(rest.slice(0, m.index))
    const tok = m[0]
    n++

    if (tok.startsWith('`')) {
      out.push(
        <code key={n} className="md-code">
          {tok.slice(1, -1)}
        </code>,
      )
    } else if (tok.startsWith('**')) {
      out.push(<strong key={n}>{tok.slice(2, -2)}</strong>)
    } else if (tok.startsWith('[')) {
      const split = tok.indexOf('](')
      const label = tok.slice(1, split)
      const href = safeHref(tok.slice(split + 2, -1))
      out.push(
        href ? (
          <a key={n} href={href} target="_blank" rel="noreferrer noopener">
            {label || href}
          </a>
        ) : (
          tok
        ),
      )
    } else {
      out.push(<em key={n}>{tok.slice(1, -1)}</em>)
    }

    rest = rest.slice(m.index + tok.length)
  }

  if (rest) out.push(rest)
  return out
}

export function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      {parse(text).map((b, i) => (
        <Rendered key={i} block={b} />
      ))}
    </div>
  )
}

function Rendered({ block }: { block: Block }) {
  switch (block.kind) {
    case 'code':
      return (
        <pre className="md-block figure" data-lang={block.lang ?? undefined}>
          <code>{block.text}</code>
        </pre>
      )
    case 'heading': {
      // Never `<h1>`: the page already has one, and a model that opens its
      // answer with `#` would otherwise give the document two.
      const Tag = block.level === 1 ? 'h3' : block.level === 2 ? 'h4' : 'h5'
      return <Tag className="md-head">{inline(block.text)}</Tag>
    }
    case 'list':
      return block.ordered ? (
        <ol className="md-list">
          {block.items.map((it, i) => (
            <li key={i}>{inline(it)}</li>
          ))}
        </ol>
      ) : (
        <ul className="md-list">
          {block.items.map((it, i) => (
            <li key={i}>{inline(it)}</li>
          ))}
        </ul>
      )
    case 'quote':
      return <blockquote className="md-quote">{inline(block.text)}</blockquote>
    case 'rule':
      return <hr className="md-rule" />
    default:
      return (
        <p className="md-p">
          {block.text.split('\n').map((line, i) => (
            <Fragment key={i}>
              {i > 0 && <br />}
              {inline(line, i * 100)}
            </Fragment>
          ))}
        </p>
      )
  }
}
