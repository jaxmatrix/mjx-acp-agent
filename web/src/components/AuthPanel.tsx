/**
 * What to do about an agent that will not start until it is authenticated.
 *
 * This replaces `Connection failed: {"code":-32000,...}`. Nothing here is a
 * failure: the agent works, it has said how it will accept being let in, and
 * every one of those ways is on screen with what the server can do about it.
 *
 * No field takes a credential, and that is not an oversight. Credentials are
 * the operator's, they live in the environment the server was started in, and
 * the browser is the one place they must never travel — so the most this can do
 * is name a variable and say whether it is set.
 */

import { useState } from "react";

import type { Terminals } from "../acp/terminals";
import type { AuthMethodInfo, AuthState } from "../acp/types";
import { TerminalView } from "./TerminalView";

export function AuthPanel({
  agentName,
  auth,
  terminals,
  onAuthenticate,
  onTerminalInput,
  onTerminalResize,
}: {
  agentName: string;
  auth: AuthState;
  terminals: Terminals;
  /** Runs the method, and resolves with what to tell the user meanwhile. */
  onAuthenticate(methodId: string): Promise<string>;
  onTerminalInput(terminalId: string, bytes: Uint8Array): void;
  onTerminalResize(terminalId: string, rows: number, cols: number): void;
}) {
  const [busy, setBusy] = useState<string>();
  const [message, setMessage] = useState<string>();

  async function attempt(methodId: string) {
    setBusy(methodId);
    setMessage(undefined);
    try {
      setMessage(await onAuthenticate(methodId));
    } catch (cause: unknown) {
      setMessage(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(undefined);
    }
  }

  if (auth.authenticated) {
    return (
      <section className="auth">
        <p className="callout">{agentName} is authenticated. Start a conversation.</p>
      </section>
    );
  }

  return (
    <section className="auth">
      <h2 className="auth__title">{agentName} needs authenticating</h2>
      <p className="auth__lead">
        The agent started, and will not open a session until one of these has been used. These
        credentials are the server operator&rsquo;s: they come from the environment the server
        runs in and from <code>mjx.toml</code>, and never from this page.
      </p>

      {auth.methods.length === 0 && (
        // An agent can refuse without saying how to satisfy it. Saying so
        // plainly is still better than the raw error this replaces.
        <p className="callout callout--error">
          The agent refused but offered no way to authenticate. Check its own documentation.
        </p>
      )}

      <ul className="auth__methods">
        {auth.methods.map((method) => (
          <AuthMethodRow
            key={method.id}
            method={method}
            busy={busy === method.id}
            disabled={busy !== undefined}
            onAttempt={() => void attempt(method.id)}
          />
        ))}
      </ul>

      {message && <p className="callout">{message}</p>}

      {/* A login runs in a real PTY on the server, and needs a person at the
          keyboard: `claude login` prints a URL and then blocks on input. This
          is the only terminal in the viewer that takes any. */}
      {Object.values(terminals)
        .filter((terminal) => terminal.interactive)
        .map((terminal) => (
          <TerminalView
            key={terminal.id}
            terminal={terminal}
            onInput={(bytes) => onTerminalInput(terminal.id, bytes)}
            onResize={(rows, cols) => onTerminalResize(terminal.id, rows, cols)}
          />
        ))}
    </section>
  );
}

function AuthMethodRow({
  method,
  busy,
  disabled,
  onAttempt,
}: {
  method: AuthMethodInfo;
  busy: boolean;
  disabled: boolean;
  onAttempt(): void;
}) {
  const secrets = method.secrets ?? [];
  const declines = method.declines ?? [];

  return (
    <li className="auth__method">
      <div className="auth__method-head">
        <div>
          <strong>{method.name}</strong>
          {/* Which provider would take it, so an operator can tell a method
              that is configured from one that merely could be. */}
          {method.provider && <span className="auth__provider"> via {method.provider}</span>}
          {method.description && <p className="auth__description">{method.description}</p>}
        </div>
        <button type="button" onClick={onAttempt} disabled={disabled}>
          {busy ? "Working…" : method.kind === "terminal" ? "Log in" : "Use this"}
        </button>
      </div>

      {method.instructions && <p className="auth__instructions">{method.instructions}</p>}

      {secrets.length > 0 && (
        <ul className="auth__secrets">
          {secrets.map((secret) => (
            <li key={secret.name} className={secret.present ? "is-present" : "is-missing"}>
              <code>{secret.name}</code>
              {secret.label && <span className="auth__label"> — {secret.label}</span>}
              {/* Set or not, never the value. */}
              <span className="auth__status">
                {secret.present ? "set" : secret.optional ? "not set (optional)" : "not set"}
              </span>
            </li>
          ))}
        </ul>
      )}

      {method.link && (
        <p className="auth__link">
          <a href={method.link} target="_blank" rel="noreferrer noopener">
            Where to get this
          </a>
        </p>
      )}

      {declines.length > 0 && (
        // The whole chain, not just the winner. "No provider handled this" is
        // a support call; "the `anthropic` provider passed because
        // ANTHROPIC_API_KEY is not set" is an instruction.
        <details className="auth__declines">
          <summary>Why no provider took this</summary>
          <ul>
            {declines.map((decline) => (
              <li key={decline.provider}>
                <code>{decline.provider}</code>: {decline.reason}
              </li>
            ))}
          </ul>
        </details>
      )}
    </li>
  );
}
