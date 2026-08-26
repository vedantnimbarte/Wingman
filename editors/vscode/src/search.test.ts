import { strict as assert } from "node:assert";
import { test } from "node:test";

import { parseHits } from "./search";

// Verbatim shape of `wingman mcp-serve` -> tools/call semantic_search.
const SAMPLE = `[1] crates/wingman-autonomous/src/checkpoint.rs:114-128  (score 0.030)  [fn:tool_calls_for_task]
pub fn tool_calls_for_task(events: &[Event]) -> Vec<ToolCall> {
    events.iter().collect()
}

[2] crates/wingman-autonomous/src/prompts/tool-smith.md:1-32  (score 0.027)
# Tool-smith

You are the **tool-smith**.
`;

test("parses each hit's location", () => {
  const hits = parseHits(SAMPLE);
  assert.equal(hits.length, 2);
  assert.equal(hits[0].path, "crates/wingman-autonomous/src/checkpoint.rs");
  assert.equal(hits[0].startLine, 114);
  assert.equal(hits[0].symbol, "tool_calls_for_task");
  assert.equal(hits[0].score, "0.030");
});

test("a hit without a symbol still parses", () => {
  const hits = parseHits(SAMPLE);
  assert.equal(hits[1].path, "crates/wingman-autonomous/src/prompts/tool-smith.md");
  assert.equal(hits[1].startLine, 1);
  assert.equal(hits[1].symbol, undefined);
});

test("snippet stops at the next hit, not the end of the text", () => {
  const hits = parseHits(SAMPLE);
  assert.ok(hits[0].snippet.startsWith("pub fn tool_calls_for_task"));
  assert.ok(!hits[0].snippet.includes("Tool-smith"));
  assert.ok(hits[1].snippet.endsWith("the **tool-smith**."));
});

test("non-result text yields nothing to navigate to", () => {
  assert.deepEqual(parseHits("(no results)"), []);
  assert.deepEqual(parseHits(""), []);
});

// Windows paths carry a drive letter, so the `path:line` split must not trip
// on the colon after the drive.
test("absolute windows paths keep their drive letter", () => {
  const hits = parseHits("[1] C:\\repo\\src\\main.rs:7-9  (score 0.5)\ncode\n");
  assert.equal(hits[0].path, "C:\\repo\\src\\main.rs");
  assert.equal(hits[0].startLine, 7);
});
