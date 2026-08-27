# 0015 — `run_plan` instead of Code Mode

**Status:** accepted
**Date:** 2026-08-27

## Context

P12 of [the DSH adoption review](../DSH-ADOPTION.md) proposed **Code Mode**: an
embedded JavaScript runtime and a generated SDK, so the model writes one
program instead of emitting N rounds of tool-call JSON.

```js
const hits = await tools.grep({ pattern: "TurnGate" })
for (const h of hits) await tools.read({ path: h.path })
```

It was sized L–XL and described as "the largest available win on the metric
Wingman headlines". P12 also said how to find out: prototype behind a flag,
measure the token delta, and only then decide. This is that measurement, and
it moved the answer.

## What measuring first changed

**The agent loop already batches.** It emits many tool calls per assistant
message, and runs read-only ones *concurrently* — `AgentConfig::
parallel_safe_tools` lists 15 of them against `mutating_tools`, and
`futures::buffered` dispatches them in parallel while preserving order.

So the premise does not hold here. Independent work already costs one round
trip:

```
grep(a) + glob(b) + read_file(c)   -> one request, run in parallel
```

A language runtime buys nothing for that case, which is most cases. What the
loop genuinely cannot express is a **dependent** chain: the arguments of call 2
live inside the output of call 1, and the model must see the first result
before it can write the second call.

That is the entire remaining gap — and it costs **one** round trip, not N,
precisely because the fan-out half is already batched:

| | without | with |
|---|---|---|
| dependent chain (grep, then read each hit) | 3 requests | 2 requests |

## Decision

Build the small thing that closes that specific gap, and not the runtime.

`run_plan` takes a list of steps. A step names a tool and its arguments, and
may carry a `for_each` that runs it once per value captured from an earlier
step's output, with `{}` substituted into its arguments. Eight steps, 32 calls,
no nesting, no branching.

No language was needed, because the gap is data flow, not computation. Skipped
along with the interpreter: arithmetic, conditionals, unbounded loops, and
every sandbox-escape question an embedded runtime would have raised.

**Every call re-enters `ToolDispatcher::dispatch`, never `Tool::run`.** That
one choice is the whole security argument. `dispatch` is where the capability
gate, pre/post hooks, undo checkpoints, the audit trail, secret redaction, the
repeat guard and the per-call deadline live, so a step is gated exactly as the
identical call would be alone. `a_plan_cannot_write_in_read_only_mode` is the
test that says so. Had the tool held `Arc<dyn Tool>` handles and called them
directly — the obvious shortcut — it would have bypassed all seven at once.

Termination is structural rather than a timeout: fixed list, capped fan-out, no
recursion. So `owns_timeout` is `true`; wrapping a whole plan in one 120s
backstop would kill a legitimate chain partway and leave its earlier writes
applied.

## Off by default

`[tools].run_plan = false`, and not in `PROJECT_SAFE_TOOLS_KEYS`, so a project
config cannot turn it on. Two measured reasons:

- **It is expensive to carry.** Its schema is ~430 tokens — **9.9% of the
  entire tool list** — on every request, whether or not it is used.
  (`cargo run --example plan_cost -p wingman-tools`.)
- **What it saves is mostly latency, not tokens.** One round trip per dependent
  chain. The conversation prefix is re-sent either way, and with
  `[tokens].prompt_cache` on it is a cache read at ~10% of input price. So the
  saving is one request of latency plus the output tokens for N tool calls —
  real, but not the order-of-magnitude win the proposal implied.

It also widens blast radius per model decision: one approved tool call can now
set 32 in motion. Permissions still hold, but "how much happens per decision"
is a different axis from "what may happen", and it is the user's call.

## Consequences

- Only the outer registry gets it. Subagents build their own via
  `base_registry`, so a plan is unreachable from inside one — the same way
  `spawn_subagent` is withheld from them to bound recursion.
- The tool description tells the model *not* to use it for independent calls.
  Getting that wrong is a straight regression: slower and more expensive than
  emitting the calls side by side.
- Output is clipped per call (4000 chars) before the turn's own output budget
  sees it, so one large file cannot crowd out the other 31 results.
- The registry reference is `Weak`. A strong one would make the registry own a
  tool that owns the registry, and neither would ever drop.

## What would change this

Evidence from real sessions that dependent chains are common enough to be
worth 430 standing tokens — then reconsider the default, not the design. If
they turn out to be rare, delete the tool; that is the cheaper outcome and the
reason it shipped behind a flag.

A real Code Mode still needs a real justification. The one offered was that
multi-step work costs N round trips, and in Wingman it does not.

## A related bug, not fixed here

`disabled_tools` cannot remove `spawn_subagent`: `apply_tool_removals` runs
inside `build_registry_with_learn`, and `spawn_subagent` is registered *after*
that, once the `Arc` exists. Naming it in `disabled_tools` silently does
nothing. `run_plan` registers at the same point and would have inherited the
same hole, so it checks `disabled_tools` itself. The `spawn_subagent` case is
left alone deliberately — it is out of scope for P12 and worth fixing where
the removal logic lives, not with a second one-off check.
