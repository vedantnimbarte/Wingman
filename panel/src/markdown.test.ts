import { describe, expect, it } from 'vitest'
import { parse, safeHref } from './markdown'

/**
 * The parser, not the rendering.
 *
 * Every case here is one that produced visibly wrong output while it was being
 * written: a fence that swallowed the rest of the transcript, a `#` inside a
 * code block promoted to a heading, and `javascript:` surviving into an
 * `href`. A markdown renderer looks correct on the happy path by definition.
 */

describe('parse', () => {
  it('reads a fenced block, keeping its language and its blank lines', () => {
    const out = parse('before\n\n```rust\nfn main() {\n\n}\n```\n\nafter')
    expect(out.map((b) => b.kind)).toEqual(['p', 'code', 'p'])
    expect(out[1]).toEqual({ kind: 'code', lang: 'rust', text: 'fn main() {\n\n}' })
  })

  it('leaves an unterminated fence as code', () => {
    // The normal state of a streaming answer. Falling back to paragraphs would
    // make the block flicker between prose and code on every chunk.
    const out = parse('```\nhalf a func')
    expect(out).toEqual([{ kind: 'code', lang: null, text: 'half a func' }])
  })

  it('does not find headings or lists inside a fence', () => {
    const out = parse('```\n# not a heading\n- not a list\n```')
    expect(out).toHaveLength(1)
    expect(out[0]).toMatchObject({ kind: 'code', text: '# not a heading\n- not a list' })
  })

  it('reads the three heading levels and stops there', () => {
    const out = parse('# one\n## two\n### three\n#### four')
    expect(out.map((b) => b.kind)).toEqual(['heading', 'heading', 'heading', 'p'])
    expect(out.map((b) => ('level' in b ? b.level : null))).toEqual([1, 2, 3, null])
  })

  it('keeps a bulleted and a numbered list apart', () => {
    const out = parse('- a\n- b\n1. one\n2. two')
    expect(out).toEqual([
      { kind: 'list', ordered: false, items: ['a', 'b'] },
      { kind: 'list', ordered: true, items: ['one', 'two'] },
    ])
  })

  it('joins a wrapped paragraph and ends it at a block', () => {
    const out = parse('one\ntwo\n> quoted\n---')
    expect(out).toEqual([
      { kind: 'p', text: 'one\ntwo' },
      { kind: 'quote', text: 'quoted' },
      { kind: 'rule' },
    ])
  })

  it('survives empty input', () => {
    expect(parse('')).toEqual([])
    expect(parse('\n\n\n')).toEqual([])
  })
})

describe('safeHref', () => {
  it('follows the three schemes anyone writes', () => {
    expect(safeHref('https://example.com')).toBe('https://example.com')
    expect(safeHref('http://localhost:8787/x')).toBe('http://localhost:8787/x')
    expect(safeHref('mailto:you@example.com')).toBe('mailto:you@example.com')
    expect(safeHref('/runs/2026-08-21-2005-abc')).toBe('/runs/2026-08-21-2005-abc')
  })

  it('refuses anything that could execute', () => {
    // The one way a text renderer becomes an execution surface, and the text
    // being rendered is model-influenced.
    expect(safeHref('javascript:alert(1)')).toBeNull()
    expect(safeHref('  JavaScript:alert(1)')).toBeNull()
    expect(safeHref('data:text/html,<script>')).toBeNull()
    expect(safeHref('vbscript:msgbox')).toBeNull()
  })
})
