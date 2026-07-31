import { describe, expect, test } from "vitest";

import { agentCapabilitiesOf, authMethodsOf, noCapabilities } from "./capabilities";

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

describe("authMethodsOf", () => {
  test("reads every shape the schema defines", () => {
    const methods = authMethodsOf({
      authMethods: [
        // No `type` at all, which the schema reads as `agent`.
        { id: "own", name: "Sign in", description: "The agent does this itself." },
        {
          type: "env_var",
          id: "api-key",
          name: "API key",
          vars: [
            { name: "OPENAI_API_KEY", label: "API key" },
            { name: "OPENAI_ORG", optional: true },
          ],
          link: "https://example.test/keys",
        },
        { type: "terminal", id: "login", name: "Log in", args: ["--login"] },
      ],
    });

    expect(methods.map((m) => m.kind)).toEqual(["agent", "envVar", "terminal"]);
    const [, env, terminal] = methods;
    expect(env?.vars.map((v) => v.name)).toEqual(["OPENAI_API_KEY", "OPENAI_ORG"]);
    // Required unless it says otherwise, per the schema. Guessing the other way
    // would tell the user they can skip something they cannot.
    expect(env?.vars[0]?.optional).toBe(false);
    expect(env?.vars[1]?.optional).toBe(true);
    expect(env?.link).toBe("https://example.test/keys");
    // A `terminal` method's `args` are the server's business; the browser never
    // runs anything, so there is nothing here to carry them.
    expect(terminal?.vars).toEqual([]);
  });

  test("reads a type it does not know as the agent's own business", () => {
    // Dropping it would hide a choice the agent really offered. "The agent
    // handles this itself" is true of every method we cannot classify, and
    // `authenticate` with its id is still the right thing to send.
    const [method] = authMethodsOf({
      authMethods: [{ type: "something-new", id: "future", name: "Future" }],
    });
    expect(method?.kind).toBe("agent");
    expect(method?.id).toBe("future");
  });

  test("skips an entry that names nothing we could authenticate with", () => {
    const methods = authMethodsOf({
      authMethods: [
        { name: "no id" },
        { id: "", name: "blank id" },
        7,
        null,
        { id: "usable", name: "Usable" },
      ],
    });
    // One bad entry must not cost the user the methods that do work.
    expect(methods.map((m) => m.id)).toEqual(["usable"]);
  });

  test("falls back to the id when an agent gives no name", () => {
    const [method] = authMethodsOf({ authMethods: [{ id: "unnamed" }] });
    expect(method?.name).toBe("unnamed");
  });

  test("survives anything at all coming back", () => {
    for (const nonsense of [undefined, null, 7, "hello", [], { authMethods: 3 }]) {
      expect(authMethodsOf(nonsense)).toEqual([]);
    }
  });

  test("an agent that needs nothing offers nothing", () => {
    // The common case, and the one that must not produce a panel.
    expect(authMethodsOf({ authMethods: [] })).toEqual([]);
  });
});
