"use client";

import { useEffect, useRef, useState } from "react";
import { TerminalWindow } from "./TerminalWindow";

type Step =
  | { kind: "type"; text: string }
  | { kind: "out"; text: string; tone?: "out" | "dim" | "ok" }
  | { kind: "pause"; ms: number };

const SCRIPT: Step[] = [
  { kind: "type", text: 'wingman --print "add a --version-only flag"' },
  { kind: "pause", ms: 400 },
  { kind: "out", text: "● planning · reading crates/wingman-cli", tone: "dim" },
  { kind: "out", text: "  ↳ recall_memory(user-prefers-terse)", tone: "dim" },
  { kind: "out", text: "✎ edit_file  src/main.rs   (+12 −1)", tone: "out" },
  { kind: "out", text: "✎ edit_file  src/args.rs   (+7  −0)", tone: "out" },
  { kind: "out", text: "✓ cargo build --release   ok", tone: "ok" },
  { kind: "out", text: "✓ done — opened PR #214", tone: "ok" },
];

const PROMPT = "~/Wingman ❯ ";

export function HeroTerminal() {
  const [typed, setTyped] = useState("");
  const [outputs, setOutputs] = useState<{ text: string; tone: string }[]>([]);
  const [typing, setTyping] = useState(true);
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  useEffect(() => {
    const reduce = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;

    if (reduce) {
      const cmd = SCRIPT.find((s) => s.kind === "type");
      if (cmd && cmd.kind === "type") setTyped(cmd.text);
      setOutputs(
        SCRIPT.filter((s): s is Extract<Step, { kind: "out" }> => s.kind === "out").map(
          (s) => ({ text: s.text, tone: s.tone ?? "out" }),
        ),
      );
      setTyping(false);
      return;
    }

    let cancelled = false;
    const run = async () => {
      const wait = (ms: number) =>
        new Promise<void>((res) => {
          const id = setTimeout(res, ms);
          timers.current.push(id);
        });

      for (const step of SCRIPT) {
        if (cancelled) return;
        if (step.kind === "type") {
          setTyping(true);
          for (let i = 1; i <= step.text.length; i++) {
            if (cancelled) return;
            setTyped(step.text.slice(0, i));
            await wait(34);
          }
          setTyping(false);
        } else if (step.kind === "pause") {
          await wait(step.ms);
        } else {
          await wait(280);
          if (cancelled) return;
          setOutputs((prev) => [
            ...prev,
            { text: step.text, tone: step.tone ?? "out" },
          ]);
        }
      }
    };

    run();
    return () => {
      cancelled = true;
      timers.current.forEach(clearTimeout);
      timers.current = [];
    };
  }, []);

  const toneColor: Record<string, string> = {
    out: "text-[var(--color-french-500)]",
    dim: "text-[var(--text-dim)]",
    ok: "text-[var(--color-platinum)]",
  };

  return (
    <TerminalWindow title="wingman — headless run">
      <div className="min-h-[15.5rem]">
        <div className="flex flex-wrap">
          <span className="text-[var(--text-dim)]">{PROMPT}</span>
          <span className="text-[var(--text-strong)]">{typed}</span>
          {typing && (
            <span className="ml-0.5 inline-block h-[1.05em] w-[0.55ch] translate-y-[2px] animate-pulse bg-[var(--color-french-500)]" />
          )}
        </div>
        <div className="mt-2 space-y-1">
          {outputs.map((o, i) => (
            <div key={i} className={toneColor[o.tone] ?? toneColor.out}>
              {o.text}
            </div>
          ))}
        </div>
      </div>
    </TerminalWindow>
  );
}
