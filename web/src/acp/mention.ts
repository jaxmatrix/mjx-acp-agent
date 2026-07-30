/**
 * What an `@`-mention points at.
 *
 * The browser's half of `crates/mjx-acp-thread/src/mention.rs`, which is itself
 * a port of Zed's `acp_thread/src/mention.rs`. A mention travels over ACP as a
 * `resource_link` and its `uri` is the whole of what it means, so both ends of
 * this project need the same parser — and it has to be exact, or a link that
 * survives one hop stops surviving the other.
 *
 * The two are held together by `fixtures/mention-uris.json`, read by
 * `mention.test.ts` here and `crates/mjx-acp-thread/tests/mention_uris.rs`
 * there, the same way `session-updates.jsonl` holds the two thread models
 * together. Change the parsing rules and the same case has to move on both
 * sides.
 *
 * URIs are built by hand rather than through `URL`'s setters: the WHATWG
 * pathname setter and Rust's `Url::set_path` do not percent-encode the same
 * set, and a mention that serialises differently on the two sides is exactly
 * the drift the fixture exists to catch.
 */

/** Which spelling of a path a string is written in. */
export type PathStyle = "unix" | "windows";

/** Line ranges are stored 0-based and inclusive; URIs spell them 1-based. */
export type LineRange = readonly [start: number, end: number];

export type MentionUri =
  | { variant: "file"; absPath: string }
  | { variant: "pastedImage"; name: string }
  | { variant: "directory"; absPath: string }
  | { variant: "symbol"; absPath: string; name: string; lineRange: LineRange }
  | { variant: "thread"; id: string; name: string }
  /** Deprecated, kept because older threads still carry it. */
  | { variant: "rule"; id: unknown; name: string }
  | { variant: "diagnostics"; includeErrors: boolean; includeWarnings: boolean }
  | { variant: "selection"; absPath: string | null; lineRange: LineRange; column: number | null }
  | { variant: "fetch"; url: string }
  | { variant: "terminalSelection"; lineCount: number }
  | { variant: "gitDiff"; baseRef: string }
  | { variant: "mergeConflict"; filePath: string }
  | { variant: "skill"; name: string; source: string; skillFilePath: string };

const DEPRECATED_RULE_ID = { User: { uuid: "00000000-0000-0000-0000-000000000000" } };

function fail(message: string): never {
  throw new Error(message);
}

function isWindows(style: PathStyle): boolean {
  return style === "windows";
}

function hasDrivePrefix(path: string): boolean {
  return /^[A-Za-z]:/.test(path);
}

/** Whether `path` is absolute *in the given style*, textually. */
export function isAbsolute(path: string, style: PathStyle): boolean {
  if (style === "unix") return path.startsWith("/");
  // `/C:/foo` and `//server/share` are both spellings Windows tooling emits.
  return path.startsWith("/") || path.startsWith("\\") || hasDrivePrefix(path);
}

/** Percent-decodes a whole path, falling back to the input if it is malformed. */
function percentDecode(input: string): string {
  try {
    return decodeURIComponent(input);
  } catch {
    return input;
  }
}

/**
 * Splits a trailing `:row` or `:row:column` off a path.
 *
 * A suffix only counts when it parses as a number, which is what keeps a
 * Windows drive letter out of it: `\dir\file.rs` is not a number.
 */
export function pathWithPosition(input: string): {
  path: string;
  row: number | null;
  column: number | null;
} {
  const bare = { path: input, row: null, column: null };

  const lastColon = input.lastIndexOf(":");
  if (lastColon < 0) return bare;
  const head = input.slice(0, lastColon);
  const last = parseIndex(input.slice(lastColon + 1));
  if (last === null) return bare;

  const secondColon = head.lastIndexOf(":");
  if (secondColon >= 0) {
    const middle = parseIndex(head.slice(secondColon + 1));
    if (middle !== null) {
      return { path: head.slice(0, secondColon), row: middle, column: last };
    }
  }
  return { path: head, row: last, column: null };
}

/** A non-negative decimal integer, or null. Rust's `parse::<u32>()`. */
function parseIndex(input: string): number | null {
  if (!/^\d+$/.test(input)) return null;
  const value = Number(input);
  return Number.isSafeInteger(value) ? value : null;
}

/**
 * Parses a line-range fragment: `L10:20`, `L10-20`, `L10-L20` or `L1872`.
 *
 * Returns a 0-based inclusive range; the URI spells it 1-based.
 */
export function parseLineRange(fragment: string): LineRange {
  const range = fragment.startsWith("L") ? fragment.slice(1) : fragment;

  let start: string;
  let end: string;
  const colon = range.indexOf(":");
  const dash = range.indexOf("-");
  if (colon >= 0) {
    start = range.slice(0, colon);
    end = range.slice(colon + 1);
  } else if (dash >= 0) {
    start = range.slice(0, dash);
    const tail = range.slice(dash + 1);
    end = tail.startsWith("L") ? tail.slice(1) : tail;
  } else {
    start = range;
    end = range;
  }

  const startLine = parseIndex(start);
  const endLine = parseIndex(end);
  if (startLine === null) fail("Parsing line range start");
  if (endLine === null) fail("Parsing line range end");
  if (startLine === 0 || endLine === 0) fail("Line numbers should be 1-based");
  return [startLine - 1, endLine - 1];
}

function queryParam(url: URL, name: string): string | null {
  return url.searchParams.get(name);
}

function singleQueryParam(url: URL, name: string): string | null {
  const pairs = [...url.searchParams];
  if (pairs.length === 0) return null;
  if (pairs.length > 1) fail("too many query pairs");
  const [key, value] = pairs[0]!;
  if (key !== name) fail("invalid query parameter");
  return value;
}

function validateQueryParams(url: URL, allowed: string[]): void {
  for (const [key] of url.searchParams) {
    if (!allowed.includes(key)) fail("invalid query parameter");
  }
}

function parseColumn(input: string | null): number | null {
  if (input === null) return null;
  const value = parseIndex(input);
  if (value === null || value === 0) return null;
  return value - 1;
}

/** The fragment without its `#`, or null. `URL.hash` keeps the `#`. */
function fragmentOf(url: URL): string | null {
  return url.hash.length > 1 ? url.hash.slice(1) : null;
}

function stripBackticks(input: string): string {
  return input.startsWith("`") && input.endsWith("`") && input.length >= 2
    ? input.slice(1, -1)
    : input;
}

export function parseMentionUri(input: string, style: PathStyle): MentionUri {
  input = stripBackticks(input);

  if (isAbsolute(input, style) && !input.includes("://")) {
    return parseAbsolutePath(input);
  }

  const url = new URL(input);
  const rawPath = url.pathname;

  switch (url.protocol) {
    case "file:": {
      const trimmed = isWindows(style) ? rawPath.replace(/^\/+/, "") : rawPath;
      const decoded = percentDecode(trimmed);
      const path = isWindows(style) ? (toNativeWindowsPath(decoded) ?? decoded) : decoded;

      const fragment = fragmentOf(url);
      if (fragment !== null) {
        validateQueryParams(url, ["symbol", "column"]);
        let lineRange: LineRange;
        try {
          lineRange = parseLineRange(fragment);
        } catch {
          lineRange = [0, 0];
        }
        const column = parseColumn(queryParam(url, "column"));
        const symbol = queryParam(url, "symbol");
        if (symbol !== null) {
          return { variant: "symbol", name: symbol, absPath: path, lineRange };
        }
        return { variant: "selection", absPath: path, lineRange, column };
      }
      if (input.endsWith("/")) return { variant: "directory", absPath: path };
      return { variant: "file", absPath: path };
    }

    case "zed:":
      return parseZed(url, rawPath, input);

    case "http:":
    case "https:":
      return { variant: "fetch", url: url.toString() };

    default:
      return fail(`unrecognized scheme ${JSON.stringify(url.protocol.replace(/:$/, ""))}`);
  }
}

function parseZed(url: URL, path: string, input: string): MentionUri {
  if (path.startsWith("/agent/thread/")) {
    const name = singleQueryParam(url, "name");
    if (name === null) fail("Missing thread name");
    return { variant: "thread", id: path.slice("/agent/thread/".length), name };
  }
  if (path.startsWith("/agent/rule/")) {
    // Deprecated: parses legacy rule mentions.
    const name = singleQueryParam(url, "name");
    if (name === null) fail("Missing rule name");
    const ruleId = path.slice("/agent/rule/".length);
    const id = ruleId.length === 0 ? DEPRECATED_RULE_ID : { User: { uuid: ruleId } };
    return { variant: "rule", id, name };
  }
  if (path === "/agent/diagnostics") {
    let includeErrors = true;
    let includeWarnings = false;
    for (const [key, value] of url.searchParams) {
      if (key === "include_warnings") includeWarnings = value === "true";
      else if (key === "include_errors") includeErrors = value === "true";
      else fail("invalid query parameter");
    }
    return { variant: "diagnostics", includeErrors, includeWarnings };
  }
  if (path.startsWith("/agent/pasted-image")) {
    return { variant: "pastedImage", name: singleQueryParam(url, "name") ?? "Image" };
  }
  if (path.startsWith("/agent/untitled-buffer")) {
    const fragment = fragmentOf(url);
    if (fragment === null) fail("Missing fragment for untitled buffer selection");
    const lineRange = parseLineRange(fragment);
    validateQueryParams(url, ["column"]);
    return {
      variant: "selection",
      absPath: null,
      lineRange,
      column: parseColumn(queryParam(url, "column")),
    };
  }
  if (path.startsWith("/agent/symbol/")) {
    const fragment = fragmentOf(url);
    if (fragment === null) fail("Missing fragment for untitled buffer selection");
    const lineRange = parseLineRange(fragment);
    const absPath = singleQueryParam(url, "path");
    if (absPath === null) fail("Missing path for symbol");
    return {
      variant: "symbol",
      name: path.slice("/agent/symbol/".length),
      absPath,
      lineRange,
    };
  }
  if (path.startsWith("/agent/file")) {
    const absPath = singleQueryParam(url, "path");
    if (absPath === null) fail("Missing path for file");
    return { variant: "file", absPath };
  }
  if (path.startsWith("/agent/directory")) {
    const absPath = singleQueryParam(url, "path");
    if (absPath === null) fail("Missing path for directory");
    return { variant: "directory", absPath };
  }
  if (path.startsWith("/agent/selection")) {
    validateQueryParams(url, ["path", "column"]);
    const fragment = fragmentOf(url);
    if (fragment === null) fail("Missing fragment for selection");
    const lineRange = parseLineRange(fragment);
    const column = parseColumn(queryParam(url, "column"));
    const absPath = queryParam(url, "path");
    if (absPath === null) fail("Missing path for selection");
    return { variant: "selection", absPath, lineRange, column };
  }
  if (path.startsWith("/agent/terminal-selection")) {
    const lines = singleQueryParam(url, "lines") ?? "0";
    return { variant: "terminalSelection", lineCount: parseIndex(lines) ?? 0 };
  }
  if (path.startsWith("/agent/git-diff")) {
    return { variant: "gitDiff", baseRef: singleQueryParam(url, "base") ?? "main" };
  }
  if (path.startsWith("/agent/merge-conflict")) {
    return { variant: "mergeConflict", filePath: singleQueryParam(url, "path") ?? "" };
  }
  if (path.startsWith("/agent/skill")) {
    let name: string | null = null;
    let source: string | null = null;
    let skillFilePath: string | null = null;
    for (const [key, value] of url.searchParams) {
      if (key === "name") {
        if (name !== null) fail("duplicate skill name query parameter");
        name = value;
      } else if (key === "source") {
        if (source !== null) fail("duplicate skill source query parameter");
        source = value;
      } else if (key === "path") {
        if (skillFilePath !== null) fail("duplicate skill file path query parameter");
        skillFilePath = value;
      } else {
        fail("invalid query parameter");
      }
    }
    if (name === null) fail("missing skill name");
    if (source === null) fail("missing skill source");
    if (skillFilePath === null) fail("missing skill file path");
    return { variant: "skill", name, source, skillFilePath };
  }
  return fail(`invalid zed url: ${JSON.stringify(input)}`);
}

function splitPathFragment(input: string): [string, string | null] {
  const hash = input.indexOf("#");
  return hash < 0 ? [input, null] : [input.slice(0, hash), input.slice(hash + 1)];
}

function parseAbsolutePath(input: string): MentionUri {
  const [pathInput, fragment] = splitPathFragment(input);
  return absolutePathMention(pathInput, fragment);
}

function absolutePathMention(pathInput: string, fragment: string | null): MentionUri {
  if (fragment !== null) {
    try {
      return {
        variant: "selection",
        absPath: pathInput,
        lineRange: parseLineRange(fragment),
        column: null,
      };
    } catch {
      // Not a line range; fall through and read the path as it stands.
    }
  }

  const { path, row, column } = pathWithPosition(pathInput);
  if (row !== null) {
    if (row === 0) fail("Line numbers should be 1-based");
    const line = row - 1;
    return {
      variant: "selection",
      absPath: path,
      lineRange: [line, line],
      column: column === null ? null : Math.max(column - 1, 0),
    };
  }
  return { variant: "file", absPath: path };
}

/**
 * Parses a hyperlink target from agent-authored Markdown.
 *
 * Unlike {@link parseMentionUri} — which stays strict so canonical mention URIs
 * round-trip verbatim — bare path targets are normalized first.
 */
export function parseHyperlink(input: string, style: PathStyle): MentionUri {
  const target = barePathTarget(input, style);
  if (target !== null) return parseHyperlinkPath(target, style, true);
  return parseMentionUri(input, style);
}

/**
 * The literal (un-decoded) reading of a bare-path hyperlink, for files whose
 * names really do contain an escape sequence. Null when it would not differ.
 */
export function parseHyperlinkLiteral(input: string, style: PathStyle): MentionUri | null {
  const target = barePathTarget(input, style);
  if (target === null) return null;
  const [pathInput] = splitPathFragment(target);
  if (decodePathEscapes(pathInput) === pathInput) return null;
  try {
    return parseHyperlinkPath(target, style, false);
  } catch {
    return null;
  }
}

function barePathTarget(input: string, style: PathStyle): string | null {
  const stripped = stripBackticks(input);
  return isAbsolute(stripped, style) && !stripped.includes("://") ? stripped : null;
}

function parseHyperlinkPath(input: string, style: PathStyle, decode: boolean): MentionUri {
  const [rawPath, fragment] = splitPathFragment(input);
  const decoded = decode ? decodePathEscapes(rawPath) : rawPath;
  const path = isWindows(style) ? (toNativeWindowsPath(decoded) ?? decoded) : decoded;
  return absolutePathMention(path, fragment);
}

/**
 * Decodes percent escapes in a path, leaving separator escapes (`%2F`, `%5C`)
 * encoded so decoding cannot change which directories the path traverses.
 * Invalid sequences and non-UTF-8 results leave the input unchanged.
 */
export function decodePathEscapes(input: string): string {
  if (!input.includes("%")) return input;

  const bytes: number[] = [];
  const source = new TextEncoder().encode(input);
  let index = 0;
  while (index < source.length) {
    const high = hexDigit(source[index + 1]);
    const low = hexDigit(source[index + 2]);
    if (source[index] === 0x25 /* % */ && high !== null && low !== null) {
      const byte = (high << 4) | low;
      if (byte !== 0x2f /* / */ && byte !== 0x5c /* \ */) {
        bytes.push(byte);
        index += 3;
        continue;
      }
    }
    bytes.push(source[index]!);
    index += 1;
  }

  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(bytes));
  } catch {
    return input;
  }
}

function hexDigit(byte: number | undefined): number | null {
  if (byte === undefined) return null;
  if (byte >= 0x30 && byte <= 0x39) return byte - 0x30;
  if (byte >= 0x61 && byte <= 0x66) return byte - 0x61 + 10;
  if (byte >= 0x41 && byte <= 0x46) return byte - 0x41 + 10;
  return null;
}

/**
 * Converts Windows-compatible path spellings into a native Windows path.
 * Returns null when the input needs no changes.
 */
export function toNativeWindowsPath(path: string): string | null {
  const joinDrive = (drive: string, rest: string) =>
    `${drive.toUpperCase()}:\\${rest.replaceAll("/", "\\")}`;

  if (path.startsWith("/")) {
    const rest = path.slice(1);
    // URL-style path with a leading slash before the drive: `/C:/foo`.
    if (/^[A-Za-z]:[/\\]/.test(rest)) return joinDrive(rest[0]!, rest.slice(3));
    // MSYS/Git Bash style: `/c/foo`. Lowercase only, since that is what those
    // shells emit and uppercase risks misreading a real directory.
    if (/^[a-z][/\\]/.test(rest)) return joinDrive(rest[0]!, rest.slice(2));
  }

  if (hasDrivePrefix(path)) {
    const drive = path[0]!;
    if (drive === drive.toUpperCase() && !path.includes("/")) return null;
    return `${drive.toUpperCase()}:${path.slice(2).replaceAll("/", "\\")}`;
  }

  if (path.includes("/")) return path.replaceAll("/", "\\");
  return null;
}

/**
 * A label for a mention. Total over every variant, so a chip always has
 * something to show and never falls back to `[resource_link]`.
 */
export function mentionName(mention: MentionUri): string {
  switch (mention.variant) {
    case "file":
    case "directory":
      return baseName(mention.absPath);
    case "pastedImage":
    case "symbol":
    case "thread":
    case "rule":
    case "skill":
      return mention.name;
    case "diagnostics":
      return "Diagnostics";
    case "terminalSelection":
      return mention.lineCount === 1 ? "Terminal (1 line)" : `Terminal (${mention.lineCount} lines)`;
    case "gitDiff":
      return `Branch Diff (${mention.baseRef})`;
    case "mergeConflict":
      return `Merge Conflict (${baseName(mention.filePath)})`;
    case "selection":
      return selectionName(mention.absPath, mention.lineRange);
    case "fetch":
      return mention.url;
  }
}

export function selectionName(path: string | null, lineRange: LineRange): string {
  const name = path === null ? "Untitled" : baseName(path);
  return `${name} (${lineRange[0] + 1}:${lineRange[1] + 1})`;
}

/**
 * Formats a 0-based inclusive range as a 1-based path suffix: `:5`, or `:5-9`.
 */
export function lineRangeSuffix(lineRange: LineRange): string {
  const start = lineRange[0] + 1;
  const end = lineRange[1] + 1;
  return start === end ? `:${start}` : `:${start}-${end}`;
}

/** Rust's `Path::file_name`, for both separators. */
function baseName(path: string): string {
  const trimmed = path.replace(/[/\\]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
}

/** Something a mention chip can hover over, or null when there is nothing more to say. */
export function mentionTooltip(mention: MentionUri): string | null {
  switch (mention.variant) {
    case "file":
    case "directory":
      return mention.absPath;
    case "symbol":
      return `${mention.absPath}:${mention.lineRange[0]}-${mention.lineRange[1]}`;
    case "selection":
      return mention.absPath === null
        ? null
        : `${mention.absPath}:${mention.lineRange[0]}-${mention.lineRange[1]}`;
    case "skill":
      return mention.skillFilePath;
    default:
      return null;
  }
}

/**
 * The URL path percent-encode set, as Rust's `Url::set_path` applies it:
 * C0 controls, everything above `~`, and ` "#<>?` plus backtick and braces.
 *
 * `%` is deliberately absent — an already-escaped path passes through
 * untouched, which is what makes `to_uri` idempotent on both sides.
 */
function encodePath(path: string): string {
  const escaped = ' "#<>?`{}';
  let out = "";
  for (const byte of new TextEncoder().encode(path)) {
    const char = String.fromCharCode(byte);
    if (byte < 0x20 || byte > 0x7e || escaped.includes(char)) {
      out += `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
    } else {
      out += char;
    }
  }
  return out.startsWith("/") ? out : `/${out}`;
}

/** Form-encodes query pairs the way Rust's `query_pairs_mut` does. */
function query(pairs: [string, string][]): string {
  if (pairs.length === 0) return "";
  const params = new URLSearchParams();
  for (const [key, value] of pairs) params.append(key, value);
  return `?${params.toString()}`;
}

function lineFragment(lineRange: LineRange): string {
  return `#L${lineRange[0] + 1}:${lineRange[1] + 1}`;
}

/** The canonical URI for a mention — what goes in a `resource_link`. */
export function mentionToUri(mention: MentionUri): string {
  switch (mention.variant) {
    case "file":
      return `file://${encodePath(mention.absPath)}`;
    case "directory": {
      const path = /[/\\]$/.test(mention.absPath) ? mention.absPath : `${mention.absPath}/`;
      return `file://${encodePath(path)}`;
    }
    case "symbol":
      return (
        `file://${encodePath(mention.absPath)}` +
        query([["symbol", mention.name]]) +
        lineFragment(mention.lineRange)
      );
    case "selection": {
      const base =
        mention.absPath === null
          ? "zed:///agent/untitled-buffer"
          : `file://${encodePath(mention.absPath)}`;
      const params: [string, string][] =
        mention.column === null ? [] : [["column", String(mention.column + 1)]];
      return base + query(params) + lineFragment(mention.lineRange);
    }
    case "pastedImage":
      return `zed:///agent/pasted-image${query([["name", mention.name]])}`;
    case "thread":
      return `zed:///agent/thread/${mention.id}${query([["name", mention.name]])}`;
    case "rule": {
      const uuid = ruleUuid(mention.id);
      return `zed:///agent/rule/${uuid}${query([["name", mention.name]])}`;
    }
    case "diagnostics": {
      const params: [string, string][] = [];
      if (mention.includeWarnings) params.push(["include_warnings", "true"]);
      if (!mention.includeErrors) params.push(["include_errors", "false"]);
      return `zed:///agent/diagnostics${query(params)}`;
    }
    case "fetch":
      return mention.url;
    case "terminalSelection":
      return `zed:///agent/terminal-selection${query([["lines", String(mention.lineCount)]])}`;
    case "gitDiff":
      return `zed:///agent/git-diff${query([["base", mention.baseRef]])}`;
    case "mergeConflict":
      return `zed:///agent/merge-conflict${query([["path", mention.filePath]])}`;
    case "skill":
      return `zed:///agent/skill${query([
        ["name", mention.name],
        ["source", mention.source],
        ["path", mention.skillFilePath],
      ])}`;
  }
}

function ruleUuid(id: unknown): string {
  if (typeof id === "object" && id !== null && "User" in id) {
    const user = (id as { User: unknown }).User;
    if (typeof user === "object" && user !== null && "uuid" in user) {
      const uuid = (user as { uuid: unknown }).uuid;
      if (typeof uuid === "string") return uuid;
    }
  }
  return "";
}

/** A mention written the way it appears in Markdown: `[@name](uri)`. */
export function mentionLink(mention: MentionUri): string {
  return `[@${mentionName(mention)}](${mentionToUri(mention)})`;
}
