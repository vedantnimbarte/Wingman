# Wingman for VS Code

Brings Wingman's warm repo index, team memory, and code intelligence into
VS Code by talking to `wingman mcp-serve` over MCP (JSON-RPC/stdio). The
extension is a thin client — all the intelligence lives in the Rust core, so the
same capabilities are available in the editor as on the CLI.

## Commands

- **Wingman: Semantic Search** — query the warm repo index (`semantic_search`).
- **Wingman: Recall Memory** — search team/project memory (`recall_memory`).
- **Wingman: Restart Server** — restart the underlying `wingman mcp-serve`.

## Requirements

- The `wingman` binary on your `PATH` (or set `wingman.binaryPath`).
- A project that Wingman has indexed (open a workspace folder).

## Build / run

```bash
cd editors/vscode
npm install
npm run build          # bundles src/extension.ts -> dist/extension.js
# then press F5 in VS Code (Extension Development Host), or package with vsce:
npx @vscode/vsce package
```

## How it works

On first command use, the extension spawns `wingman mcp-serve` in the workspace
folder and speaks MCP over its stdio: `initialize`, then `tools/call` for
`semantic_search` / `recall_memory`. Because it uses the stable MCP surface, the
same client works against any Wingman version. Read-only by default (the server
defaults to read-only permission), so the editor integration can't mutate your
tree unless you launch the server with a higher `--mode`.

This lives in its own npm/TypeScript toolchain, separate from the Rust
workspace, and ships through the VS Code Marketplace rather than the Rust
release pipeline.
