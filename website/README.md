# Wingman Website

Marketing landing page + documentation site for the
[Wingman](../README.md) CLI.

Built with **Next.js (App Router)**, **Tailwind CSS v4**, **Motion**, and
**MDX**. Dark-themed, with an animated dotted-wave background and soft scroll
reveals. Content is adapted from the project root `README.md`.

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

## Structure

```
app/
  layout.tsx          # fonts (Nunito + JetBrains Mono), Nav, Footer
  globals.css         # Tailwind v4 @theme palette + base/code styles
  page.tsx            # marketing landing
  docs/
    layout.tsx        # sidebar + content + table of contents
    page.mdx          # docs introduction (/docs)
    <slug>/page.mdx   # one MDX file per documentation page
components/            # DottedWaves, Hero, Reveal, Nav, Footer, sections…
lib/
  site.ts             # site metadata + primary nav
  nav.ts              # docs nav tree + prev/next helpers
mdx-components.tsx     # themed element map for all MDX docs
```

## Theme

The grayscale palette is expressed as CSS variables in `app/globals.css` via
Tailwind v4's `@theme`. The source palette had a duplicate `pale_slate` key
(`#ced4da` and `#adb5bd`); the `#adb5bd` scale is preserved here as
`french_grey`. Dark-theme semantic aliases (`--bg`, `--surface`, `--text`, …)
sit on top of the raw scales.
