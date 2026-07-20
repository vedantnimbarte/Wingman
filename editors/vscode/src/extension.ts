// Wingman VS Code extension.
//
// Bridges the editor to `wingman mcp-serve`: it spawns the wingman binary as an
// MCP server (JSON-RPC over stdio, newline-delimited) and exposes Wingman's
// tools — most usefully `semantic_search` (the warm repo index) and
// `recall_memory` (team memory) — as editor commands. This is a thin client;
// all the intelligence lives in the Rust core.

import * as vscode from "vscode";
import { ChildProcessWithoutNullStreams, spawn } from "child_process";

/** Minimal MCP-over-stdio client for a single spawned server process. */
class McpClient {
  private proc: ChildProcessWithoutNullStreams | undefined;
  private nextId = 1;
  private pending = new Map<number, (result: any, error?: any) => void>();
  private buffer = "";

  constructor(private binaryPath: string, private cwd: string) {}

  start(): void {
    this.proc = spawn(this.binaryPath, ["mcp-serve"], { cwd: this.cwd });
    this.proc.stdout.setEncoding("utf8");
    this.proc.stdout.on("data", (chunk: string) => this.onData(chunk));
    this.proc.on("exit", () => {
      // Reject any in-flight requests so callers don't hang.
      for (const [, cb] of this.pending) cb(undefined, new Error("wingman mcp-serve exited"));
      this.pending.clear();
      this.proc = undefined;
    });
    // MCP handshake (fire-and-forget; the server also works without it here).
    void this.request("initialize", { protocolVersion: "2024-11-05" });
  }

  stop(): void {
    this.proc?.kill();
    this.proc = undefined;
  }

  private onData(chunk: string): void {
    this.buffer += chunk;
    let idx: number;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      try {
        const msg = JSON.parse(line);
        if (typeof msg.id === "number" && this.pending.has(msg.id)) {
          const cb = this.pending.get(msg.id)!;
          this.pending.delete(msg.id);
          cb(msg.result, msg.error);
        }
      } catch {
        // ignore non-JSON lines
      }
    }
  }

  request(method: string, params: unknown): Promise<any> {
    if (!this.proc) this.start();
    const id = this.nextId++;
    const payload = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      this.pending.set(id, (result, error) => (error ? reject(error) : resolve(result)));
      this.proc!.stdin.write(payload);
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`wingman ${method} timed out`));
        }
      }, 30000);
    });
  }

  /** Call an MCP tool and return its concatenated text content. */
  async callTool(name: string, args: Record<string, unknown>): Promise<string> {
    const res = await this.request("tools/call", { name, arguments: args });
    const parts = (res?.content ?? []) as Array<{ type: string; text?: string }>;
    return parts
      .filter((p) => p.type === "text")
      .map((p) => p.text ?? "")
      .join("\n");
  }
}

let client: McpClient | undefined;

function getClient(): McpClient {
  const cfg = vscode.workspace.getConfiguration("wingman");
  const bin = cfg.get<string>("binaryPath", "wingman");
  const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd();
  if (!client) {
    client = new McpClient(bin, cwd);
    client.start();
  }
  return client;
}

async function showToolResult(title: string, text: string): Promise<void> {
  const doc = await vscode.workspace.openTextDocument({ content: text || "(no results)", language: "markdown" });
  await vscode.window.showTextDocument(doc, { preview: true });
  void vscode.window.setStatusBarMessage(title, 3000);
}

export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("wingman.semanticSearch", async () => {
      const query = await vscode.window.showInputBox({ prompt: "Wingman semantic search" });
      if (!query) return;
      try {
        const text = await getClient().callTool("semantic_search", { query });
        await showToolResult("Wingman: search complete", text);
      } catch (e) {
        void vscode.window.showErrorMessage(`Wingman: ${e}`);
      }
    }),

    vscode.commands.registerCommand("wingman.recallMemory", async () => {
      const query = await vscode.window.showInputBox({ prompt: "Wingman recall memory" });
      if (!query) return;
      try {
        const text = await getClient().callTool("recall_memory", { query });
        await showToolResult("Wingman: recall complete", text);
      } catch (e) {
        void vscode.window.showErrorMessage(`Wingman: ${e}`);
      }
    }),

    vscode.commands.registerCommand("wingman.restart", () => {
      client?.stop();
      client = undefined;
      getClient();
      void vscode.window.showInformationMessage("Wingman server restarted.");
    })
  );
}

export function deactivate(): void {
  client?.stop();
  client = undefined;
}
