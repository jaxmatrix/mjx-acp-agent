/**
 * Turning what was typed into what is sent.
 *
 * The composer's text is the model, and a mention in it is the Markdown link
 * Zed also uses: `[@name](uri)`. This is where those links become
 * `resource_link` content blocks and everything else stays text.
 *
 * Port of `parse_mention_links` and `mention_to_content_block`
 * (`reference/zed-acp/agent_ui/src/message_editor.rs:2182, 2137`).
 */

import type { ContentBlock } from "@agentclientprotocol/sdk";

import { mentionName, mentionToUri, parseMentionUri, type MentionUri } from "./mention";

/**
 * `[@name](uri)`, with a URI that runs to the closing paren.
 *
 * A URI may not contain a paren or whitespace here, which is why
 * {@link mentionMarkdown} escapes them: a file really can be called `a(1).txt`.
 */
const MENTION_LINK = /\[@([^\]]*)\]\(([^()\s]*)\)/g;

/** The link text for a mention, with parens escaped so the regex above holds. */
export function mentionMarkdown(mention: MentionUri): string {
  const uri = mentionToUri(mention).replaceAll("(", "%28").replaceAll(")", "%29");
  return `[@${mentionName(mention)}](${uri})`;
}

/**
 * Splits typed text into the blocks a `session/prompt` carries.
 *
 * A link whose URI does not parse is left as literal text: emitting a
 * `resource_link` we could not read back ourselves would put something on the
 * wire that no part of this project understands.
 *
 * Text with no links comes back as exactly one text block, byte for byte what
 * it was before mentions existed.
 */
export function promptBlocks(text: string): ContentBlock[] {
  const blocks: ContentBlock[] = [];
  let cut = 0;

  const pushText = (value: string) => {
    if (value.length > 0) blocks.push({ type: "text", text: value });
  };

  for (const match of text.matchAll(MENTION_LINK)) {
    const [whole, name, uri] = match;
    let mention: MentionUri;
    try {
      mention = parseMentionUri(uri!, "unix");
    } catch {
      continue; // Not a mention. Leave it in the surrounding text.
    }

    pushText(text.slice(cut, match.index));
    blocks.push({
      type: "resource_link",
      uri: mentionToUri(mention),
      name: name!.length > 0 ? name! : mentionName(mention),
    });
    cut = match.index + whole.length;
  }

  pushText(text.slice(cut));
  return blocks;
}
