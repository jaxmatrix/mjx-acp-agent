/** The conversation timeline. */

import { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { ToolCallCard } from "./ToolCallCard";
import { chunkText, type AssistantChunk, type Thread } from "../acp/types";

export function ThreadView({
  thread,
  onPermission,
}: {
  thread: Thread;
  onPermission(toolCallId: string, optionId: string | null): void;
}) {
  const bottom = useRef<HTMLDivElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const [follow, setFollow] = useState(true);

  // Follow the tail while the user is at the bottom, and stop the moment they
  // scroll up — yanking someone back down mid-read is the worst thing a
  // streaming log can do.
  useEffect(() => {
    if (follow) bottom.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [thread.entries, follow]);

  function onScroll() {
    const el = scroller.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    setFollow(distanceFromBottom < 80);
  }

  return (
    <div className="thread" ref={scroller} onScroll={onScroll}>
      {thread.entries.length === 0 && (
        <p className="thread__empty dim">
          Ask the agent something. Try “fix the median bug in stats.js”.
        </p>
      )}

      {thread.entries.map((entry) => {
        switch (entry.type) {
          case "user":
            return (
              <article key={entry.id} className="message message--user">
                {entry.content.map((block, i) => (
                  <p key={i}>{block.type === "text" ? block.text : `[${block.type}]`}</p>
                ))}
              </article>
            );

          case "assistant":
            return (
              <article key={entry.id} className="message message--assistant">
                {entry.chunks.map((chunk, i) => (
                  <ChunkView key={i} chunk={chunk} />
                ))}
              </article>
            );

          case "toolCall":
            return (
              <ToolCallCard
                key={entry.id}
                toolCall={entry.toolCall}
                terminals={thread.terminals}
                onPermission={onPermission}
              />
            );

          default:
            return null;
        }
      })}

      {thread.status === "generating" && <div className="thinking-dots" aria-label="Working" />}
      {thread.stopReason && thread.stopReason !== "end_turn" && (
        <p className="callout callout--warn">Turn ended: {thread.stopReason.replace(/_/g, " ")}</p>
      )}

      <div ref={bottom} />
      {!follow && (
        <button type="button" className="follow-button" onClick={() => setFollow(true)}>
          ↓ Jump to latest
        </button>
      )}
    </div>
  );
}

/** Prose renders as markdown; thinking renders collapsed and dimmed. */
function ChunkView({ chunk }: { chunk: AssistantChunk }) {
  const [open, setOpen] = useState(false);

  if (chunk.kind === "thought") {
    return (
      <details className="thought" open={open} onToggle={(e) => setOpen(e.currentTarget.open)}>
        <summary>Thinking</summary>
        <div className="thought__body">{chunkText(chunk)}</div>
      </details>
    );
  }

  // Text renders as one markdown document so a code fence split across chunks
  // still parses; anything else keeps its own place in the run.
  return (
    <>
      {chunk.content.map((block, i) =>
        block.type === "text" ? null : (
          <NonTextBlock key={i} type={block.type} />
        ),
      )}
      <div className="markdown">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{chunkText(chunk)}</ReactMarkdown>
      </div>
    </>
  );
}

/** A placeholder for content the viewer doesn't render inline yet. */
function NonTextBlock({ type }: { type: string }) {
  return <p className="dim">[{type}]</p>;
}
