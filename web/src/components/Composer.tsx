/** The prompt box. */

import { useRef, useState } from "react";
import type { ContentBlock } from "@agentclientprotocol/sdk";

import { mentionMarkdown, promptBlocks } from "../acp/prompt";
import type { AvailableCommand } from "../acp/types";
import { applyCompletion, triggerAt, type Trigger } from "../composer/trigger";
import { useWorkspaceFiles, type WorkspaceFile } from "../useWorkspaceFiles";

/** One row of the completion menu. */
interface Suggestion {
  key: string;
  /** What is inserted in place of the trigger. */
  replacement: string;
  label: string;
  detail: string;
}

export function Composer({
  busy,
  ready,
  commands,
  cwd,
  onSend,
  onCancel,
}: {
  busy: boolean;
  /** Whether the session exists yet. */
  ready: boolean;
  commands: AvailableCommand[];
  /** The directory this session is open in — the root mentions resolve against. */
  cwd?: string;
  onSend(prompt: ContentBlock[]): void;
  onCancel(): void;
}) {
  const [text, setText] = useState("");
  const [caret, setCaret] = useState(0);
  const [highlighted, setHighlighted] = useState(0);
  // Escape closes the menu until the trigger moves. Without this the next
  // keystroke re-derives the same trigger and it reopens immediately.
  const [dismissed, setDismissed] = useState<string | null>(null);
  const input = useRef<HTMLTextAreaElement>(null);

  const trigger = triggerAt(text, caret);
  const triggerKey = trigger === null ? null : `${trigger.kind}:${trigger.range[0]}`;

  const mentionQuery = trigger?.kind === "mention" ? trigger.query : "";
  const wantsFiles = trigger?.kind === "mention" && !isUrl(mentionQuery);
  const files = useWorkspaceFiles(cwd ?? null, mentionQuery, wantsFiles);

  const suggestions = suggestionsFor(trigger, commands, files);
  const open = suggestions.length > 0 && dismissed !== triggerKey;
  const activeIndex = Math.min(highlighted, suggestions.length - 1);
  const active = open ? (suggestions[activeIndex] ?? null) : null;

  /** The hint for a command that takes an argument, until one is typed. */
  const hint =
    trigger?.kind === "command" && (trigger.argument === null || trigger.argument.length === 0)
      ? commands.find((command) => command.name === trigger.name)?.input?.hint
      : undefined;

  // Real agents take a few seconds to start and create a session. Accepting a
  // prompt before then loses it and reports "not connected", which tells the
  // user nothing about what to do.
  const canSend = ready && !busy && text.trim().length > 0;

  function edit(next: string, nextCaret: number) {
    setText(next);
    setCaret(nextCaret);
    setHighlighted(0);
  }

  function accept(suggestion: Suggestion) {
    if (trigger === null) return;
    const applied = applyCompletion(text, trigger.range, suggestion.replacement);
    edit(applied.text, applied.caret);
    setDismissed(null);
    const box = input.current;
    if (box) {
      // React writes the value on the next render, so move the caret after it.
      queueMicrotask(() => {
        box.focus();
        box.setSelectionRange(applied.caret, applied.caret);
      });
    }
  }

  function send() {
    if (!canSend) return;
    onSend(promptBlocks(text.trim()));
    edit("", 0);
    setDismissed(null);
  }

  const placeholder = !ready
    ? "Starting the agent…"
    : busy
      ? "Agent is working…"
      : "Ask the agent something — @ for a file, / for a command";

  return (
    <div className="composer">
      {open && (
        <ul className="composer__suggestions" role="listbox" id="composer-suggestions">
          {suggestions.map((suggestion, index) => (
            <li key={suggestion.key}>
              <button
                type="button"
                id={`composer-suggestion-${index}`}
                role="option"
                aria-selected={suggestion === active}
                className={suggestion === active ? "is-active" : undefined}
                // Mouse down, not click: the textarea must not lose focus.
                onMouseDown={(event) => {
                  event.preventDefault();
                  accept(suggestion);
                }}
                onMouseEnter={() => setHighlighted(index)}
              >
                <code>{suggestion.label}</code> <span className="dim">{suggestion.detail}</span>
              </button>
            </li>
          ))}
        </ul>
      )}

      {!open && hint !== undefined && trigger?.kind === "command" && (
        <p className="composer__hint" id="composer-hint">
          <code>/{trigger.name}</code> <span className="dim">· {hint}</span>
        </p>
      )}

      <div className="composer__row">
        <textarea
          ref={input}
          rows={2}
          value={text}
          placeholder={placeholder}
          disabled={!ready}
          role="combobox"
          aria-expanded={open}
          aria-controls="composer-suggestions"
          aria-activedescendant={active ? `composer-suggestion-${activeIndex}` : undefined}
          aria-describedby={hint !== undefined ? "composer-hint" : undefined}
          onChange={(event) => edit(event.target.value, event.target.selectionStart)}
          onSelect={(event) => setCaret(event.currentTarget.selectionStart)}
          onKeyDown={(event) => {
            // The menu gets first refusal on every key it uses — Enter above
            // all, or accepting a completion would send the message instead.
            if (open && active) {
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                const step = event.key === "ArrowDown" ? 1 : suggestions.length - 1;
                setHighlighted((current) => (current + step) % suggestions.length);
                return;
              }
              if (event.key === "Enter" || event.key === "Tab") {
                event.preventDefault();
                accept(active);
                return;
              }
              if (event.key === "Escape") {
                event.preventDefault();
                setDismissed(triggerKey);
                return;
              }
            }

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

function isUrl(query: string): boolean {
  return query.startsWith("http://") || query.startsWith("https://");
}

/** What the menu offers for the trigger the caret is in. */
function suggestionsFor(
  trigger: Trigger | null,
  commands: AvailableCommand[],
  files: WorkspaceFile[],
): Suggestion[] {
  if (trigger === null) return [];

  if (trigger.kind === "command") {
    // Only while the name is still being typed: once there is an argument the
    // command is chosen and the hint takes over.
    if (trigger.argument !== null) return [];
    return commands
      .filter((command) => command.name.startsWith(trigger.name))
      .map((command) => ({
        key: command.name,
        replacement: `/${command.name} `,
        label: command.input ? `/${command.name} <${command.input.hint}>` : `/${command.name}`,
        detail: command.description,
      }));
  }

  // A URL is a mention the workspace knows nothing about, so it is offered
  // directly rather than looked up.
  if (isUrl(trigger.query)) {
    return [
      {
        key: trigger.query,
        replacement: `${mentionMarkdown({ variant: "fetch", url: trigger.query })} `,
        label: trigger.query,
        detail: "fetch this URL",
      },
    ];
  }

  return files.slice(0, 12).map((file) => ({
    key: file.path,
    replacement: `${mentionMarkdown(
      file.isDir
        ? { variant: "directory", absPath: file.path }
        : { variant: "file", absPath: file.path },
    )} `,
    label: file.isDir ? `${file.name}/` : file.name,
    detail: file.relPath,
  }));
}
