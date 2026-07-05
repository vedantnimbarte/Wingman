# Self-Improving Learning Loop

Wingman builds a persistent model of you and your projects across sessions. This document explains how memories, skill tracking, and session recall work together.

## Overview

The learning loop comprises four subsystems:

1. **Memory Store** — Persistent markdown notes (user prefs, project facts, feedback).
2. **Skill Stats** — SQLite database tracking which skills are used and their success rate.
3. **Session Index** — Embedding and semantic search over past conversation transcripts.
4. **Learning Hooks** — Agent loop integration points that trigger persistence and tracking.

All data is local-first — no cloud component. Everything lives under `~/.wingman/` (global) and `<project>/.wingman/` (project-scoped).

## Memory Store

### Structure

Memories are stored as markdown files with YAML frontmatter, indexed in a sibling `MEMORY.md` file.

**File layout:**
```
~/.wingman/memory/
├── MEMORY.md                    # index: one bullet per memory
├── user_role.md
├── feedback_testing.md
├── reference_issue_tracker.md
└── …

<project>/.wingman/memory/
├── MEMORY.md
├── project_build_command.md
└── …
```

**Memory file format:**
```markdown
---
name: user-role
description: Role, expertise, preferences
type: user
---

I'm a senior Rust engineer working on infrastructure.
I prefer pnpm over npm. I avoid mocks in tests.
```

### Memory Types

| Type        | Default Scope | Purpose                                 | Examples                                |
|-------------|---------------|-----------------------------------------|------------------------------------------|
| `user`      | global        | Facts about the human                   | Role, expertise, working style           |
| `feedback`  | global        | How to behave (constraints, preferences)| "avoid mocks", "use real DB", "be terse" |
| `project`   | project       | Facts about this codebase               | Build commands, naming conventions       |
| `reference` | global        | Pointers to external systems            | Issue tracker URL, CI dashboard          |

### Memory Index

The `MEMORY.md` index is rendered into the system prompt at every turn. It looks like:

```markdown
# Memory Index

- [user-role](user_role.md) — Senior Rust engineer, prefers pnpm
- [feedback-testing](feedback_testing.md) — Avoid mocks; use real DB
- [project-build](project_build.md) — `cargo build --release` in crates/foo
```

Each bullet is ~100 bytes and helps the agent decide which full memories to fetch with `recall_memory()`.

### Saving Memories

**User request:**
```
Tell the TUI: "Remember that I prefer pnpm over npm"
```

**Agent workflow:**
1. Agent calls `save_memory` tool with:
   - `name`: "user-pkg-manager"
   - `type`: "feedback"
   - `body`: "I use pnpm for all package installs, not npm."
2. `MemoryStore::save()` writes the memory file and updates `MEMORY.md` index.
3. Next session loads the index and surfaces it in the system prompt.

**Recall:**
```
User: "Have we discussed package managers before?"
Agent sees "user-pkg-manager" in the memory index and calls recall_memory()
→ full body fetched → agent incorporates into response.
```

**Forgetting:**
```
User: "Forget that note about pnpm"
Agent calls forget_memory("user-pkg-manager")
→ file deleted, index updated.
```

## Skill Usage Tracking

### Data Model

Skills are tracked in `~/.wingman/learn.db` (SQLite). Each invocation records:
- Skill name
- Timestamp
- Outcome (success / corrected / unclear)

### Outcome Scoring

After the agent calls `invoke_skill`, the system monitors the next user turn for:

| Heuristic         | Outcome        | Signals                                     |
|-------------------|----------------|---------------------------------------------|
| Success           | `success`      | Next turn doesn't mention the skill result. |
| Corrected         | `corrected`    | "no", "wait", "wrong", "actually", etc.     |
| Unclear           | `unclear`      | Skill not mentioned in next turn; ambiguous. |

### Skill Proposal Workflow

When a skill crosses a threshold:
- **3+ invocations** AND **≥50% correction rate** → skill flagged for rewrite.
- Next session: system prompt includes suggestion: *"Skill X has been corrected often. Consider refining it."*
- User can run `wingman skill extract` to generate a new draft from recent sessions.

### Viewing Statistics

```bash
# List all skills with usage counts
wingman skill stats

# Detailed breakdown of one skill
wingman skill stats my-skill
# Output: my-skill: invoked 5 times, success 3, corrected 1, unclear 1
```

## Session Index & Recall

### Session Embedding

After a session ends, the system:
1. Chunks the conversation transcript into thread-shaped windows (last N turns with context).
2. Embeds each chunk using the same embedder as RAG (fastembed BGE small or hash fallback).
3. Stores embeddings in `~/.wingman/sessions.db` (SQLite).

This happens asynchronously; the TUI remains responsive.

### Cross-Project Recall

When the agent sees a prompt like *"Have we done this before?"*, it calls `recall_session`:

```json
{"tool": "recall_session", "args": {"query": "fixing a race condition in tokio code"}}
```

The tool:
1. Embeds the query.
2. Searches `~/.wingman/sessions.db` for similar session chunks.
3. Returns top-5 results with project, session ID, and snippet.

The agent can then call `read_session(session_id)` to fetch the full transcript.

## Learning Hooks

The `LearnHook` trait is implemented by `LearnHook` in `crates/wingman-learn/src/hooks.rs`. It integrates into the agent loop at key points.

### Hook Points

**1. `on_session_start()`**
- Load memory indices (global + project).
- Check quiet-session counter (nudge user to save something after 5 quiet sessions).

**2. `on_tool_outcome()`**
- Record tool call into `learn.db` for skill stats (outcome determined next turn).
- Emit `"tool_outcome"` event (will be scored as success/corrected/unclear later).

**3. `on_user_turn()`**
- Score previous turn's tool outcomes (if any) based on heuristics.
- Check if user is saving/recalling memory or invoking skills.
- Emit `"turn_outcome"` event.

**4. `on_session_end()`**
- Chunk the full transcript.
- Embed chunks (async background task).
- Insert into `~/.wingman/sessions.db`.

## Nudges

The quiet-session nudge encourages memory creation when the agent hasn't saved anything in a while.

**Mechanism:**
- Counter stored in `~/.wingman/learn.db`: "quiet sessions since last save".
- On session start: if counter ≥ 5, append to system prompt:
  ```
  The user hasn't saved a memory in 5 sessions.
  If anything surprising came up, consider proposing to save it.
  ```
- On `save_memory`: reset counter to 0.
- `/learn reset`: manually reset counter.
- `/learn status`: show current counter.

## Data Files Reference

### Global (`~/.wingman/`)

| File/Dir            | Purpose                                  |
|---------------------|------------------------------------------|
| `memory/`           | Markdown memory files (user, feedback, reference). |
| `memory/MEMORY.md`  | Memory index.                            |
| `learn.db`          | Skill usage + outcome stats (SQLite).    |
| `sessions.db`       | Session embeddings for cross-project recall (SQLite). |

### Project-Scoped (`<project>/.wingman/`)

| File/Dir            | Purpose                                  |
|---------------------|------------------------------------------|
| `memory/`           | Project-local memories (mostly `project` type). |
| `memory/MEMORY.md`  | Project memory index.                    |
| `sessions/`         | Session JSONL files (append-only logs).  |
| `index.db`          | RAG index (semantic code search).        |

## Integration Examples

### Example 1: Learning a New Preference

```
User (in TUI): "Remember that I always run tests before commits"
    ↓
Agent calls: save_memory("user-testing-habit", type="feedback", body="...")
    ↓
MemoryStore writes ~/.wingman/memory/user_testing_habit.md
MemoryStore updates ~/.wingman/memory/MEMORY.md
    ↓
Next session:
  - MEMORY.md index loaded into system prompt
  - User: "Should I commit now?"
  - Agent sees testing-habit memory in index
  - Agent calls recall_memory("user-testing-habit")
  - Agent suggests running tests first
```

### Example 2: Skill Refinement

```
Session 1:
  Agent calls: invoke_skill("refactor-rust")
  Output used, but user says: "Actually, I want this refactored differently"
  → Outcome recorded as "corrected"

Session 2:
  Agent calls: invoke_skill("refactor-rust")
  Output used, user: "Wait, that's not quite right"
  → Outcome recorded as "corrected"

Session 3:
  Agent calls: invoke_skill("refactor-rust")
  Output used, user: "No, this doesn't compile"
  → Outcome recorded as "corrected"
    ↓
Skill crosses threshold (3 invokes, 100% corrected)
    ↓
Session 4 startup:
  System prompt: "Skill 'refactor-rust' has been corrected in all recent uses.
                  Consider refining it."
  User: wingman skill extract
    ↓
    New draft generated from recent sessions, saved to
    ~/.wingman/skills/proposed/refactor-rust.md for review.
```

### Example 3: Cross-Project Recall

```
Current project: Wingman

User: "How have we handled async error recovery in the past?"
    ↓
Agent calls: recall_session(query="async error recovery")
    ↓
SessionIndex searches ~/.wingman/sessions.db across all projects
    ↓
Returns:
  - Project "MyTokioApp" (2024-01-15), snippet about RetryPolicy
  - Project "MyRustServer" (2024-02-03), snippet about exponential backoff
    ↓
Agent: "We've used exponential backoff in MyRustServer. Let me fetch that session."
    ↓
Agent calls: read_session(session_id="...")
    ↓
Full transcript returned; agent incorporates patterns into response.
```

## Configuration

Memory and learning settings are configured in `config.toml`:

```toml
[learn]
# Session embedding happens asynchronously after session end.
# Set to false to disable.
embed_sessions = true

# Quiet-session threshold for nudge.
quiet_sessions_threshold = 5

# Stats database location (relative to ~/.wingman/)
stats_db = "learn.db"

# Session embeddings database location
sessions_db = "sessions.db"

[memory]
# Memory file extensions and formats
# Currently only markdown with YAML frontmatter is supported.
format = "markdown"
```

## Privacy & Local-First Design

- **No cloud uploads.** All memories and sessions stored locally.
- **No external API calls.** Embedding uses fastembed (ONNX runtime) or deterministic hash.
- **User control.** Full history available under `~/.wingman/` for inspection, deletion, or sharing.
- **Selective recall.** Agent doesn't fetch full memory bodies unless relevant (prevents token waste).
- **Opt-out per memory.** `forget_memory()` deletes any memory permanently.

## Troubleshooting

### Q: Why isn't my memory showing up?

**A:** Check `~/.wingman/memory/MEMORY.md` — it may not be indexed. Run:
```bash
wingman memory list
```

If the file exists in the directory but not in MEMORY.md, the agent may not have saved it properly. Try manually adding an entry to MEMORY.md:
```markdown
- [my-memory](my_memory.md) — description
```

### Q: How do I export my memories to share with a colleague?

**A:**
```bash
wingman memory export /tmp/my-memories.json
# Send /tmp/my-memories.json to colleague
# They run: wingman memory import /tmp/my-memories.json
```

### Q: Session embeddings take up too much space. Can I clear them?

**A:** Yes, it's safe to delete `~/.wingman/sessions.db`. It will be rebuilt on next session end. Cross-project recall will be unavailable until rebuilt.

### Q: I want to disable learning entirely. How?

**A:** In `config.toml`:
```toml
[learn]
embed_sessions = false

[memory]
enabled = false
```

Agent loop will still work; just without persistence.
