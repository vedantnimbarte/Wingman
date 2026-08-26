import { describe, expect, it } from 'vitest'
import type { SchemaNode } from './api'
import { at, fieldsOf, resolve, toPatch } from './schema'

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
