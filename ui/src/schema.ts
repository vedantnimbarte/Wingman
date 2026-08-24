import type { SchemaNode } from './api'

/**
 * Reading the config JSON Schema.
 *
 * Kept apart from the components because this is the part that can be wrong
 * without looking wrong — a `$ref` followed to the wrong place renders a
 * plausible form for the wrong type.
 */

/** How a field should be edited. The renderer switches on this and nothing else. */
export type FieldKind =
  | 'boolean'
  | 'integer'
  | 'number'
  | 'string'
  | 'enum'
  | 'string-list'
  /** Arrays of objects, maps of objects — edited as JSON, validated by the server. */
  | 'json'

export type Field = {
  key: string
  path: string[]
  kind: FieldKind
  node: SchemaNode
  description?: string
  /** Enum choices, each with its own `///` comment where one exists. */
  choices?: { value: string; description?: string }[]
  nullable: boolean
}

/**
 * Follow `$ref` and the `allOf: [{$ref}]` wrapper schemars emits for a field
 * that has a description or default alongside a reference.
 *
 * Bounded rather than recursive-until-resolved: a schema with a self-reference
 * would otherwise spin here, and a hung tab is a poor way to learn that.
 */
export function resolve(
  node: SchemaNode | undefined,
  defs: Record<string, SchemaNode>,
): SchemaNode {
  let cur = node ?? {}
  for (let hops = 0; hops < 10; hops++) {
    if (cur.$ref) {
      const name = cur.$ref.replace(/^#\/definitions\//, '')
      const target = defs[name]
      if (!target) return cur
      cur = target
      continue
    }
    // `allOf` with a single ref is how schemars attaches a description to a
    // referenced type. Merge so the field keeps its own description rather
    // than inheriting the definition's.
    if (cur.allOf?.length === 1 && cur.allOf[0].$ref) {
      const { allOf, ...rest } = cur
      cur = { ...resolve(allOf[0], defs), ...rest }
      continue
    }
    return cur
  }
  return cur
}

/** `["string", "null"]` → `"string"`, and remembers it was nullable. */
function typeOf(node: SchemaNode): { type: string | undefined; nullable: boolean } {
  const t = node.type
  if (Array.isArray(t)) {
    const real = t.find((x) => x !== 'null')
    return { type: real, nullable: t.includes('null') }
  }
  return { type: t, nullable: false }
}

/** String enums arrive as `oneOf` entries each carrying a single-value `enum`. */
function enumChoices(node: SchemaNode): { value: string; description?: string }[] | undefined {
  if (node.enum?.length) return node.enum.map((value) => ({ value }))
  if (!node.oneOf?.length) return undefined
  const out: { value: string; description?: string }[] = []
  for (const variant of node.oneOf) {
    // A `oneOf` branch that is not a plain single-value enum means a real sum
    // type, not a pick-one field. Bail rather than render a select that cannot
    // express it.
    if (!variant.enum || variant.enum.length !== 1) return undefined
    out.push({ value: variant.enum[0], description: variant.description })
  }
  return out
}

function kindOf(node: SchemaNode, choices?: { value: string }[]): FieldKind {
  if (choices) return 'enum'
  const { type } = typeOf(node)
  switch (type) {
    case 'boolean':
      return 'boolean'
    case 'integer':
      return 'integer'
    case 'number':
      return 'number'
    case 'string':
      return 'string'
    case 'array': {
      const item = node.items
      const it = item ? typeOf(item).type : undefined
      // A list of scalars edits fine as one-per-line. A list of objects does
      // not, and pretending otherwise loses data on save.
      return it === 'string' || it === 'integer' || it === 'number' ? 'string-list' : 'json'
    }
    default:
      return 'json'
  }
}

/** Flatten one section into the fields a form should show, nested objects included. */
export function fieldsOf(
  section: SchemaNode,
  defs: Record<string, SchemaNode>,
  prefix: string[] = [],
  depth = 0,
): Field[] {
  const resolved = resolve(section, defs)
  const props = resolved.properties
  // A map keyed by user-chosen names (`[mcp.<name>]`, `[providers.<name>]`)
  // has no fixed properties, so there is no form to generate — it is edited
  // whole, as JSON, and the server validates it.
  if (!props || depth > 3) return []

  const out: Field[] = []
  for (const [key, raw] of Object.entries(props)) {
    const node = resolve(raw, defs)
    const { type, nullable } = typeOf(node)
    const choices = enumChoices(node)

    if (!choices && type === 'object' && node.properties) {
      out.push(...fieldsOf(node, defs, [...prefix, key], depth + 1))
      continue
    }

    out.push({
      key,
      path: [...prefix, key],
      kind: kindOf(node, choices),
      node,
      description: node.description,
      choices,
      nullable,
    })
  }
  return out
}

/** Read `a.b.c` out of a nested object. */
export function at(obj: unknown, path: string[]): unknown {
  let cur: unknown = obj
  for (const k of path) {
    if (cur == null || typeof cur !== 'object') return undefined
    cur = (cur as Record<string, unknown>)[k]
  }
  return cur
}

/** Build the nested object a patch needs from a flat map of path → value. */
export function toPatch(edits: Map<string, unknown>): Record<string, unknown> {
  const root: Record<string, unknown> = {}
  for (const [joined, value] of edits) {
    const path = joined.split('.')
    let cur = root
    for (const k of path.slice(0, -1)) {
      if (typeof cur[k] !== 'object' || cur[k] === null) cur[k] = {}
      cur = cur[k] as Record<string, unknown>
    }
    cur[path[path.length - 1]] = value
  }
  return root
}
