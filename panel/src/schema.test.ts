import { describe, expect, it } from 'vitest'
import type { SchemaNode } from './api'
import { at, fieldsOf, optionsFor, resolve, toPatch } from './schema'

/**
 * Schema shapes here are copied from what `GET /v1/config/schema` actually
 * emits (schemars 0.8), not invented — a form generator tested against an
 * idealised schema is tested against the wrong thing.
 */

const defs: Record<string, SchemaNode> = {
  VerifyConfig: {
    type: 'object',
    description: 'The verification gate.',
    properties: {
      max_retries: { type: 'integer', default: 2, description: 'How many gate failures…' },
      affected_tests: { type: 'boolean', default: true },
      turn_gate: { type: 'string', default: 'on' },
    },
  },
  PermissionMode: {
    description: 'Permission model.',
    oneOf: [
      { type: 'string', enum: ['read-only'], description: 'Reads free; writes prompt.' },
      { type: 'string', enum: ['auto-edit'], description: 'Edits apply.' },
    ],
  },
  Nested: {
    type: 'object',
    properties: {
      inner: { type: 'object', properties: { leaf: { type: 'string' } } },
    },
  },
  // Copied verbatim from `wingman_config::json_schema()`. This is the shape
  // every `schemars(with = …)` shadow enum produces — nine fields today — and
  // it is *not* the plain `$ref` above: the field keeps its own description
  // and default, and the choices sit one `allOf` hop away.
  ReasoningLevel: {
    description: 'The four canonical `reasoning` levels, for the schema only.',
    oneOf: [
      { type: 'string', enum: ['off'], description: 'No reasoning. The default…' },
      { type: 'string', enum: ['low'], description: 'A short budget…' },
      { type: 'string', enum: ['medium'], description: 'Anthropic 16384 thinking tokens…' },
      { type: 'string', enum: ['high'], description: 'Anthropic 32768 thinking tokens…' },
    ],
  },
}

describe('resolve', () => {
  it('follows a plain $ref', () => {
    expect(resolve({ $ref: '#/definitions/VerifyConfig' }, defs).description).toBe(
      'The verification gate.',
    )
  })

  it('keeps the field description over the definition it points at', () => {
    // schemars emits this shape whenever a field has both a `///` comment and
    // a referenced type. The field's own words are the more specific ones.
    const node: SchemaNode = {
      allOf: [{ $ref: '#/definitions/VerifyConfig' }],
      description: 'Verification settings for this repo.',
      default: {},
    }
    const out = resolve(node, defs)
    expect(out.description).toBe('Verification settings for this repo.')
    expect(out.properties?.max_retries).toBeDefined()
  })

  it('returns the node untouched when the target is missing', () => {
    const node = { $ref: '#/definitions/Ghost' }
    expect(resolve(node, defs)).toEqual(node)
  })

  it('gives up rather than spinning on a self-reference', () => {
    const loop: Record<string, SchemaNode> = { A: { $ref: '#/definitions/A' } }
    expect(resolve({ $ref: '#/definitions/A' }, loop)).toBeDefined()
  })
})

describe('fieldsOf', () => {
  it('classifies each field by how it should be edited', () => {
    const fields = fieldsOf({ $ref: '#/definitions/VerifyConfig' }, defs)
    const kinds = Object.fromEntries(fields.map((f) => [f.key, f.kind]))
    expect(kinds).toEqual({ max_retries: 'integer', affected_tests: 'boolean', turn_gate: 'string' })
  })

  it('carries the doc comment through as help text', () => {
    const fields = fieldsOf({ $ref: '#/definitions/VerifyConfig' }, defs)
    expect(fields.find((f) => f.key === 'max_retries')?.description).toBe('How many gate failures…')
  })

  it('turns a oneOf of single-value enums into choices with their descriptions', () => {
    const fields = fieldsOf(
      { type: 'object', properties: { mode: { $ref: '#/definitions/PermissionMode' } } },
      defs,
    )
    const mode = fields[0]
    expect(mode.kind).toBe('enum')
    expect(mode.choices).toEqual([
      { value: 'read-only', description: 'Reads free; writes prompt.' },
      { value: 'auto-edit', description: 'Edits apply.' },
    ])
  })

  it('flattens nested objects into dotted paths', () => {
    const fields = fieldsOf({ $ref: '#/definitions/Nested' }, defs)
    expect(fields.map((f) => f.path.join('.'))).toEqual(['inner.leaf'])
  })

  it('remembers that an optional field can be unset', () => {
    const fields = fieldsOf(
      { type: 'object', properties: { model: { type: ['string', 'null'] } } },
      defs,
    )
    expect(fields[0].nullable).toBe(true)
    expect(fields[0].kind).toBe('string')
  })

  it('edits a list of scalars as text and a list of objects as JSON', () => {
    const fields = fieldsOf(
      {
        type: 'object',
        properties: {
          names: { type: 'array', items: { type: 'string' } },
          hooks: { type: 'array', items: { type: 'object' } },
        },
      },
      defs,
    )
    const kinds = Object.fromEntries(fields.map((f) => [f.key, f.kind]))
    // Flattening a list of objects to one-per-line would lose data on save.
    expect(kinds).toEqual({ names: 'string-list', hooks: 'json' })
  })

  it('has no fields for a map keyed by user-chosen names', () => {
    // `[mcp.<name>]` has no fixed properties, so it is edited whole as JSON.
    const fields = fieldsOf(
      { type: 'object', additionalProperties: { $ref: '#/definitions/VerifyConfig' } },
      defs,
    )
    expect(fields).toEqual([])
  })
})

describe('at', () => {
  const cfg = { verify: { max_retries: 2, browser: { url: '' } } }

  it('reads a nested path', () => {
    expect(at(cfg, ['verify', 'max_retries'])).toBe(2)
    expect(at(cfg, ['verify', 'browser', 'url'])).toBe('')
  })

  it('returns undefined rather than throwing on a missing path', () => {
    expect(at(cfg, ['nope', 'deeper'])).toBeUndefined()
    expect(at(cfg, ['verify', 'max_retries', 'deeper'])).toBeUndefined()
  })
})

describe('toPatch', () => {
  it('rebuilds the nesting the server merges on', () => {
    const edits = new Map<string, unknown>([
      ['verify.max_retries', 3],
      ['verify.browser.threshold', 0.05],
      ['default_model', 'opus'],
    ])
    expect(toPatch(edits)).toEqual({
      verify: { max_retries: 3, browser: { threshold: 0.05 } },
      default_model: 'opus',
    })
  })

  it('sends only what changed, so untouched siblings are never rewritten', () => {
    expect(toPatch(new Map([['verify.max_retries', 3]]))).toEqual({ verify: { max_retries: 3 } })
  })

  it('is empty when nothing was edited', () => {
    expect(toPatch(new Map())).toEqual({})
  })
})

/**
 * The `String`-typed enums.
 *
 * `wingman-config` has nine fields that are `String` in Rust and a choice in
 * the schema, wired with `schemars(with = …)`. A test over there asserts each
 * one still references its definition. Nothing asserted this end — that the
 * panel turns that shape into a dropdown — and the two halves are only useful
 * together: a schema that offers choices no form reads is the same free-text
 * box it was before.
 */
describe('the schema-only enums', () => {
  const field = {
    allOf: [{ $ref: '#/definitions/ReasoningLevel' }],
    default: 'off',
    description: 'How hard the model should think before answering.',
  }

  it('reads a choice through the allOf hop the real schema uses', () => {
    const [reasoning] = fieldsOf({ type: 'object', properties: { reasoning: field } }, defs)
    expect(reasoning.kind).toBe('enum')
    expect(reasoning.choices?.map((c) => c.value)).toEqual(['off', 'low', 'medium', 'high'])
  })

  it('keeps the field’s own documentation, not the shadow type’s', () => {
    // The shadow enum's doc comment explains why it exists to a Rust reader.
    // Putting that in the form's help text would be telling the user about an
    // implementation detail instead of about the setting.
    const [reasoning] = fieldsOf({ type: 'object', properties: { reasoning: field } }, defs)
    expect(reasoning.description).toBe('How hard the model should think before answering.')
  })
})

describe('optionsFor', () => {
  const levels = [
    { value: 'off', description: 'No reasoning.' },
    { value: 'low' },
    { value: 'medium' },
    { value: 'high' },
  ]

  it('offers exactly the schema list when the value is one of them', () => {
    expect(optionsFor(levels, 'medium').map((c) => c.value)).toEqual([
      'off',
      'low',
      'medium',
      'high',
    ])
    expect(optionsFor(levels, 'medium').every((c) => c.listed)).toBe(true)
  })

  it('carries an accepted alias in rather than letting the select lie', () => {
    // `ReasoningEffort::parse` takes `med`. A controlled select whose value
    // matches no option renders its *first* option, so this displayed "off" —
    // a config that says medium reading as reasoning turned off.
    const out = optionsFor(levels, 'med')
    expect(out[0]).toEqual({ value: 'med', listed: false })
    expect(out.map((c) => c.value)).toEqual(['med', 'off', 'low', 'medium', 'high'])
  })

  it('does the same for a theme name the resolver would fall back on', () => {
    // `theme::resolve` accepts any string and falls back to the default, so
    // the value is legal and the schema still does not list it.
    const themes = [{ value: 'default' }, { value: 'light' }, { value: 'mono' }]
    expect(optionsFor(themes, 'solarized')[0]).toEqual({ value: 'solarized', listed: false })
  })

  it('leaves unset alone — that is the nullable option’s job', () => {
    expect(optionsFor(levels, '')).toHaveLength(4)
    expect(optionsFor(levels, undefined)).toHaveLength(4)
    expect(optionsFor(levels, 3)).toHaveLength(4)
  })

  it('survives a field the schema gave no choices at all', () => {
    expect(optionsFor(undefined, 'anything')).toEqual([{ value: 'anything', listed: false }])
  })
})
