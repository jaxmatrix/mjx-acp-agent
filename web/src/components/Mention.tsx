/** An `@`-mention, as it appears in a message. */

import { mentionName, mentionTooltip, parseMentionUri, type MentionUri } from "../acp/mention";

/**
 * A mention chip.
 *
 * A `<span>`, not a button or a link. Clicking a mention should open the file
 * it names, and there is no editor here to open it in; a button that does
 * nothing is a worse answer than plain text. See the README's limitations.
 *
 * `mentionName` is total over all thirteen variants, so a chip always has
 * something to show — the fallback below is for a URI neither port could parse
 * at all, which is the only case ACP's own `name` has to cover.
 */
export function MentionChip({ uri, name }: { uri: string; name: string }) {
  let mention: MentionUri | null = null;
  try {
    mention = parseMentionUri(uri, "unix");
  } catch {
    mention = null;
  }

  if (mention === null) {
    return (
      <span className="mention mention--unknown" title={uri}>
        @{name}
      </span>
    );
  }

  return (
    <span className="mention" data-mention={mention.variant} title={mentionTooltip(mention) ?? uri}>
      @{label(mention)}
    </span>
  );
}

/** What the chip reads. Elided for a URL, which is otherwise unboundedly long. */
function label(mention: MentionUri): string {
  if (mention.variant === "directory") return `${mentionName(mention)}/`;
  if (mention.variant === "fetch") return elide(mention.url.replace(/^https?:\/\//, ""), 40);
  return mentionName(mention);
}

function elide(value: string, max: number): string {
  return value.length <= max ? value : `${value.slice(0, max - 1)}…`;
}

/**
 * A path a tool call touched, shown muted rather than as a chip.
 *
 * A file the agent *read* is not a file the user *pointed at*, and drawing them
 * the same way misreads the conversation. Zed makes the same distinction
 * (`reference/zed-acp/agent_ui/src/conversation_view/thread_view.rs:10151`).
 */
export function ResourceRow({ uri, name }: { uri: string; name: string }) {
  let display = name;
  try {
    display = mentionName(parseMentionUri(uri, "unix"));
  } catch {
    // Keep the name the agent gave it.
  }
  return (
    <p className="tool-call__resource dim" title={uri}>
      {display}
    </p>
  );
}
