import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Deliberately not `panel/vite.config.ts` with a second entry: that config
// pins `app.js`/`app.css` and sets `inlineDynamicImports`, because
// `crates/wingman-cli/build.rs` embeds those three files by name. It is
// single-entry by construction. Tauri takes a directory instead, so hashing is
// fine and there is nothing to pin.
export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist', emptyOutDir: true },
  // Fixed port because `tauri.conf.json`'s `devUrl` has to name it.
  server: { port: 1421, strictPort: true },
})
