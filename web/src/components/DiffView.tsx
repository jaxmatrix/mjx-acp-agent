/** A read-only unified diff. */

import { useMemo } from "react";
import { diffLines, diffStat } from "../acp/diff";

export function DiffView({
  path,
  oldText,
  newText,
}: {
  path: string;
  oldText: string;
  newText: string;
}) {
  // Diffing is O(n·m); recomputing it on every keystroke elsewhere in the app
  // would be felt.
  const lines = useMemo(() => diffLines(oldText, newText), [oldText, newText]);
  const stat = useMemo(() => diffStat(lines), [lines]);

  return (
    <figure className="diff">
      <figcaption className="diff__header">
        <code>{path}</code>
        <span className="diff__stat">
          <span className="diff__added">+{stat.added}</span>{" "}
          <span className="diff__removed">−{stat.removed}</span>
        </span>
      </figcaption>
      <div className="diff__body">
        {lines.map((line, index) => (
          <div key={index} className={`diff__line diff__line--${line.kind}`}>
            <span className="diff__gutter">{line.oldLine ?? ""}</span>
            <span className="diff__gutter">{line.newLine ?? ""}</span>
            <span className="diff__sign" aria-hidden="true">
              {line.kind === "added" ? "+" : line.kind === "removed" ? "−" : " "}
            </span>
            <span className="diff__text">{line.text}</span>
          </div>
        ))}
      </div>
    </figure>
  );
}
