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

export function TerminalView({ terminal }: { terminal: Terminal }) {
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
      // Read-only: input would have nowhere to go, since the agent owns this
      // process and ACP has no way for a client to type into it.
      disableStdin: true,
      scrollback: 5000,
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(host.current);
    fitAddon.fit();

    xterm.current = term;
    fit.current = fitAddon;
    written.current = 0;

    const resize = new ResizeObserver(() => {
      try {
        fitAddon.fit();
      } catch {
        // Fitting a detached or zero-sized element throws; nothing to do.
      }
    });
    resize.observe(host.current);

    return () => {
      resize.disconnect();
      term.dispose();
      xterm.current = null;
    };
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
      <div className="terminal__screen" data-testid="terminal" ref={host} />
    </figure>
  );
}
