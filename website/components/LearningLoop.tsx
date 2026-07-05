import { Section, SectionHeading } from "./Section";
import { Reveal, RevealGroup, RevealItem } from "./Reveal";
import { CodeBlock } from "./CodeBlock";

const pillars = [
  {
    title: "Memories",
    body: "Markdown + frontmatter under ~/.wingman/memory/ and per-project. Four types — user, feedback, project, reference. The index is in every prompt; bodies load on demand.",
  },
  {
    title: "Skills",
    body: "Skills are created and refined from observed work. Every invoke_skill call is recorded with an outcome; when a skill drifts past a correction threshold, the next session suggests a rewrite and skill extract mines a fresh draft.",
  },
  {
    title: "Session recall",
    body: "Finished sessions are embedded and searchable across projects. Ask “how did we fix this last time?” and the agent finds the thread.",
  },
];

export function LearningLoop() {
  return (
    <Section id="learning">
      <div className="grid items-start gap-12 lg:grid-cols-[1fr_0.9fr]">
        <div>
          <SectionHeading
            eyebrow="Self-improving loop"
            title="It remembers you. And your projects."
            lead="There is no cloud component — everything is local-first. Each session contributes to a small set of files the next run reads on startup."
          />
          <RevealGroup className="mt-10 space-y-4">
            {pillars.map((p) => (
              <RevealItem key={p.title}>
                <div className="rounded-xl border border-[var(--border)] bg-[var(--surface)] p-5">
                  <h3 className="text-base font-bold text-[var(--text-strong)]">
                    {p.title}
                  </h3>
                  <p className="mt-1.5 text-sm leading-6 text-[var(--text-muted)]">
                    {p.body}
                  </p>
                </div>
              </RevealItem>
            ))}
          </RevealGroup>
        </div>

        <Reveal delay={0.1} className="lg:sticky lg:top-28">
          <CodeBlock
            title="~/.wingman/memory/prefers-pnpm.md"
            lines={[
              { text: "---", tone: "comment" },
              { text: "name: prefers-pnpm", tone: "default" },
              { text: "description: package manager preference", tone: "default" },
              { text: "type: feedback", tone: "default" },
              { text: "---", tone: "comment" },
              { text: "", tone: "default" },
              { text: "Always use pnpm over npm in this user's", tone: "out" },
              { text: "projects. They asked for it explicitly.", tone: "out" },
            ]}
          />
          <p className="mt-4 text-sm text-[var(--text-dim)]">
            Tell the agent “remember that I prefer pnpm” and it calls{" "}
            <code>save_memory</code>. The next session sees it in the system
            prompt — automatically.
          </p>
        </Reveal>
      </div>
    </Section>
  );
}
