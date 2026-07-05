import { Section, SectionHeading } from "./Section";
import { RevealGroup, RevealItem } from "./Reveal";

type Feature = {
  title: string;
  body: string;
  icon: React.ReactNode;
  span?: boolean;
};

const I = (d: string[]) => (
  <svg
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.7"
    strokeLinecap="round"
    strokeLinejoin="round"
    className="h-5 w-5"
  >
    {d.map((p, i) => (
      <path key={i} d={p} />
    ))}
  </svg>
);

const features: Feature[] = [
  {
    title: "Self-improving learning loop",
    body: "Persistent memories, skill usage stats with outcome scoring, and cross-session semantic recall. Every conversation makes the next one sharper — no cloud, all local-first.",
    icon: I(["M12 2a7 7 0 0 0-4 12.7V17h8v-2.3A7 7 0 0 0 12 2Z", "M9 21h6", "M10 17v4", "M14 17v4"]),
    span: true,
  },
  {
    title: "73+ providers, one shape",
    body: "Anthropic, OpenAI, Gemini, ChatGPT (OAuth), OpenRouter, LiteLLM, Ollama, vLLM and dozens more — all behind one streaming Message contract. Guided login probes your key and stores it in the OS keyring.",
    icon: I(["M3 12h18", "M3 6h18", "M3 18h18"]),
  },
  {
    title: "Three surfaces",
    body: "A ratatui TUI for interactive coding, a headless --print mode that emits text or newline-delimited JSON, and --batch to run a JSONL file of prompts — all ready to pipe into CI.",
    icon: I(["M3 4h18v16H3z", "m7 9 3 3-3 3", "M13 15h4"]),
  },
  {
    title: "Multi-agent pilot mode",
    body: "wingman pilot run plans a goal, delegates to worker agents in isolated git worktrees, and converges their output into a single reviewable pull request.",
    icon: I([
      "M6 3v8",
      "M6 21a2 2 0 1 0 0-4 2 2 0 0 0 0 4Z",
      "M6 5a2 2 0 1 0 0-4 2 2 0 0 0 0 4Z",
      "M18 9a2 2 0 1 0 0-4 2 2 0 0 0 0 4Z",
      "M18 7c0 6-6 4-6 10",
    ]),
  },
  {
    title: "MCP host",
    body: "Declare Model Context Protocol servers under [mcp.<name>] (stdio or HTTP); their tools show up namespaced as mcp__<server>__<tool> and dispatch like built-ins. Manage them live with /mcp.",
    icon: I([
      "M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6Z",
      "M12 3v3",
      "M12 18v3",
      "M3 12h3",
      "M18 12h3",
    ]),
  },
  {
    title: "Built-in tool layer",
    body: "20+ tools — read, write, edit, apply_patch, glob, grep, list, shell, web fetch/search, semantic search and the learning tools — each gated by the active permission mode.",
    icon: I(["m14.7 6.3 3 3", "M3 21l3.5-1 11-11a2.1 2.1 0 0 0-3-3l-11 11L3 21Z"]),
  },
  {
    title: "Dynamic /model picker",
    body: "Swap provider/model mid-session with /model — the picker is scoped to the providers you've actually connected. No restart, history preserved, and fallback chains walk in order on failure.",
    icon: I(["M21 12a9 9 0 1 1-3-6.7", "M21 3v6h-6"]),
  },
  {
    title: "Token-aware pipeline",
    body: "Per-tool output budgets with head/tail truncation, history estimation, and an automatic compaction trigger so long sessions stay in-window.",
    icon: I(["M3 3v18h18", "m7 14 3-3 3 3 4-5"]),
  },
  {
    title: "Permission modes & hooks",
    body: "read-only, plan, auto-edit and yolo modes, plus pre/post tool-use, stop and prompt-submit shell hooks that can block a call.",
    icon: I(["M12 2 4 6v6c0 5 3.5 8 8 10 4.5-2 8-5 8-10V6Z", "m9 12 2 2 4-4"]),
  },
];

export function FeatureGrid() {
  return (
    <Section id="features">
      <SectionHeading
        eyebrow="Highlights"
        title="Everything a coding agent should be — and a memory on top."
        lead="Wingman pairs a fast, scriptable agent loop with a learning layer that quietly gets to know how you work."
      />
      <RevealGroup className="mt-12 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {features.map((f) => (
          <RevealItem
            key={f.title}
            className={f.span ? "sm:col-span-2 lg:col-span-1 lg:row-span-2" : ""}
          >
            <article
              className={`group h-full rounded-2xl border border-[var(--border)] bg-[var(--surface)] p-6 transition-all duration-300 hover:-translate-y-1 hover:border-[var(--border-strong)] ${
                f.span ? "lg:flex lg:flex-col lg:justify-between" : ""
              }`}
            >
              <div>
                <span className="grid h-10 w-10 place-items-center rounded-xl border border-[var(--border)] bg-[var(--color-carbon-300)] text-[var(--color-french-600)] transition-colors group-hover:text-[var(--text-strong)]">
                  {f.icon}
                </span>
                <h3 className="mt-5 text-lg font-bold text-[var(--text-strong)]">
                  {f.title}
                </h3>
                <p className="mt-2 text-sm leading-6 text-[var(--text-muted)]">
                  {f.body}
                </p>
              </div>
            </article>
          </RevealItem>
        ))}
      </RevealGroup>
    </Section>
  );
}
