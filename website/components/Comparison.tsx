import { Reveal } from "./Reveal";
import { Section, SectionHeading } from "./Section";

type Mark = "full" | "partial" | "none";

type Tool = {
  name: string;
  tag: string;
  highlight?: boolean;
};

type Row = {
  label: string;
  marks: Mark[];
};

// Column order is shared by the header and every row's `marks` array.
const tools: Tool[] = [
  { name: "Wingman", tag: "Autonomous · Rust", highlight: true },
  { name: "Hermes", tag: "Open-model agent" },
  { name: "Devin", tag: "Autonomous SWE" },
  { name: "Cursor", tag: "AI editor" },
  { name: "Copilot", tag: "Autocomplete" },
  { name: "Claude Code", tag: "Agentic CLI" },
];

const rows: Row[] = [
  { label: "Autonomous goal → ship loop", marks: ["full", "full", "full", "partial", "none", "partial"] },
  { label: "Dependency-aware task graph", marks: ["full", "partial", "full", "none", "none", "none"] },
  { label: "Sandboxed worktree execution", marks: ["full", "partial", "partial", "none", "none", "partial"] },
  { label: "Self-correcting test loop", marks: ["full", "full", "full", "partial", "none", "partial"] },
  { label: "Opens real pull requests", marks: ["full", "partial", "full", "partial", "none", "partial"] },
  { label: "Runs headless / unattended", marks: ["full", "full", "full", "none", "none", "partial"] },
  { label: "Single binary, no runtime", marks: ["full", "none", "none", "none", "none", "none"] },
  { label: "Model-agnostic / BYO model", marks: ["full", "full", "none", "partial", "none", "none"] },
  { label: "Self-hostable / local", marks: ["full", "full", "none", "none", "none", "partial"] },
];

// label column + one column per tool. Kept as a static literal so Tailwind's
// JIT scanner can see it (interpolated class names are not detected).
const gridCols = "grid-cols-[minmax(180px,1.5fr)_repeat(6,minmax(0,1fr))]";

const markMeta: Record<Mark, string> = {
  full: "Native",
  partial: "Limited",
  none: "Not offered",
};

function Check({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 20 20"
      className={`h-[18px] w-[18px] ${className ?? ""}`}
      fill="none"
      stroke="currentColor"
      strokeWidth={2.2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M4 10.5l4 4 8-9.5" />
    </svg>
  );
}

function PartialMark() {
  return <span className="inline-block h-[3px] w-3.5 rounded-full bg-[var(--text-dim)]" />;
}

function NoneMark() {
  return <span className="inline-block h-1.5 w-1.5 rounded-full bg-[var(--border-strong)]" />;
}

function MarkCell({ mark, highlight }: { mark: Mark; highlight?: boolean }) {
  return (
    <span className="inline-flex items-center justify-center" title={markMeta[mark]}>
      <span className="sr-only">{markMeta[mark]}</span>
      {mark === "full" && (
        <Check className={highlight ? "text-[var(--text-strong)]" : "text-[var(--text-muted)]"} />
      )}
      {mark === "partial" && <PartialMark />}
      {mark === "none" && <NoneMark />}
    </span>
  );
}

export function Comparison() {
  return (
    <Section id="compare">
      <SectionHeading
        eyebrow="Compare"
        title="How Wingman holds the line"
        lead="Most tools help you type faster. Wingman closes the loop on its own — planning, sandboxed execution, self-correcting tests, and a reviewable pull request. Here is how it measures against the field, open-model agents like Hermes included."
      />

      <Reveal className="mt-10">
        <div className="glow-ring overflow-hidden rounded-2xl border border-[var(--border)] bg-[var(--surface)]">
          <div className="overflow-x-auto">
            <div className="min-w-[720px]">
              {/* Header */}
              <div className={`grid ${gridCols} border-b border-[var(--border)]`}>
                <div className="px-5 py-5 font-mono text-[0.68rem] uppercase tracking-[0.14em] text-[var(--text-dim)]">
                  Capability
                </div>
                {tools.map((tool) => (
                  <div
                    key={tool.name}
                    className={`relative px-3 py-5 text-center ${
                      tool.highlight ? "bg-[rgba(248,249,250,0.05)]" : ""
                    }`}
                  >
                    {tool.highlight && (
                      <span
                        className="absolute inset-x-0 top-0 h-0.5 bg-[var(--color-snow)]"
                        aria-hidden
                      />
                    )}
                    <div
                      className={`text-sm font-bold tracking-tight ${
                        tool.highlight ? "text-[var(--text-strong)]" : "text-[var(--text)]"
                      }`}
                    >
                      {tool.name}
                    </div>
                    <div className="mt-1 font-mono text-[0.6rem] uppercase tracking-[0.1em] text-[var(--text-dim)]">
                      {tool.tag}
                    </div>
                  </div>
                ))}
              </div>

              {/* Rows */}
              {rows.map((row, ri) => (
                <div
                  key={row.label}
                  className={`group grid ${gridCols} ${
                    ri !== rows.length - 1 ? "border-b border-[var(--border)]" : ""
                  }`}
                >
                  <div className="px-5 py-4 text-sm font-medium text-[var(--text)] transition-colors group-hover:text-[var(--text-strong)]">
                    {row.label}
                  </div>
                  {row.marks.map((mark, ci) => (
                    <div
                      key={tools[ci].name}
                      className={`flex items-center justify-center px-3 py-4 ${
                        tools[ci].highlight
                          ? "bg-[rgba(248,249,250,0.05)]"
                          : "transition-colors group-hover:bg-[rgba(248,249,250,0.02)]"
                      }`}
                    >
                      <MarkCell mark={mark} highlight={tools[ci].highlight} />
                    </div>
                  ))}
                </div>
              ))}
            </div>
          </div>

          {/* Legend */}
          <div className="flex flex-wrap items-center gap-x-6 gap-y-2 border-t border-[var(--border)] bg-[var(--bg-soft)] px-5 py-4 font-mono text-[0.68rem] uppercase tracking-[0.12em] text-[var(--text-dim)]">
            <span className="inline-flex items-center gap-2">
              <Check className="text-[var(--text-strong)]" />
              Native
            </span>
            <span className="inline-flex items-center gap-2">
              <PartialMark />
              Limited
            </span>
            <span className="inline-flex items-center gap-2">
              <NoneMark />
              Not offered
            </span>
            <span className="ml-auto normal-case tracking-normal text-[var(--text-dim)]">
              Reflects typical configurations and may evolve.
            </span>
          </div>
        </div>
      </Reveal>
    </Section>
  );
}
