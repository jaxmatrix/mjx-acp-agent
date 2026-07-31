/**
 * A live terminal, rendered with xterm.js.
 *
 * The bytes arrive over `_mjx/terminal/output` exactly as the PTY produced
 * them, escape sequences included, so a real emulator is the only thing that
 * renders them correctly — colour, progress bars and cursor movement all
 * depend on it.
 */

import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

import type { Terminal } from "../acp/types";

export function TerminalView({
  terminal,
  onInput,
  onResize,
}: {
  terminal: Terminal;
  /**
   * Where keystrokes go, for a terminal that takes them.
   *
   * Absent for every terminal the *agent* started, which is all of them bar a
   * login this server opened: the agent owns those processes and ACP has no
   * notion of a client typing into one. The server refuses the write either
   * way; this is so the cursor does not invite it.
   */
  onInput?(bytes: Uint8Array): void;
  /** Told the size the browser is showing, so a prompt lays out correctly. */
  onResize?(rows: number, cols: number): void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const xterm = useRef<XTerm>(null);
  const fit = useRef<FitAddon>(null);
  /** How many chunks have been written, so re-renders only write new ones. */
  const written = useRef(0);

  useEffect(() => {
    if (!host.current) return;

    const term = new XTerm({
      convertEol: true,
      fontSize: 12,
      fontFamily: 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, monospace',
      theme: { background: "#0c0e13", foreground: "#d8dce5" },
      // Read-only unless something is listening. See `onInput`.
      disableStdin: !onInput,
      scrollback: 5000,
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(host.current);
    fitAddon.fit();

    xterm.current = term;
    fit.current = fitAddon;
    written.current = 0;

    // The far end has to be told, or a login prompt laid out for the PTY's
    // default 120 columns wraps into nonsense in a narrower pane.
    const disposeInput = onInput
      ? term.onData((data) => onInput(new TextEncoder().encode(data)))
      : undefined;

    const resize = new ResizeObserver(() => {
      try {
        fitAddon.fit();
        onResize?.(term.rows, term.cols);
      } catch {
        // Fitting a detached or zero-sized element throws; nothing to do.
      }
    });
    resize.observe(host.current);

    return () => {
      resize.disconnect();
      disposeInput?.dispose();
      term.dispose();
      xterm.current = null;
    };
    // The callbacks are read once, when the emulator is built. Re-running this
    // would throw away the scrollback on every parent render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Write only what has arrived since the last render. Rewriting the whole
  // buffer each time would fight the scrollback and flicker.
  useEffect(() => {
    const term = xterm.current;
    if (!term) return;
    for (let i = written.current; i < terminal.output.length; i += 1) {
      const chunk = terminal.output[i];
      if (chunk) term.write(chunk);
    }
    written.current = terminal.output.length;
  }, [terminal.output]);

  const exited = terminal.exitCode != null || terminal.signal != null;

  return (
    <figure className="terminal">
      <figcaption className="terminal__header">
        <code>
          {terminal.command} {terminal.args.join(" ")}
        </code>
        {terminal.truncated && (
          <span className="pill pill--warn" title="Older output was discarded">
            truncated
          </span>
        )}
        {exited && (
          <span className={`pill pill--${terminal.exitCode === 0 ? "completed" : "failed"}`}>
            {terminal.signal ? terminal.signal : `exit ${terminal.exitCode}`}
          </span>
        )}
        {!exited && <span className="pill pill--in_progress">running</span>}
      </figcaption>
      <div className="terminal__screen" ref={host} />
    </figure>
  );
}
