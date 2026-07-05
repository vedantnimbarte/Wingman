import { Section, SectionHeading } from "./Section";
import { Reveal, RevealGroup, RevealItem } from "./Reveal";

const tiers = [
  {
    name: "assist",
    tagline: "You approve every decision.",
    body: "Agent plans, you confirm, agent executes one run, opens a PR, exits. No daemon, no critic, no learning.",
    featured: false,
  },
  {
    name: "copilot",
    tagline: "Agent flies; you monitor.",
    body: "The default. Trust-tiered approval, self-healing retries, per-task reviewer, real verification, PR automation, and cross-run learning.",
    featured: true,
  },
  {
    name: "autopilot",
    tagline: "Agent flies and navigates.",
    body: "Daemon mode, multi-channel intake, a critic agent, knowledge graph, tool synthesis, and sandboxed execution.",
    featured: false,
  },
];

const flow = [
  { step: "Plan", detail: "decompose into a validated task DAG (tasks.jsonl)" },
  { step: "Workers", detail: "specialised agents in isolated git worktrees" },
  { step: "Converge", detail: "squash-merge + reviewer + verification" },
  { step: "PR", detail: "open a single pull request via gh" },
];

export function PilotTiers() {
  return (
    <Section id="pilot">
      <SectionHeading
        eyebrow="Pilot mode"
        title="From single goal to a reviewed pull request."
        lead="wingman pilot run plans a multi-task piece of work, spawns specialised worker agents in isolated worktrees, and converges their output into one PR. Pick a capability tier in config."
      />

      <RevealGroup className="mt-12 grid gap-4 md:grid-cols-3">
        {tiers.map((t) => (
          <RevealItem key={t.name}>
            <article
              className={`relative h-full overflow-hidden rounded-2xl border p-6 transition-all duration-300 hover:-translate-y-1 ${
                t.featured
                  ? "border-[var(--color-french-500)]/50 bg-[var(--surface)]"
                  : "border-[var(--border)] bg-[var(--surface)]"
              }`}
            >
              {t.featured && (
                <span className="absolute right-5 top-6 rounded-full border border-[var(--border-strong)] bg-[var(--color-carbon-300)] px-2.5 py-0.5 font-mono text-[0.62rem] uppercase tracking-wider text-[var(--text-muted)]">
                  default
                </span>
              )}
              <h3 className="font-mono text-lg font-bold text-[var(--text-strong)]">
                {t.name}
              </h3>
              <p className="mt-1 text-sm font-semibold text-[var(--color-french-500)]">
                {t.tagline}
              </p>
              <p className="mt-3 text-sm leading-6 text-[var(--text-muted)]">
                {t.body}
              </p>
            </article>
          </RevealItem>
        ))}
      </RevealGroup>

      <Reveal className="mt-6" delay={0.1}>
        <div className="rounded-2xl border border-[var(--border)] bg-[var(--color-carbon-200)] p-6 sm:p-8">
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            {flow.map((f, i) => (
              <div key={f.step} className="relative">
                <div className="flex items-center gap-3">
                  <span className="grid h-8 w-8 shrink-0 place-items-center rounded-lg border border-[var(--border-strong)] bg-[var(--color-carbon-400)] font-mono text-sm font-bold text-[var(--text-strong)]">
                    {i + 1}
                  </span>
                  <span className="font-semibold text-[var(--text-strong)]">
                    {f.step}
                  </span>
                </div>
                <p className="mt-2 pl-11 text-sm leading-6 text-[var(--text-muted)]">
                  {f.detail}
                </p>
              </div>
            ))}
          </div>
        </div>
      </Reveal>
    </Section>
  );
}
