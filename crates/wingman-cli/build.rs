//! Stage the web UI bundle into `OUT_DIR` so `serve::ui` can `include_bytes!`
//! it by name.
//!
//! The panel lives in `panel/` and is built by npm, not cargo. Embedding it
//! with a bare `include_bytes!("../../panel/dist/app.js")` would make every
//! fresh clone, every contributor without node, and `cargo install wingman` fail to
//! compile — so this copies the built bundle when it exists and substitutes a
//! placeholder when it does not. `cargo build` never depends on npm.
//!
//! The trade-off that buys: a binary built without the bundle still runs, and
//! serves a page saying the UI was not built. That is a real failure mode to
//! ship, so `ci.yml` asserts the real bundle is embedded on release rather
//! than trusting the build to notice.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Files `serve::ui` embeds, with what to fall back to. Vite is configured to
/// emit exactly these names (`panel/vite.config.ts`).
const FILES: [(&str, &str); 3] = [
    ("index.html", PLACEHOLDER_HTML),
    ("app.js", ""),
    ("app.css", ""),
];

/// Served when the bundle is absent. Says what happened and what to do, in the
/// interface's voice — a blank page would leave someone guessing whether the
/// daemon was even up.
const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Wingman</title>
<style>
  :root { color-scheme: light dark; }
  body { margin:0; min-height:100vh; display:grid; place-items:center;
         font:16px/1.5 ui-sans-serif, system-ui, sans-serif; padding:1rem; }
  main { max-width:32rem; }
  h1 { font-size:1.5rem; margin:0 0 .25rem; }
  p { margin:0 0 1rem; opacity:.7; font-size:.875rem; }
  code { font-family:ui-monospace, Menlo, Consolas, monospace; font-size:.8125rem; }
  pre { padding:.75rem; border-radius:6px; overflow-x:auto;
        background:color-mix(in srgb, currentColor 8%, transparent); }
</style>
</head>
<body>
<main>
  <h1>The web UI was not built</h1>
  <p>This binary was compiled without the panel bundle. The API is unaffected &mdash;
     every <code>/v1</code> route works normally.</p>
  <pre><code>cd ui &amp;&amp; npm install &amp;&amp; npm run build
cargo build --release</code></pre>
  <p>See <code>docs/WEB-UI.md</code>.</p>
</main>
</body>
</html>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // Walk up to the workspace root rather than joining `../../` — same path,
    // but it reads as one in `cargo build -v` output.
    //
    // Rerun detection is by **mtime**, not content: cargo reruns this script
    // when a watched path is newer than the last run. `npm run build` rewrites
    // all three files, so the normal loop works. Restoring an old `dist/` with
    // a file copy that preserves timestamps will not trigger a rerun; rebuild
    // the bundle (or `touch` it) rather than assuming the embed is stale.
    let root = manifest
        .ancestors()
        .nth(2)
        .expect("crates/wingman-cli is two levels below the workspace root");
    let dist = root.join("panel").join("dist");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("panel");

    fs::create_dir_all(&out).expect("creating OUT_DIR/panel");

    // Watch the directory as well as the files: when `dist/` does not exist,
    // watching only the files inside it would leave nothing to notice its
    // arrival.
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut embedded = true;
    for (name, fallback) in FILES {
        let src = dist.join(name);
        // Cargo reruns when a watched path appears, changes, or disappears, so
        // building the bundle after the binary still picks it up.
        println!("cargo:rerun-if-changed={}", src.display());

        let body = match fs::read(&src) {
            Ok(bytes) => bytes,
            Err(_) => {
                embedded = false;
                fallback.as_bytes().to_vec()
            }
        };
        fs::write(out.join(name), body).expect("staging panel asset");
    }

    // Readable by `serve::ui` so the daemon can say which it is, and by CI so
    // a release that silently shipped the placeholder fails loudly.
    println!("cargo:rustc-env=WINGMAN_UI_EMBEDDED={embedded}");
}
