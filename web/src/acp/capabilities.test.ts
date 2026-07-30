import { describe, expect, test } from "vitest";

import { agentCapabilitiesOf, noCapabilities } from "./capabilities";

describe("reading what an agent can do", () => {
  test("takes the session lifecycle it advertises", () => {
    const capabilities = agentCapabilitiesOf({
      protocolVersion: 1,
      agentCapabilities: {
        loadSession: true,
        sessionCapabilities: { list: {}, delete: {}, fork: {}, resume: {}, close: {} },
      },
    });

    expect(capabilities.loadSession).toBe(true);
    expect(capabilities.session).toEqual({
      list: true,
      delete: true,
      fork: true,
      resume: true,
      close: true,
    });
  });

  test("offers only what was named", () => {
    // The common case: an agent that can list and load, and nothing else.
    const capabilities = agentCapabilitiesOf({
      agentCapabilities: { loadSession: true, sessionCapabilities: { list: {} } },
    });

    expect(capabilities.session.list).toBe(true);
    expect(capabilities.session.fork).toBe(false);
    expect(capabilities.session.delete).toBe(false);
  });

  test("reads a bare `true` as unsupported", () => {
    // The schema types these as objects with a lenient deserializer, so an
    // agent that writes `true` has told the protocol nothing — and calling a
    // method on the strength of it would fail on the agent's side.
    const capabilities = agentCapabilitiesOf({
      agentCapabilities: { sessionCapabilities: { list: true, delete: null, fork: [] } },
    });

    expect(capabilities.session.list).toBe(false);
    expect(capabilities.session.delete).toBe(false);
    expect(capabilities.session.fork).toBe(false);
  });

  test("survives anything at all coming back", () => {
    for (const nonsense of [undefined, null, 7, "hello", [], { agentCapabilities: 3 }]) {
      expect(agentCapabilitiesOf(nonsense)).toEqual(noCapabilities());
    }
  });

  test("an agent that has said nothing is offered nothing", () => {
    expect(noCapabilities().loadSession).toBe(false);
    expect(Object.values(noCapabilities().session).every((v) => v === false)).toBe(true);
  });
});
