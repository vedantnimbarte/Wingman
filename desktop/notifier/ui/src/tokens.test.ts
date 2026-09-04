import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

/*
 * `tokens.css` is a copy of the panel's token block — see the note at the top
 * of that file for why it is copied rather than imported. This is the only
 * thing standing between the two and silent drift: change a hue in the panel
 * and forget the popup, and the two surfaces start disagreeing about what
 * "failed" looks like.
 */

function read(rel: string): string {
  return readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8')
}

/** Custom-property declarations, as `name -> value`, from the first block a
 *  selector opens. Good enough for a flat token file and no parser. */
function tokens(css: string, after: string): Map<string, string> {
  const start = css.indexOf(after)
  expect(start, `no ${after} block`).toBeGreaterThanOrEqual(0)
  const block = css.slice(start, css.indexOf('\n}', start))
  const out = new Map<string, string>()
  for (const [, name, value] of block.matchAll(/(--[\w-]+):\s*([^;]+);/g)) {
    out.set(name, value.trim().replace(/\s+/g, ' '))
  }
  return out
}

const panel = read('../../../../panel/src/app.css')
const popup = read('./tokens.css')

describe('design tokens', () => {
  it('agree with the panel in light mode', () => {
    const theirs = tokens(panel, ':root {')
    const ours = tokens(popup, ':root {')
    expect(ours.size).toBeGreaterThan(15)
    for (const [name, value] of ours) {
      expect(theirs.get(name), `${name} drifted from the panel`).toBe(value)
    }
  })

  it('agree with the panel in dark mode', () => {
    const theirs = tokens(panel, "@media (prefers-color-scheme: dark) {")
    const ours = tokens(popup, '@media (prefers-color-scheme: dark) {')
    expect(ours.size).toBeGreaterThan(10)
    for (const [name, value] of ours) {
      expect(theirs.get(name), `${name} drifted from the panel`).toBe(value)
    }
  })

  it('carry the three status hues, which are what the colour rule is about', () => {
    const ours = tokens(popup, ':root {')
    for (const hue of ['--proven', '--asserted', '--failed']) {
      expect(ours.has(hue), `${hue} missing`).toBe(true)
    }
  })

  it('never paint the window itself, or the popup becomes a grey rectangle', () => {
    // The window is transparent and frameless: the cards are the only thing
    // drawn, and the gaps between them are the user's desktop.
    expect(popup).toMatch(/html,\s*\n?body\s*\{[^}]*background:\s*transparent/)
  })
})
