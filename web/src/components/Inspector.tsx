/**
 * The protocol inspector — the thing this project is named for.
 *
 * Every JSON-RPC frame, both directions, including the `fs/*` and `terminal/*`
 * traffic the server answered on our behalf and we would otherwise never see.
 * Modelled on Zed's `acp_tools` (`reference/zed-acp/acp_tools/src/acp_tools.rs`).
 */

import { useMemo, useState } from "react";
import type { InspectorEntry } from "../acp/types";

export function Inspector({ frames }: { frames: InspectorEntry[] }) {
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<number>();

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return frames;
    return frames.filter(
      (frame) =>
        frame.method?.toLowerCase().includes(needle) ||
        frame.line.toLowerCase().includes(needle),
    );
  }, [frames, filter]);

  const detail = frames.find((frame) => frame.seq === selected);

  return (
    <section className="inspector">
      <header className="inspector__header">
        <h2>Protocol</h2>
        <input
          type="search"
          placeholder="Filter by method or content"
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
        />
        <span className="dim">{visible.length} frames</span>
      </header>

      <ol className="inspector__list">
        {visible.map((frame) => (
          <li key={frame.seq}>
            <button
              type="button"
              className={`inspector__row ${selected === frame.seq ? "is-selected" : ""}`}
              onClick={() => setSelected(selected === frame.seq ? undefined : frame.seq)}
            >
              <span
                className={`inspector__arrow inspector__arrow--${frame.direction}`}
                title={frame.direction === "agentToClient" ? "agent → client" : "client → agent"}
                aria-hidden="true"
              >
                {frame.direction === "agentToClient" ? "←" : "→"}
              </span>
              <span className="inspector__method">{frame.method ?? "(response)"}</span>
              {frame.intercepted && (
                <span className="pill pill--warn" title="Answered by the server, not the browser">
                  server
                </span>
              )}
              <span className="dim inspector__time">
                {new Date(frame.at).toLocaleTimeString()}
              </span>
            </button>
          </li>
        ))}
        {visible.length === 0 && <li className="dim inspector__empty">Nothing yet.</li>}
      </ol>

      {detail && <pre className="inspector__detail">{pretty(detail.line)}</pre>}
    </section>
  );
}

/** Pretty-prints a frame, falling back to the raw text if it isn't JSON. */
function pretty(line: string): string {
  try {
    return JSON.stringify(JSON.parse(line), null, 2);
  } catch {
    return line;
  }
}
