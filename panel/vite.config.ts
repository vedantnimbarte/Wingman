import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { mockApi } from './mock'

// The build emits exactly three files with stable names — `index.html`,
// `app.js`, `app.css` — because `crates/wingman-cli/build.rs` embeds them by
// name with `include_bytes!`. Hashed filenames would force a new Rust
// dependency (`include_dir`/`rust-embed`) to walk an unknown tree, and
// cache-busting buys nothing for a binary that serves its own assets: the
// ETag is the build version, and a new binary is a new ETag.
//
// `inlineDynamicImports` keeps that promise even if a future route lazy-loads:
// one entry, one chunk, no vendor split that would emit a fourth file nobody
// embeds.
// `npm run dev:mock` swaps the proxy for an in-memory fake daemon (`mock.ts`),
// for UI work with no `wingman serve` running.
export default defineConfig(({ mode }) => ({
  plugins: mode === 'mock' ? [react(), mockApi()] : [react()],
  build: {
    // The font is inlined into app.css rather than emitted as a fourth file:
    // build.rs embeds exactly index.html, app.js and app.css.
    assetsInlineLimit: (file) => (file.endsWith('.woff2') ? true : undefined),
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        inlineDynamicImports: true,
        entryFileNames: 'app.js',
        assetFileNames: 'app.[ext]',
      },
    },
  },
  server: {
    // `npm run dev` serves the UI with HMR and proxies the API to a real
    // `wingman serve`, so development needs no Rust rebuild. Change this if
    // your `[serve].addr` differs.
    proxy: mode === 'mock' ? undefined : {
      '/v1': {
        target: 'http://127.0.0.1:8787',
        changeOrigin: false,
      },
    },
  },
}))
