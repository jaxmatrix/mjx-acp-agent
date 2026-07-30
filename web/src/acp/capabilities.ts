/**
 * What the agent said it can do, read out of the `initialize` response.
 *
 * The session lifecycle varies more between agents than anything else in ACP:
 * `claude-acp` and `kilo` list, load, fork, resume, delete and close; most of
 * the registry does none of it. A UI that offered all six regardless would be
 * wrong for nearly every agent, so everything the history drawer shows is
 * decided here.
 *
 * The payload is untrusted input, not a typed value: it comes off a socket, and
 * an unfamiliar shape should cost us a button rather than the page.
 */

/** The session methods an agent may offer. */
export interface SessionCapabilities {
  list: boolean;
  delete: boolean;
  fork: boolean;
  resume: boolean;
  close: boolean;
}

/** What the connected agent supports. */
export interface AgentCapabilities {
  /** Whether `session/load` may be called at all. */
  loadSession: boolean;
  session: SessionCapabilities;
}

/** An agent that has told us nothing, which is what to assume before it does. */
export function noCapabilities(): AgentCapabilities {
  return {
    loadSession: false,
    session: { list: false, delete: false, fork: false, resume: false, close: false },
  };
}

/**
 * Reads an `initialize` response.
 *
 * Two different spellings, because the protocol has two. `loadSession` is a
 * plain boolean at the top of `agentCapabilities` — the spec notes this is the
 * one that has not been unified yet — while each session capability is an
 * *object*: `{}` means supported, absent or `null` means not. A bare `true`
 * therefore reads as unsupported, the same rule the elicitation capabilities
 * already follow. That looks harsh, and it is what the schema says: a lenient
 * reading here would have us call a method the agent never claimed.
 */
export function agentCapabilitiesOf(response: unknown): AgentCapabilities {
  const capabilities = record(record(response).agentCapabilities);
  const session = record(capabilities.sessionCapabilities);

  return {
    loadSession: capabilities.loadSession === true,
    session: {
      list: offered(session.list),
      delete: offered(session.delete),
      fork: offered(session.fork),
      resume: offered(session.resume),
      close: offered(session.close),
    },
  };
}

/** Whether a capability object says "supported". */
function offered(capability: unknown): boolean {
  return typeof capability === "object" && capability !== null && !Array.isArray(capability);
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}
