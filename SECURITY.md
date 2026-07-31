# Security Policy

Wingman runs shell commands, spawns subprocesses from configuration, hosts MCP
servers, and handles API keys. We take reports seriously and would rather hear
about a problem early than read about it later.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting:
[**Report a vulnerability**](https://github.com/vedantnimbarte/Wingman/security/advisories/new).

If that is unavailable to you, open a public issue containing only "security
report, please provide a private channel" — with no details — and we will follow
up.

Useful reports include: the version or commit, your platform, the permission
mode in use, the relevant configuration, and the smallest set of steps that
demonstrates the issue. A proof of concept is welcome but not required.

### What to expect

| | |
|---|---|
| Acknowledgement | within 3 working days |
| Initial assessment | within 7 working days |
| Fix or mitigation plan | communicated once assessed, with an ETA |
| Disclosure | coordinated with you; credited unless you prefer otherwise |

This is a small project without a dedicated security team, so timelines are
best-effort rather than contractual. We will tell you if something is going to
take longer.

## Supported versions

Only the latest release is supported. Wingman is pre-1.0 and moves quickly;
fixes land on `main` and in the next release rather than being backported.

## Threat model

Being explicit about this saves everyone time, because several behaviours that
look alarming are intentional.

### In scope

- **Escaping the active permission mode.** Reading or writing outside the
  project root when the mode forbids it; executing a command in a mode that
  does not permit shell access.
- **Code execution without a tool call.** Anything that runs code as a
  side effect of opening a project, reading a file, or starting a language
  server.
- **Credential exposure.** API keys or OAuth tokens reaching disk in the clear,
  a log, a session transcript, an audit record, a prompt, or a network
  destination other than the configured provider.
- **Bypassing the project-config trust boundary.** A repository's
  `.wingman/config.toml` causing execution or credential redirection without an
  explicit `wingman trust` decision.
- **Writes to protected paths** (`.git/`, `.wingman/config.toml`,
  `.wingman/skills/`, `.wingman/trusted.toml`) in any mode.
- **Unauthenticated access to a listener**, e.g. `wingman pilot intake slack`.
- **Supply-chain integrity**: release artifacts, checksums, install scripts, CI.

### Out of scope

These are documented, intentional behaviours, not vulnerabilities:

- **`yolo` mode has no guardrails.** That is what it is for. (Protected paths
  are still refused, and *that* boundary holding is in scope.)
- **`run_shell` executes arbitrary commands** in `auto-edit` and `yolo`. The
  shell denylist is a convenience, not a boundary, and says so.
- **The model does something you did not want.** Wrong edits, wasted tokens, and
  bad suggestions are quality problems — file a normal issue.
- **A configured MCP server misbehaves.** MCP servers are third-party programs
  you chose to run; Wingman gates *when* they run, not what they do.
- **Attacks requiring an already-compromised machine** or another local user
  with write access to your home directory.

### Prompt injection

Content Wingman reads — web pages, files in a cloned repository, MCP tool
results and descriptions — is wrapped in an `<untrusted-content>` fence, and the
system prompt instructs the model to treat fenced content as data rather than
instructions.

**This is a mitigation, not a boundary.** No prompt-level defence is reliable
against a determined injection. The real boundaries are the permission modes,
the project-config trust gate, and protected paths. A report showing that
injected content crossed one of *those* is in scope and valuable. A report
showing only that a model followed injected instructions within the limits of
its current mode is expected behaviour — though we still want to hear about
novel techniques.

## Hardening notes for operators

- Keep the default `read-only` mode for unfamiliar code.
- Do not run `wingman trust` in a repository you did not write.
- `wingman attest` reports which egress channels are configured. Read its scope
  statement: it reflects configuration only.
- If you expose `wingman pilot intake slack`, set
  `[pilot.daemon].slack_signing_secret` (it now refuses to start without one)
  and put TLS in front of it.
