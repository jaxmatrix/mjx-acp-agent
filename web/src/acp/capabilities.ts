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

/** One way an agent will accept being authenticated. */
export interface AuthMethod {
  /** The `methodId` to pass to `authenticate`. */
  id: string;
  name: string;
  description?: string;
  /** `envVar`, `terminal`, or `agent` for a method the agent handles itself. */
  kind: "envVar" | "terminal" | "agent";
  /** For an `envVar` method: the variables it wants. */
  vars: AuthEnvVar[];
  /** Documentation the agent pointed at. */
  link?: string;
}

/** One variable an `envVar` method wants. */
export interface AuthEnvVar {
  name: string;
  label?: string;
  /** Whether the login works without it. */
  optional: boolean;
}

/**
 * Reads `authMethods` off an `initialize` response.
 *
 * A sibling of {@link agentCapabilitiesOf} rather than part of it, because
 * `authMethods` sits at the top of the result and not under `agentCapabilities`.
 *
 * Defensive in the same way as everything else here, and in one way particular
 * to this shape: the schema says a method with no `type` *is* an `agent` method,
 * so an unfamiliar `type` is read the same way rather than dropped. Dropping it
 * would hide a choice the agent really offered; showing it as "the agent handles
 * this itself" is true of every method we cannot classify. An entry that is not
 * an object at all, or has no usable `id`, is skipped — it names nothing we
 * could pass to `authenticate`.
 */
export function authMethodsOf(response: unknown): AuthMethod[] {
  const advertised = record(response).authMethods;
  if (!Array.isArray(advertised)) return [];

  const methods: AuthMethod[] = [];
  for (const entry of advertised) {
    const method = record(entry);
    const id = text(method.id);
    const name = text(method.name);
    if (!id) continue;

    const kind =
      method.type === "env_var" ? "envVar" : method.type === "terminal" ? "terminal" : "agent";
    methods.push({
      id,
      // A method with no name is still selectable; its id is the honest label.
      name: name ?? id,
      description: text(method.description),
      kind,
      vars: kind === "envVar" ? envVars(method.vars) : [],
      link: text(method.link),
    });
  }
  return methods;
}

function envVars(value: unknown): AuthEnvVar[] {
  if (!Array.isArray(value)) return [];
  const vars: AuthEnvVar[] = [];
  for (const entry of value) {
    const variable = record(entry);
    const name = text(variable.name);
    if (!name) continue;
    // `optional` defaults to false, per the schema: a variable is required
    // unless it says otherwise, and guessing the other way would tell the user
    // they can skip something they cannot.
    vars.push({ name, label: text(variable.label), optional: variable.optional === true });
  }
  return vars;
}

/** A non-empty string, or `undefined` for anything else. */
function text(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/** Whether a capability object says "supported". */
function offered(capability: unknown): boolean {
  return typeof capability === "object" && capability !== null && !Array.isArray(capability);
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
}
