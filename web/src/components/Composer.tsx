/** The prompt box. */

import { useRef, useState } from "react";
import type { ContentBlock } from "@agentclientprotocol/sdk";
import type { AvailableCommand } from "../acp/types";

export function Composer({
  busy,
  ready,
  commands,
  onSend,
  onCancel,
}: {
  busy: boolean;
  /** Whether the session exists yet. */
  ready: boolean;
  commands: AvailableCommand[];
  onSend(prompt: ContentBlock[]): void;
  onCancel(): void;
}) {
  const [text, setText] = useState("");
  const input = useRef<HTMLTextAreaElement>(null);

  // Only offer completions while the whole input is one unfinished command:
  // a `/` in the middle of prose is not a command.
  const suggestions =
    text.startsWith("/") && !text.includes(" ")
      ? commands.filter((c) => c.name.startsWith(text.slice(1)))
      : [];

  // Real agents take a few seconds to start and create a session. Accepting a
  // prompt before then loses it and reports "not connected", which tells the
  // user nothing about what to do.
  const canSend = ready && !busy && text.trim().length > 0;

  function send() {
    if (!canSend) return;
    onSend([{ type: "text", text: text.trim() }]);
    setText("");
  }

  const placeholder = !ready
    ? "Starting the agent…"
    : busy
      ? "Agent is working…"
      : "Ask the agent something";

  return (
    <div className="composer">
      {suggestions.length > 0 && (
        <ul className="composer__suggestions">
          {suggestions.map((command) => (
            <li key={command.name}>
              <button
                type="button"
                onClick={() => {
                  setText(`/${command.name} `);
                  input.current?.focus();
                }}
              >
                <code>/{command.name}</code> <span className="dim">{command.description}</span>
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="composer__row">
        <textarea
          ref={input}
          rows={2}
          value={text}
          placeholder={placeholder}
          disabled={!ready}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            // Enter sends, Shift-Enter is a newline — the convention every
            // chat input follows.
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              send();
            }
          }}
        />
        {busy ? (
          <button type="button" className="button button--stop" onClick={onCancel}>
            Stop
          </button>
        ) : (
          <button type="button" className="button" onClick={send} disabled={!canSend}>
            Send
          </button>
        )}
      </div>
    </div>
  );
}
