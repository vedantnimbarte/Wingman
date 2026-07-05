"use client";

import Link from "next/link";
import { motion } from "motion/react";
import { DottedWaves } from "./DottedWaves";
import { HeroTerminal } from "./HeroTerminal";
import { ArrowIcon, GitHubIcon } from "./icons";
import { site } from "@/lib/site";

const EASE = [0.22, 1, 0.36, 1] as const;

const stats = [
  { value: "73+", label: "LLM providers" },
  { value: "20+", label: "built-in tools" },
  { value: "Rust", label: "single binary" },
];

const MotionDiv = motion.div;

export function Hero() {
  return (
    <section className="relative overflow-hidden pt-32 pb-20 sm:pt-40 sm:pb-28">
      {/* animated dotted-wave field */}
      <div className="pointer-events-none absolute inset-0 -z-0">
        <DottedWaves className="opacity-70" />
        <div className="absolute inset-0 bg-gradient-to-b from-transparent via-transparent to-[var(--bg)]" />
        <div className="absolute inset-x-0 top-0 h-40 bg-gradient-to-b from-[var(--bg)] to-transparent" />
      </div>

      <div className="container-page relative z-10 grid items-center gap-14 lg:grid-cols-[1.05fr_0.95fr]">
        <div>
          <MotionDiv
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.6, ease: EASE }}
          >
            <span className="inline-flex items-center gap-2 rounded-full border border-[var(--border)] bg-[var(--color-carbon-300)]/70 px-3 py-1 font-mono text-[0.72rem] font-medium uppercase tracking-[0.14em] text-[var(--text-muted)] backdrop-blur">
              <span className="h-1.5 w-1.5 rounded-full bg-[var(--color-french-500)]" />
              self-improving · provider-agnostic
            </span>
          </MotionDiv>

          <MotionDiv
            initial={{ opacity: 0, y: 22 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, ease: EASE, delay: 0.06 }}
          >
            <h1 className="mt-6 text-balance text-5xl font-black leading-[1.02] tracking-tight text-[var(--text-strong)] sm:text-6xl lg:text-[4.25rem]">
              The terminal-first
              <br />
              coding agent that
              <br />
              <span className="text-gradient">learns as you work.</span>
            </h1>
          </MotionDiv>

          <MotionDiv
            initial={{ opacity: 0, y: 20 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, ease: EASE, delay: 0.14 }}
          >
            <p className="mt-6 max-w-xl text-pretty text-lg leading-8 text-[var(--text-muted)]">
              <span className="font-mono text-[var(--text)]">wingman</span> talks
              to 73+ LLM providers behind one streaming interface, edits your
              project with a built-in tool layer, and builds a persistent memory
              of you and your codebase — all from the terminal, all local-first.
            </p>
          </MotionDiv>

          <MotionDiv
            initial={{ opacity: 0, y: 18 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, ease: EASE, delay: 0.22 }}
          >
            <div className="mt-8 flex flex-wrap items-center gap-3">
              <Link
                href="/docs/installation"
                className="group inline-flex items-center gap-2 rounded-xl bg-[var(--color-snow)] px-5 py-3 text-sm font-bold text-[var(--color-carbon-100)] transition-transform hover:-translate-y-0.5"
              >
                Get started
                <ArrowIcon className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
              </Link>
              <Link
                href={site.github}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-2 rounded-xl border border-[var(--border-strong)] bg-[var(--color-carbon-300)]/60 px-5 py-3 text-sm font-semibold text-[var(--text)] backdrop-blur transition-colors hover:border-[var(--color-french-500)] hover:text-[var(--text-strong)]"
              >
                <GitHubIcon className="h-4 w-4" />
                Star on GitHub
              </Link>
            </div>
          </MotionDiv>

          <MotionDiv
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.7, ease: EASE, delay: 0.3 }}
          >
            <dl className="mt-12 flex flex-wrap gap-x-10 gap-y-4">
              {stats.map((s) => (
                <div key={s.label}>
                  <dt className="text-2xl font-extrabold text-[var(--text-strong)]">
                    {s.value}
                  </dt>
                  <dd className="text-xs uppercase tracking-wider text-[var(--text-dim)]">
                    {s.label}
                  </dd>
                </div>
              ))}
            </dl>
          </MotionDiv>
        </div>

        <MotionDiv
          initial={{ opacity: 0, y: 30, scale: 0.98 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          transition={{ duration: 0.8, ease: EASE, delay: 0.2 }}
        >
          <HeroTerminal />
        </MotionDiv>
      </div>
    </section>
  );
}
