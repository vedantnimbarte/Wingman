import { Logo } from "@/components/Logo";
import { GitHubIcon, ArrowIcon } from "@/components/icons";
import { Reveal, RevealGroup, RevealItem } from "@/components/Reveal";
import { NotifyForm } from "@/components/NotifyForm";
import { site } from "@/lib/site";

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

const highlights = [
  {
    title: "Self-improving learning loop",
    body: "Persistent memories, outcome-scored skills, and cross-session recall. Every conversation sharpens the next — local-first, no cloud.",
    icon: I(["M12 2a7 7 0 0 0-4 12.7V17h8v-2.3A7 7 0 0 0 12 2Z", "M9 21h6", "M10 17v4", "M14 17v4"]),
  },
  {
    title: "73+ providers, one shape",
    body: "Anthropic, OpenAI, Gemini, OpenRouter, Ollama, vLLM and dozens more — all behind a single streaming Message contract.",
    icon: I(["M3 12h18", "M3 6h18", "M3 18h18"]),
  },
  {
    title: "Autonomous pilot mode",
    body: "Hand it a goal. It plans tasks, spawns workers in isolated worktrees, verifies, and converges to one reviewable pull request.",
    icon: I(["M12 2 4 6v6c0 5 3.5 8 8 10 4.5-2 8-5 8-10V6Z", "m9 12 2 2 4-4"]),
  },
  {
    title: "Two surfaces",
    body: "A ratatui TUI for interactive coding and a headless --print mode emitting text or JSON, ready to pipe straight into CI.",
    icon: I(["M3 4h18v16H3z", "m7 9 3 3-3 3", "M13 15h4"]),
  },
  {
    title: "Built-in tool layer",
    body: "Read, write, edit, glob, grep, shell, and semantic search — each gated by the active permission mode.",
    icon: I(["m14.7 6.3 3 3", "M3 21l3.5-1 11-11a2.1 2.1 0 0 0-3-3l-11 11L3 21Z"]),
  },
  {
    title: "Rust-native & fast",
    body: "A single static binary with no runtime to install. Live model swap mid-session, token-aware compaction, scriptable to the core.",
    icon: I(["M13 2 3 14h7l-1 8 10-12h-7l1-8Z"]),
  },
];

const runway = [
  {
    phase: "01",
    title: "Private beta",
    body: "Invite-only TUI builds for early testers shaping the agent loop.",
    state: "In progress",
  },
  {
    phase: "02",
    title: "Public CLI",
    body: "Single-binary release for macOS, Linux & Windows. Install and go.",
    state: "Next",
  },
  {
    phase: "03",
    title: "Pilot, generally available",
    body: "Autonomous multi-task pilot mode with critic, verification & PR automation.",
    state: "Soon",
  },
];

function Eyebrow({ children }: { children: React.ReactNode }) {
  return (
    <span className="inline-flex items-center gap-2 rounded-full border border-[var(--border)] bg-[var(--color-carbon-300)] px-3 py-1 font-mono text-[0.7rem] font-medium uppercase tracking-[0.14em] text-[var(--text-muted)]">
      <span className="live-dot h-1.5 w-1.5 rounded-full bg-[var(--text-strong)]" />
      {children}
    </span>
  );
}

export default function ComingSoonPage() {
  return (
    <>
      {/* Top bar */}
      <header className="container-page flex h-16 items-center justify-between gap-4">
        <div className="flex items-center gap-2.5">
          <Logo className="h-7 w-7" />
          <span className="font-mono text-sm font-bold tracking-tight text-[var(--text-strong)]">
            {site.name}
          </span>
          <span className="ml-1 hidden items-center gap-1.5 rounded-full border border-[var(--border)] bg-[var(--surface)] px-2.5 py-0.5 font-mono text-[0.62rem] uppercase tracking-wider text-[var(--text-muted)] sm:inline-flex">
            <span className="live-dot h-1.5 w-1.5 rounded-full bg-[var(--text-strong)]" />
            {site.status}
          </span>
        </div>
        <a
          href={site.github}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-2 rounded-lg border border-[var(--border)] bg-[var(--surface)] px-3 py-2 text-sm font-medium text-[var(--text)] transition-colors hover:border-[var(--border-strong)] hover:text-[var(--text-strong)]"
        >
          <GitHubIcon className="h-4 w-4" />
          <span className="hidden sm:inline">Star on GitHub</span>
        </a>
      </header>

      {/* Hero */}
      <section className="container-page relative grid items-center gap-14 py-16 sm:py-24 lg:grid-cols-[1.05fr_0.95fr] lg:gap-10 lg:py-28">
        {/* soft radial glow behind hero copy */}
        <div
          aria-hidden
          className="pointer-events-none absolute -left-32 top-1/4 -z-[0] h-[28rem] w-[28rem] rounded-full opacity-[0.06] blur-3xl"
          style={{ background: "radial-gradient(circle, #f8f9fa 0%, transparent 70%)" }}
        />

        <div>
          <Reveal>
            <Eyebrow>Launching soon · v0.1</Eyebrow>
          </Reveal>

          <Reveal delay={0.06}>
            <h1 className="mt-6 text-balance text-4xl font-extrabold leading-[1.05] tracking-tight text-[var(--text-strong)] sm:text-5xl lg:text-6xl">
              The coding agent that gets{" "}
              <span className="sheen-text">sharper</span> every session.
            </h1>
          </Reveal>

          <Reveal delay={0.12}>
            <p className="mt-6 max-w-xl text-pretty text-base leading-7 text-[var(--text-muted)] sm:text-lg">
              {site.name} is a Rust-native, terminal-first AI coding agent with a
              self-improving learning loop, 73+ model providers, and an
              autonomous pilot mode. We&rsquo;re putting on the finishing touches
              — leave your email and you&rsquo;ll be first through the door.
            </p>
          </Reveal>

          <Reveal delay={0.18}>
            <div className="mt-8 max-w-md">
              <NotifyForm />
            </div>
          </Reveal>

          <Reveal delay={0.24}>
            <div className="mt-7 flex flex-wrap items-center gap-x-6 gap-y-2 font-mono text-xs text-[var(--text-dim)]">
              <span>{site.license}</span>
              <span className="h-1 w-1 rounded-full bg-[var(--border-strong)]" />
              <span>Rust · ratatui · 73+ providers</span>
              <span className="h-1 w-1 rounded-full bg-[var(--border-strong)]" />
              <a
                href={site.github}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 transition-colors hover:text-[var(--text-strong)]"
              >
                Follow development
                <ArrowIcon className="h-3.5 w-3.5" />
              </a>
            </div>
          </Reveal>
        </div>

        {/* Terminal preview */}
        <Reveal delay={0.16} y={32}>
          <div className="glow-ring overflow-hidden rounded-2xl border border-[var(--border)] bg-[var(--bg-soft)]">
            <div className="flex items-center gap-2 border-b border-[var(--border)] bg-[var(--surface)] px-4 py-3">
              <span className="h-3 w-3 rounded-full bg-[var(--color-iron-500)]" />
              <span className="h-3 w-3 rounded-full bg-[var(--color-iron-400)]" />
              <span className="h-3 w-3 rounded-full bg-[var(--color-iron-300)]" />
              <span className="ml-2 font-mono text-xs text-[var(--text-dim)]">
                {site.command} — zsh
              </span>
            </div>
            <div className="space-y-2 p-5 font-mono text-[0.82rem] leading-6">
              <p className="text-[var(--text-dim)]">
                <span className="text-[var(--text-strong)]">$</span> {site.command} pilot
                run &quot;add OAuth login + tests&quot;
              </p>
              <p className="text-[var(--text-muted)]">
                <span className="text-[var(--text-strong)]">◇</span> planning ·
                decomposed into 5 tasks
              </p>
              <p className="text-[var(--text-muted)]">
                <span className="text-[var(--text-strong)]">◇</span> 3 workers ·
                isolated worktrees
              </p>
              <p className="text-[var(--text-muted)]">
                <span className="text-[var(--text-strong)]">◇</span> tests green ·
                self-healed 2 retries
              </p>
              <p className="text-[var(--text-muted)]">
                <span className="text-[var(--text-strong)]">◇</span> reviewer ·
                converged to one PR
              </p>
              <p className="text-[var(--text)]">
                <span className="text-[var(--text-strong)]">✓</span> opened{" "}
                <span className="text-[var(--text-strong)]">#128</span> — ready for
                review
              </p>
              <p className="pt-1 text-[var(--text-dim)]">
                <span className="text-[var(--text-strong)]">$</span>{" "}
                <span className="caret" />
              </p>
            </div>
          </div>
        </Reveal>
      </section>

      {/* What's coming */}
      <section className="container-page py-16 sm:py-20">
        <Reveal className="max-w-2xl">
          <Eyebrow>What you&rsquo;ll get</Eyebrow>
          <h2 className="mt-5 text-balance text-3xl font-extrabold tracking-tight text-[var(--text-strong)] sm:text-4xl">
            A full coding agent — with a memory on top.
          </h2>
          <p className="mt-4 text-pretty text-base leading-7 text-[var(--text-muted)] sm:text-lg">
            Everything below ships in the first public release. A peek at what
            you&rsquo;re signing up for.
          </p>
        </Reveal>

        <RevealGroup className="mt-12 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {highlights.map((f) => (
            <RevealItem key={f.title}>
              <article className="group h-full rounded-2xl border border-[var(--border)] bg-[var(--surface)] p-6 transition-all duration-300 hover:-translate-y-1 hover:border-[var(--border-strong)]">
                <span className="grid h-10 w-10 place-items-center rounded-xl border border-[var(--border)] bg-[var(--color-carbon-300)] text-[var(--color-french-600)] transition-colors group-hover:text-[var(--text-strong)]">
                  {f.icon}
                </span>
                <h3 className="mt-5 text-lg font-bold text-[var(--text-strong)]">
                  {f.title}
                </h3>
                <p className="mt-2 text-sm leading-6 text-[var(--text-muted)]">
                  {f.body}
                </p>
              </article>
            </RevealItem>
          ))}
        </RevealGroup>
      </section>

      {/* On the runway */}
      <section className="container-page py-16 sm:py-20">
        <Reveal className="max-w-2xl">
          <Eyebrow>On the runway</Eyebrow>
          <h2 className="mt-5 text-balance text-3xl font-extrabold tracking-tight text-[var(--text-strong)] sm:text-4xl">
            How we get to launch.
          </h2>
        </Reveal>

        <RevealGroup className="mt-12 grid gap-4 md:grid-cols-3">
          {runway.map((r) => (
            <RevealItem key={r.phase}>
              <article className="relative h-full overflow-hidden rounded-2xl border border-[var(--border)] bg-[var(--surface)] p-6">
                <div className="flex items-center justify-between">
                  <span className="font-mono text-3xl font-bold text-[var(--color-iron-500)]">
                    {r.phase}
                  </span>
                  <span className="rounded-full border border-[var(--border-strong)] bg-[var(--bg-soft)] px-2.5 py-0.5 font-mono text-[0.6rem] uppercase tracking-wider text-[var(--text-muted)]">
                    {r.state}
                  </span>
                </div>
                <h3 className="mt-4 text-lg font-bold text-[var(--text-strong)]">
                  {r.title}
                </h3>
                <p className="mt-2 text-sm leading-6 text-[var(--text-muted)]">
                  {r.body}
                </p>
              </article>
            </RevealItem>
          ))}
        </RevealGroup>

        {/* Closing CTA */}
        <Reveal className="mt-12">
          <div className="glow-ring flex flex-col items-start justify-between gap-6 rounded-2xl border border-[var(--border)] bg-[var(--bg-soft)] p-8 sm:flex-row sm:items-center sm:p-10">
            <div>
              <h3 className="text-2xl font-extrabold tracking-tight text-[var(--text-strong)]">
                Be there on day one.
              </h3>
              <p className="mt-2 max-w-md text-sm leading-6 text-[var(--text-muted)]">
                Drop your email and we&rsquo;ll send a single note the moment
                Wingman is live.
              </p>
            </div>
            <div className="w-full max-w-sm sm:shrink-0">
              <NotifyForm />
            </div>
          </div>
        </Reveal>
      </section>

      {/* Footer */}
      <footer className="border-t border-[var(--border)] bg-[var(--bg-soft)]">
        <div className="container-page flex flex-col items-center justify-between gap-4 py-8 sm:flex-row">
          <div className="flex items-center gap-2.5">
            <Logo className="h-6 w-6" />
            <span className="font-mono text-xs font-bold tracking-tight text-[var(--text-strong)]">
              {site.name}
            </span>
            <span className="font-mono text-xs text-[var(--text-dim)]">
              — {site.tagline}
            </span>
          </div>
          <p className="font-mono text-xs text-[var(--text-dim)]">
            © 2026 {site.name}. {site.license}.
          </p>
        </div>
      </footer>
    </>
  );
}
