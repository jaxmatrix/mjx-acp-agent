/**
 * A tool call, with its status, content and any pending authorization.
 *
 * Follows the structure of Zed's `render_tool_call`
 * (`reference/zed-acp/agent_ui/src/conversation_view/thread_view.rs`): a header
 * that is always visible, and content that can be collapsed away.
 */

import { useState } from "react";
import type { ToolKind } from "@agentclientprotocol/sdk";

import { DiffView } from "./DiffView";
import { PermissionPrompt } from "./PermissionPrompt";
import { TerminalView } from "./TerminalView";
import type { Terminal, ToolCall } from "../acp/types";

/** A glyph per tool kind, so the timeline is scannable without reading. */
const ICONS: Record<ToolKind, string> = {
  read: "📖",
  edit: "✏️",
  delete: "🗑️",
  move: "📦",
  search: "🔍",
  execute: "▶️",
  think: "💭",
  fetch: "🌐",
  switch_mode: "🔀",
  other: "🔧",
};

const STATUS_LABEL: Record<ToolCall["status"], string> = {
  pending: "pending",
  in_progress: "running",
  completed: "done",
  failed: "failed",
};

export function ToolCallCard({
  toolCall,
  terminals,
  onPermission,
}: {
  toolCall: ToolCall;
  terminals: Record<string, Terminal>;
  onPermission(toolCallId: string, optionId: string | null): void;
}) {
  // Finished calls start collapsed: the interesting one is the one running.
  const [expanded, setExpanded] = useState(
    toolCall.status === "in_progress" || toolCall.status === "pending",
  );
  const hasContent = toolCall.content.length > 0;

  return (
    <div className={`tool-call tool-call--${toolCall.status}`}>
      <button
        type="button"
        className="tool-call__header"
        onClick={() => setExpanded((open) => !open)}
        aria-expanded={expanded}
        disabled={!hasContent}
      >
        <span className="tool-call__icon" aria-hidden="true">
          {ICONS[toolCall.kind] ?? ICONS.other}
        </span>
        <span className="tool-call__title">{toolCall.title}</span>
        <span className={`pill pill--${toolCall.status}`}>{STATUS_LABEL[toolCall.status]}</span>
        {hasContent && (
          <span className="tool-call__chevron" aria-hidden="true">
            {expanded ? "▾" : "▸"}
          </span>
        )}
      </button>

      {toolCall.locations.length > 0 && (
        <div className="tool-call__locations">
          {toolCall.locations.map((location) => (
            <code key={`${location.path}:${location.line ?? ""}`}>
              {shortenPath(location.path)}
              {location.line != null && `:${location.line}`}
            </code>
          ))}
        </div>
      )}

      {expanded && hasContent && (
        <div className="tool-call__content">
          {toolCall.content.map((content, index) => {
            switch (content.type) {
              case "diff":
                return (
                  <DiffView
                    key={index}
                    path={content.path}
                    oldText={content.oldText ?? ""}
                    newText={content.newText}
                  />
                );
              case "terminal": {
                const terminal = terminals[content.terminalId];
                return terminal ? (
                  <TerminalView key={index} terminal={terminal} />
                ) : (
                  <p key={index} className="dim">
                    Waiting for terminal {content.terminalId}…
                  </p>
                );
              }
              case "content":
                return (
                  <pre key={index} className="tool-call__text">
                    {content.content.type === "text"
                      ? content.content.text
                      : `[${content.content.type}]`}
                  </pre>
                );
              default:
                return null;
            }
          })}
        </div>
      )}

      {toolCall.awaitingPermission && (
        <PermissionPrompt
          options={toolCall.awaitingPermission.options}
          onChoose={(optionId) => onPermission(toolCall.id, optionId)}
        />
      )}
    </div>
  );
}

/** Trims a path to its last two segments; the full path is in the title. */
function shortenPath(path: string): string {
  const parts = path.split("/");
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join("/")}`;
}
