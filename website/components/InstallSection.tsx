import Link from "next/link";
import { Section } from "./Section";
import { Reveal } from "./Reveal";
import { CodeBlock } from "./CodeBlock";
import { ArrowIcon } from "./icons";

const steps = [
  {
    n: "01",
    title: "Build from source",
    block: {
      title: "shell",
      lines: [
        { text: "git clone https://github.com/vedantnimbarte/Wingman.git", tone: "prompt" as const },
        { text: "cd Wingman", tone: "prompt" as const },
        { text: "cargo install --path crates/wingman-cli", tone: "prompt" as const },
      ],
    },
  },
  {
    n: "02",
    title: "Scaffold config & set a key",
    block: {
      title: "shell",
      lines: [
        { text: "wingman config init", tone: "prompt" as const },
        { text: "export ANTHROPIC_API_KEY=sk-ant-...", tone: "prompt" as const },
      ],
    },
  },
  {
    n: "03",
    title: "Run it",
    block: {
      title: "shell",
      lines: [
        { text: "# interactive TUI", tone: "comment" as const },
        { text: "wingman", tone: "prompt" as const },
        { text: "# or headless one-shot", tone: "comment" as const },
        { text: 'wingman --print "explain the agent loop"', tone: "prompt" as const },
      ],
    },
  },
];

export function InstallSection() {
  return (
    <Section id="install">
      <Reveal>
        <div className="relative overflow-hidden rounded-3xl border border-[var(--border)] bg-[var(--color-carbon-200)] p-8 sm:p-12">
          <div className="grid gap-10 lg:grid-cols-[0.8fr_1.2fr]">
            <div>
              <span className="inline-flex items-center gap-2 rounded-full border border-[var(--border)] bg-[var(--color-carbon-300)] px-3 py-1 font-mono text-[0.7rem] uppercase tracking-[0.14em] text-[var(--text-muted)]">
                <span className="h-1.5 w-1.5 rounded-full bg-[var(--color-french-500)]" />
                Quick start
              </span>
              <h2 className="mt-5 text-3xl font-extrabold tracking-tight text-[var(--text-strong)] sm:text-4xl">
                Up and running in three commands.
              </h2>
              <p className="mt-4 text-base leading-7 text-[var(--text-muted)]">
                Requires Rust 1.80+. The binary lands at{" "}
                <code>target/release/wingman</code>. For local providers like
                Ollama or LM Studio, no API key is needed.
              </p>
              <div className="mt-7 flex flex-wrap gap-3">
                <Link
                  href="/docs/installation"
                  className="group inline-flex items-center gap-2 rounded-xl bg-[var(--color-snow)] px-5 py-3 text-sm font-bold text-[var(--color-carbon-100)] transition-transform hover:-translate-y-0.5"
                >
                  Read the docs
                  <ArrowIcon className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
                </Link>
                <Link
                  href="/docs/quick-start"
                  className="inline-flex items-center gap-2 rounded-xl border border-[var(--border-strong)] px-5 py-3 text-sm font-semibold text-[var(--text)] transition-colors hover:text-[var(--text-strong)]"
                >
                  Quick start guide
                </Link>
              </div>
            </div>

            <div className="space-y-4">
              {steps.map((s) => (
                <div key={s.n} className="flex gap-4">
                  <span className="mt-1 font-mono text-sm font-bold text-[var(--text-dim)]">
                    {s.n}
                  </span>
                  <div className="min-w-0 flex-1">
                    <h3 className="mb-2 text-sm font-semibold text-[var(--text)]">
                      {s.title}
                    </h3>
                    <CodeBlock title={s.block.title} lines={s.block.lines} />
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </Reveal>
    </Section>
  );
}
