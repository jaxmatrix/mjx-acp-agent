/**
 * Parses the same mention URIs the Rust port parses.
 *
 * `fixtures/mention-uris.json` is read here and by
 * `crates/mjx-acp-thread/tests/mention_uris.rs`, the way
 * `session-updates.jsonl` is read by both thread models. Two ports of the same
 * parser are only worth having if something notices when they disagree — and
 * where they will disagree is percent-encoding, which no amount of care in
 * either language prevents.
 *
 * Every shared case is Unix-styled. Windows spellings are in the unit tests
 * below; the fixture cannot express two path styles without becoming a second
 * dialect.
 */

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, test } from "vitest";

import {
  decodePathEscapes,
  isAbsolute,
  lineRangeSuffix,
  mentionLink,
  mentionName,
  mentionToUri,
  parseHyperlink,
  parseHyperlinkLiteral,
  parseMentionUri,
  toNativeWindowsPath,
  type MentionUri,
} from "./mention";

type Case = {
  name: string;
  input: string;
  mode: "parse" | "parseHyperlink" | "parseHyperlinkLiteral";
  expect: Record<string, unknown>;
  uri?: string;
};

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "../../../fixtures");
const cases: Case[] = JSON.parse(readFileSync(join(fixtures, "mention-uris.json"), "utf8"));

/**
 * The comparable shape of a mention. Only the keys a case names are checked,
 * so a case says exactly what it is about.
 */
function shape(mention: MentionUri): Record<string, unknown> {
  const base = { variant: mention.variant, label: mentionName(mention) };
  switch (mention.variant) {
    case "file":
    case "directory":
      return { ...base, absPath: mention.absPath };
    case "symbol":
      return { ...base, absPath: mention.absPath, lineRange: mention.lineRange };
    case "selection":
      return {
        ...base,
        absPath: mention.absPath,
        lineRange: mention.lineRange,
        column: mention.column,
      };
    case "thread":
      return { ...base, id: mention.id };
    case "rule":
      return { ...base, id: ruleUuid(mention.id) };
    case "diagnostics":
      return {
        ...base,
        includeErrors: mention.includeErrors,
        includeWarnings: mention.includeWarnings,
      };
    case "fetch":
      return { ...base, url: mention.url };
    case "terminalSelection":
      return { ...base, lineCount: mention.lineCount };
    case "gitDiff":
      return { ...base, baseRef: mention.baseRef };
    case "mergeConflict":
      return { ...base, filePath: mention.filePath };
    case "skill":
      return { ...base, source: mention.source, absPath: mention.skillFilePath };
    case "pastedImage":
      return base;
  }
}

function ruleUuid(id: unknown): string {
  const user = (id as { User?: { uuid?: string } }).User;
  return user?.uuid ?? "";
}

function run(kase: Case): MentionUri | null {
  try {
    switch (kase.mode) {
      case "parse":
        return parseMentionUri(kase.input, "unix");
      case "parseHyperlink":
        return parseHyperlink(kase.input, "unix");
      case "parseHyperlinkLiteral":
        return parseHyperlinkLiteral(kase.input, "unix");
    }
  } catch {
    return null;
  }
}

describe("the shared mention fixture", () => {
  test("every case is read", () => {
    // Asserted on both sides, so a case added to the fixture and read by only
    // one of the two ports fails rather than passing quietly.
    expect(cases).toHaveLength(49);
  });

  for (const kase of cases) {
    test(kase.name, () => {
      const parsed = run(kase);

      if (kase.expect.error === true || kase.expect.none === true) {
        expect(parsed).toBeNull();
        return;
      }

      expect(parsed, "did not parse").not.toBeNull();
      const actual = shape(parsed!);
      for (const [key, value] of Object.entries(kase.expect)) {
        expect(actual[key], `${key} — whole shape was ${JSON.stringify(actual)}`).toEqual(value);
      }

      if (kase.uri !== undefined) {
        expect(mentionToUri(parsed!), "serialized differently").toBe(kase.uri);
      }
    });
  }
});

describe("windows path spellings", () => {
  test("file uris use native separators", () => {
    expect(parseMentionUri("file:///C:/path/to/file.rs", "windows")).toEqual({
      variant: "file",
      absPath: "C:\\path\\to\\file.rs",
    });
    expect(parseMentionUri("file:///C:/path/to/dir/", "windows")).toEqual({
      variant: "directory",
      absPath: "C:\\path\\to\\dir\\",
    });
  });

  test("drive letters are uppercased whichever way they are spelled", () => {
    for (const input of [
      "file:///c:/foo/bar.rs",
      "/c:/foo/bar.rs",
      "/c/foo/bar.rs",
      "c:\\foo\\bar.rs",
      "c:/foo/bar.rs",
    ]) {
      expect(parseHyperlink(input, "windows"), input).toEqual({
        variant: "file",
        absPath: "C:\\foo\\bar.rs",
      });
    }
  });

  test("msys style paths require a lowercase drive", () => {
    // Uppercase `/C/foo` is more likely a real directory than a drive.
    expect(parseHyperlink("/C/Users/readme.md", "windows")).toEqual({
      variant: "file",
      absPath: "\\C\\Users\\readme.md",
    });
  });

  test("unix paths are not rewritten as windows drives", () => {
    expect(parseHyperlink("/c/Projects/AGENTS.md", "unix")).toEqual({
      variant: "file",
      absPath: "/c/Projects/AGENTS.md",
    });
  });

  test("a unc path becomes backslashes", () => {
    expect(parseHyperlink("//server/share/dir/file.rs", "windows")).toEqual({
      variant: "file",
      absPath: "\\\\server\\share\\dir\\file.rs",
    });
  });

  test("a native path that needs no change is left alone", () => {
    expect(toNativeWindowsPath("C:\\dir\\file.rs")).toBeNull();
  });

  test("a windows path is only absolute under the windows style", () => {
    expect(isAbsolute("C:\\dir\\file.rs", "windows")).toBe(true);
    expect(isAbsolute("C:\\dir\\file.rs", "unix")).toBe(false);
  });
});

describe("mention helpers", () => {
  test("a mention link is markdown", () => {
    expect(mentionLink({ variant: "file", absPath: "/w/stats.js" })).toBe(
      "[@stats.js](file:///w/stats.js)",
    );
  });

  test("a line range suffix is one-based", () => {
    expect(lineRangeSuffix([4, 4])).toBe(":5");
    expect(lineRangeSuffix([4, 8])).toBe(":5-9");
  });

  test("decoding a path cannot introduce a traversal", () => {
    expect(decodePathEscapes("/tmp/..%2F..%2Fsecret")).toBe("/tmp/..%2F..%2Fsecret");
    expect(decodePathEscapes("/tmp/a%20b.rs")).toBe("/tmp/a b.rs");
    expect(decodePathEscapes("/tmp/100%_done.txt")).toBe("/tmp/100%_done.txt");
  });

  test("every variant has a name to show", () => {
    // A chip can never fall back to rendering `[resource_link]`, so this has
    // to be total.
    const named: [MentionUri, string][] = [
      [{ variant: "file", absPath: "/a/stats.js" }, "stats.js"],
      [{ variant: "directory", absPath: "/a/src" }, "src"],
      [{ variant: "pastedImage", name: "Image" }, "Image"],
      [{ variant: "symbol", absPath: "/a/s.js", name: "median", lineRange: [0, 2] }, "median"],
      [{ variant: "thread", id: "s1", name: "A thread" }, "A thread"],
      [{ variant: "rule", id: {}, name: "A rule" }, "A rule"],
      [{ variant: "diagnostics", includeErrors: true, includeWarnings: false }, "Diagnostics"],
      [{ variant: "fetch", url: "https://example.com/" }, "https://example.com/"],
      [{ variant: "terminalSelection", lineCount: 3 }, "Terminal (3 lines)"],
      [{ variant: "gitDiff", baseRef: "main" }, "Branch Diff (main)"],
      [{ variant: "mergeConflict", filePath: "/a/stats.js" }, "Merge Conflict (stats.js)"],
      [
        { variant: "skill", name: "a-skill", source: "here", skillFilePath: "/s/SKILL.md" },
        "a-skill",
      ],
      [
        { variant: "selection", absPath: "/a/stats.js", lineRange: [4, 14], column: null },
        "stats.js (5:15)",
      ],
    ];
    for (const [mention, expected] of named) {
      expect(mentionName(mention), mention.variant).toBe(expected);
    }
    // All thirteen, so a new variant cannot be added without a label.
    expect(named).toHaveLength(13);
  });
});
