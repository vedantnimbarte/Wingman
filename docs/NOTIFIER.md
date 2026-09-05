# Desktop notifications

Status: **shipped**, off by default.

A small always-on-top window in the bottom-right corner that shows the moments
Wingman needs you — a plan waiting at the approval gate, a question the agent
would otherwise guess at, a task that failed — and lets you answer them in
place. Approve a plan, type an answer, dismiss a failure, without going to find
the terminal it came from.

It is not a notification *list*. There is no history view and no inbox to read
through: a card is on screen because something is waiting, and it leaves when
that stops being true.

```
┌────────────────────────────────────────┐
│ Plan awaiting approval — 7 tasks       │
│ Add actionable desktop notifications   │
│ est. $2.40 — touches crates/**         │
│ [ Approve ]  [ Veto ]                  │
└────────────────────────────────────────┘
```

## Running it

The popup is a separate binary, `wingman-notify`, and is not part of the default
install. Build it from a clone:

```bash
cargo build --release --manifest-path desktop/notifier/Cargo.toml
```

Put the binary next to `wingman`, then:

```bash
wingman notify
```

It exits immediately; the popup stays running with a tray icon (Quit is on its
menu — the window itself is frameless and has no taskbar entry). Starting it
twice is a no-op.

The window is hidden whenever there is nothing to show, so it is click-through
by definition. It never takes focus: a card appearing mid-keystroke must not
swallow the keystroke.

## Turning cards on

Nothing is enabled by default. With the defaults, `~/.wingman/notifications.jsonl`
is never even created.

```toml
[pilot.notifications]
# Write cards for pilot runs into the inbox the popup reads.
desktop_inbox = true

# Which severities reach the desktop. These are the shipped defaults — note
# that `progress` is "digest", so turning `desktop_inbox` on surfaces failures
# and approval gates but NOT successful completions.
escalation = ["desktop", "slack", "email"]   # failures, cost caps, aborts
decision   = ["desktop", "slack"]            # plan approval gates
progress   = "digest"                        # set to "desktop" for "run finished"
info       = "suppress"                      # set to "desktop" for "run started"

[tools]
# Seconds `ask_user` waits for an answer from the popup. 0 (the default) keeps
# the tool's original behaviour and never touches the inbox.
ask_user_desktop_timeout_secs = 120
```

## What each card does

| Card | Raised by | Buttons |
|---|---|---|
| Plan awaiting approval | the pilot approval gate | Approve / Veto |
| wingman is asking | the `ask_user` tool | the suggested answers, plus a text box |
| Task failed / N tasks failed | the orchestrator's failure watchdog | Abort run |
| Run failed / Run aborted | the same watchdog | none — the run is already over |
| Run finished | end-of-run reporting | none |
| Run started | `pilot run` | none |

Failures raised within a few seconds of each other become **one** card. The
common shape is a single broken dependency taking three tasks down and then the
run with them, which would otherwise be four cards to dismiss one at a time.
When the run itself is in the batch it wins the title and the tasks are listed
underneath it.

The abort button appears only while the run is still going. On a card that
already reports the run failing it would do nothing, so it is not offered.

Failures and gates never disappear on their own. Only plain news auto-dismisses,
after a few seconds — a card somebody owes an answer to that expires off the
screen would make this feature worse than no popup at all.

Every card can be dismissed, and the dismissal is recorded. That is what stops
it coming back the next time the popup starts.

## `ask_user`, and the order it tries things

The tool takes the first of these that is available:

1. **The popup**, when `ask_user_desktop_timeout_secs` is non-zero *and* the app
   is actually running.
2. **stdin**, when it is an interactive terminal.
3. Neither — it returns the note it always has, so the model proceeds with its
   best judgment and says so.

The popup is tried *before* stdin on purpose. The TUI holds the terminal in raw
mode under an alternate screen, so a stdin read there fights crossterm for
keystrokes and prints to a stderr you cannot see; routing to the popup fixes
that as well as the headless case.

If the timeout passes with no answer, the tool returns the same "proceed with
your best judgment" note. **A headless run is never blocked longer than this.**
And if the popup is not running, the question is not routed to it at all — you
get the terminal prompt instead of waiting out a deadline nobody will meet.

## The same cards in the web panel

`wingman serve` serves the inbox at `GET /v1/notifications`, and the panel draws
the same stack in its bottom-right corner, with the same buttons. One inbox, one
`[pilot.notifications]`, two surfaces that cannot disagree.

The panel used to raise browser notifications of its own, straight off the
`/v1/events` stream with a hard-coded filter — which is precisely how the two
came to disagree: a question from `ask_user` is not a run transition, so it
reached the popup and never reached the panel. The browser notification is still
there, because an in-page card cannot reach a backgrounded tab, but it is raised
from a card now rather than from an event.

The panel is also the only one of the two that reaches a phone. The popup is a
desktop binary; `serve` is what a phone talks to.

## How it reaches you from anywhere

A pilot run is a detached process, a worker is its child, the TUI is your
terminal, and `wingman serve` may not be running at all. There is no in-process
channel that reaches all of them, so notifications use the one thing that does:
a file.

```
~/.wingman/notifications.jsonl        any process appends; the popup reads
~/.wingman/notification-replies.jsonl the popup appends; the asker tails
~/.wingman/notifier.alive             the popup re-stamps this while running
```

This is the same shape `<run-dir>/control.jsonl` already has for live runs —
append a JSON line, tail it by byte offset — and it means the popup needs no
daemon, no port and no token.

Approvals do not travel on the reply file. The Approve button carries the
literal `ControlCommand`, so the popup appends `{"cmd":"approve"}` straight to
the run's own `control.jsonl` — the file the run is already tailing. It is the
same line `wingman pilot approve` writes.

The reasoning behind both is in
[0017](decisions/0017-notifications-are-a-file-inbox.md); why the app is not a
workspace member is [0018](decisions/0018-the-notifier-is-not-a-workspace-member.md).

## Developing it

The frontend is a second Vite project at `desktop/notifier/ui/`. Outside Tauri
it seeds itself with one card of each kind, so the popup can be designed in a
browser with no Rust rebuild:

```bash
npm --prefix desktop/notifier/ui run dev
```

Design tokens are copied from `panel/src/app.css` and held to it by
`tokens.test.ts`. The colour rule is the panel's: colour encodes epistemic
status and nothing else — `--failed` for a failure, `--asserted` for something
waiting on you, `--proven` for finished work, and the primary button is solid
ink rather than a fourth hue.

```bash
npm --prefix desktop/notifier/ui test
cargo test --manifest-path desktop/notifier/Cargo.toml
```

To run the **app** against that dev server rather than its embedded copy, build
without default features:

```bash
npm --prefix desktop/notifier/ui run dev          # must be up first, on :1421
cargo run --no-default-features --manifest-path desktop/notifier/Cargo.toml
```

That flag is the whole dev/production switch, and it is not the cargo profile.
`custom-protocol` is what makes the binary serve the assets
`generate_context!` embedded; without it the window loads `devUrl` instead, and
with no dev server listening it fills with `ERR_CONNECTION_REFUSED` — a page
that never mounts, so it never calls `resize`, so the window stays hidden and
the popup looks like it simply did nothing. It is on by default here because
`wingman notify` and CI both build with plain `cargo`, never the `tauri` CLI
that would pass the flag for them.

## Not doing

- **A retry button on failure cards.** `{"cmd":"retry_task"}` would work for
  free, but the retry ladder is already retrying and a human racing it is a new
  failure mode.
- **Autostart on login.** Three mechanisms, three uninstall stories, and a
  deleted binary auto-launching after uninstall.
- **Installers and code signing.** See
  [0018](decisions/0018-the-notifier-is-not-a-workspace-member.md).
- **`tell` as a card button.** The mechanism is free — a button carries a
  literal `ControlCommand` — but a message to a task needs a task id the card
  does not carry, and the panel's run view already has that control next to the
  task it applies to.
