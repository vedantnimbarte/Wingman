# Wingman — Coming Soon

A standalone "coming soon" / pre-launch landing page for
[Wingman](../README.md), built with the **same stack and theme** as the main
[`website`](../website) (Next.js App Router, Tailwind CSS v4, Motion).

It surfaces the full Wingman story — learning loop, 73+ providers, pilot mode,
two surfaces, tooling — framed as a pre-launch announcement, with a waitlist
email capture and a launch roadmap.

## Develop

```bash
npm install
npm run dev      # http://localhost:3000
```

## Build

```bash
npm run build
npm run start
```

## Notes

- **Theme**: identical grayscale palette + semantic tokens as `website`
  (`app/globals.css`), with a few coming-soon flourishes (pulsing live dot,
  blinking caret, animated text sheen).
- **Fonts**: Lora (headings), Nunito (body), JetBrains Mono (code), wired via
  `next/font` to the CSS variables the theme expects.
- **Email capture is visual-only** — `components/NotifyForm.tsx` validates the
  address and shows a success state but does **not** send or store anything.
  Wire a provider (Resend, ConvertKit, a Next.js route handler, …) into its
  `onSubmit` when you're ready to collect for real.
- No countdown timer (no launch date set yet).
