/**
 * Mention candidates, fetched from the server.
 *
 * A browser cannot walk a filesystem, so `GET /api/files` answers for it —
 * inside the same jail `fs/*` uses. See SECURITY.md for what that exposes.
 *
 * The server narrows by substring and caps its answer; this refetches only
 * when narrowing further could not be done from what is already in hand, and
 * ranks the rest here. Fetching the whole tree once would be a bigger
 * unauthenticated payload, stale the moment the agent writes a file, and
 * hopeless on a real repository.
 */

import { useEffect, useRef, useState } from "react";

import { rank } from "./composer/rank";

/** One file or directory the composer can offer. */
export interface WorkspaceFile {
  root: string;
  path: string;
  relPath: string;
  name: string;
  isDir: boolean;
}

interface Answer {
  /** The query the server was asked for, which every cached answer narrows. */
  query: string;
  entries: WorkspaceFile[];
  truncated: boolean;
}

/** How much to type before asking the server. Below this, everything matches. */
const MIN_QUERY = 2;
const DEBOUNCE_MS = 120;
const LIMIT = 200;

/**
 * The candidates matching `query`, ranked. `null` root means every root.
 *
 * Pass `enabled: false` when no mention is being typed, so a composer that is
 * only being written in never touches the network.
 */
export function useWorkspaceFiles(
  root: string | null,
  query: string,
  enabled: boolean,
): WorkspaceFile[] {
  const [answer, setAnswer] = useState<Answer | null>(null);
  const inFlight = useRef<AbortController | null>(null);

  // What the server was asked for. A cached answer covers any query that
  // extends it, unless the server had to truncate — then narrowing may reveal
  // matches the cap ate, so ask again.
  const prefix = query.slice(0, MIN_QUERY);
  const usable =
    answer !== null && query.startsWith(answer.query) && !(answer.truncated && query !== answer.query);
  const ask = enabled && !usable ? prefix : null;

  useEffect(() => {
    if (ask === null) return;

    const timer = setTimeout(() => {
      inFlight.current?.abort();
      const controller = new AbortController();
      inFlight.current = controller;

      const params = new URLSearchParams({ q: ask, limit: String(LIMIT) });
      if (root !== null) params.set("root", root);

      void fetch(`/api/files?${params}`, { signal: controller.signal })
        .then((response) => (response.ok ? response.json() : Promise.reject(response.statusText)))
        .then((body: { entries: WorkspaceFile[]; truncated: boolean }) => {
          setAnswer({ query: ask, entries: body.entries, truncated: body.truncated });
        })
        .catch(() => {
          // A picker that cannot reach the server offers nothing; there is
          // nothing useful to say about it in a dropdown.
          if (!controller.signal.aborted) setAnswer({ query: ask, entries: [], truncated: false });
        });
    }, DEBOUNCE_MS);

    return () => clearTimeout(timer);
  }, [ask, root]);

  useEffect(() => () => inFlight.current?.abort(), []);

  if (!enabled || answer === null) return [];
  return rank(answer.entries, query, (file) => file.relPath);
}
