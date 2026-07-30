import { describe, expect, test } from "vitest";

import { ago } from "./SessionHistory";

describe("how long ago a conversation was", () => {
  const now = Date.parse("2026-07-30T12:00:00Z");

  test("says it in the coarsest unit that still means something", () => {
    expect(ago("2026-07-30T11:59:40Z", now)).toBe("just now");
    expect(ago("2026-07-30T11:30:00Z", now)).toBe("30m ago");
    expect(ago("2026-07-30T09:00:00Z", now)).toBe("3h ago");
    expect(ago("2026-07-28T12:00:00Z", now)).toBe("2d ago");
  });

  test("falls back to a date once relative stops helping", () => {
    expect(ago("2025-01-02T12:00:00Z", now)).toBe(new Date("2025-01-02T12:00:00Z").toLocaleDateString());
  });

  test("shows an unreadable timestamp as itself", () => {
    // The timestamp is the agent's, and agents are not always careful with
    // them. "NaN minutes ago" tells the user nothing; the raw value at least
    // says where the problem is.
    expect(ago("last Tuesday", now)).toBe("last Tuesday");
  });

  test("a clock that is ahead does not report the future", () => {
    expect(ago("2026-07-30T12:05:00Z", now)).toBe("just now");
  });
});
