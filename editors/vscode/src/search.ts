// Parsing for `semantic_search` results.
//
// Kept free of any `vscode` import so it can be unit-tested under plain node
// (see search.test.ts) — the extension host is not available there.

/** One ranked hit from `semantic_search`. */
export interface SearchHit {
  /** Path as the tool reported it: repo-relative, or absolute. */
  path: string;
  /** 1-based first line of the chunk. */
  startLine: number;
  /** Symbol the chunk belongs to, when the indexer knew one. */
  symbol?: string;
  /** Similarity score, verbatim, for display only. */
  score?: string;
  /** The code body under the header. */
  snippet: string;
}

/**
 * `semantic_search` returns ranked blocks of the form
 *
 *     [1] crates/wingman-core/src/agent.rs:114-128  (score 0.030)  [fn:dispatch]
 *     <code lines>
 *
 * Rendering that straight into a scratch buffer threw away the only thing an
 * editor can act on — the location. Parsing it back gives us something to
 * jump to.
 */
export function parseHits(text: string): SearchHit[] {
  const header =
    /^\[\d+\]\s+(.+?):(\d+)-\d+\s+\(score\s+([\d.]+)\)(?:\s+\[[a-z]+:(.+?)\])?[ \t]*$/gm;

  const found: Array<{ start: number; end: number; m: RegExpExecArray }> = [];
  for (let m = header.exec(text); m; m = header.exec(text)) {
    found.push({ start: m.index, end: header.lastIndex, m });
  }

  // A hit's snippet runs from the end of its header to the start of the next.
  return found.map((f, i) => ({
    path: f.m[1],
    startLine: Number(f.m[2]),
    score: f.m[3],
    symbol: f.m[4],
    snippet: text.slice(f.end, i + 1 < found.length ? found[i + 1].start : text.length).trim(),
  }));
}
