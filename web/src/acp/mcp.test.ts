import { describe, expect, it } from "vitest";

import { mcpServersOf } from "./mcp";

describe("mcpServersOf", () => {
  it("reads what the server sent", () => {
    const servers = mcpServersOf([
      {
        name: "git",
        transport: "stdio",
        target: "npx -y @modelcontextprotocol/server-git",
        secrets: ["GITHUB_TOKEN"],
        unavailable: null,
      },
      {
        name: "docs",
        transport: "http",
        target: "https://example.test/mcp",
        secrets: [],
        unavailable: "this agent did not declare the `http` MCP transport",
      },
    ]);

    expect(servers).toHaveLength(2);
    expect(servers[0]).toEqual({
      name: "git",
      transport: "stdio",
      target: "npx -y @modelcontextprotocol/server-git",
      secrets: ["GITHUB_TOKEN"],
      unavailable: undefined,
    });
    // A reason is a string when there is one, and absent when there is not —
    // `null` from Rust's `Option` must not become a truthy "unavailable".
    expect(servers[1]?.unavailable).toContain("http");
  });

  it("survives anything, because this arrives off a socket", () => {
    // The `_mjx/agent/info` payload is cast, not validated, so a shape we did
    // not expect must cost a missing sidebar section and not the page.
    expect(mcpServersOf(undefined)).toEqual([]);
    expect(mcpServersOf("all of them")).toEqual([]);
    expect(mcpServersOf([null, 7, "git"])).toEqual([]);

    // Anonymous is the one entry there is nothing to say about, including which
    // server it is.
    expect(mcpServersOf([{ transport: "stdio" }])).toEqual([]);

    const [salvaged] = mcpServersOf([{ name: "git", secrets: [1, "TOKEN"] }]);
    expect(salvaged).toEqual({
      name: "git",
      // Stdio is what an agent must support, so it is the safe assumption when
      // the transport is missing.
      transport: "stdio",
      target: "",
      secrets: ["TOKEN"],
      unavailable: undefined,
    });
  });
});
