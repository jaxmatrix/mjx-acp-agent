/**
 * What the caret is in the middle of typing.
 *
 * The composer is a plain `<textarea>` and its text is the whole model — a
 * mention is the Markdown `[@name](uri)` Zed also uses, so nothing has to be
 * kept in sync with the string. That leaves one question to answer on every
 * keystroke: is the caret inside an unfinished `@`-mention or an unfinished
 * `/`-command? These are the pure functions that answer it, so they can be
 * tested without a DOM — there is no jsdom in this project.
 *
 * The command half is a port of Zed's `SlashCommandCompletion::try_parse`
 * (`reference/zed-acp/agent_ui/src/completion_provider.rs:2016`).
 */

/** A half-typed mention or command, and the span it occupies. */
export type Trigger =
  | { kind: "mention"; range: [start: number, end: number]; query: string }
  | {
      kind: "command";
      range: [start: number, end: number];
      name: string;
      /** Null until the first space; "" once a space has been typed. */
      argument: string | null;
    };

/** Characters a mention query may contain — a path, essentially. */
const MENTION_QUERY = /^[^\s()[\]]*$/;

function isSpace(char: string | undefined): boolean {
  return char === undefined || /\s/.test(char);
}

export function triggerAt(text: string, caret: number): Trigger | null {
  return mentionAt(text, caret) ?? commandAt(text, caret);
}

/**
 * The `@` nearest the caret with no whitespace in between.
 *
 * `@` must start a line or follow whitespace, so `user@host` is an address and
 * not a mention.
 */
function mentionAt(text: string, caret: number): Trigger | null {
  const before = text.slice(0, caret);
  const at = before.lastIndexOf("@");
  if (at < 0) return null;
  if (!isSpace(before[at - 1])) return null;

  const query = before.slice(at + 1);
  if (!MENTION_QUERY.test(query)) return null;

  return { kind: "mention", range: [at, caret], query };
}

/**
 * The `/` that starts the command the caret is in, if any.
 *
 * A `/` counts only at the start of a line or after whitespace, and only when
 * something other than whitespace follows it — otherwise every path in a
 * sentence would open the menu.
 */
function commandAt(text: string, caret: number): Trigger | null {
  const before = text.slice(0, caret);
  const lineStart = before.lastIndexOf("\n") + 1;
  const slash = before.indexOf("/", lineStart);
  if (slash < 0) return null;
  if (!isSpace(before[slash - 1]) && slash !== lineStart) return null;
  if (isSpace(text[slash + 1])) return null;

  // The command runs to the end of its line; the argument is whatever follows
  // the first space in it.
  const lineEnd = text.indexOf("\n", slash);
  const line = text.slice(slash + 1, lineEnd < 0 ? undefined : lineEnd);
  const space = line.search(/\s/);

  if (space < 0) {
    return { kind: "command", range: [slash, slash + 1 + line.length], name: line, argument: null };
  }
  return {
    kind: "command",
    range: [slash, slash + 1 + line.length],
    name: line.slice(0, space),
    argument: line.slice(space + 1),
  };
}

/** Replaces a trigger's span, leaving the caret after what was inserted. */
export function applyCompletion(
  text: string,
  range: [number, number],
  replacement: string,
): { text: string; caret: number } {
  const next = text.slice(0, range[0]) + replacement + text.slice(range[1]);
  return { text: next, caret: range[0] + replacement.length };
}
