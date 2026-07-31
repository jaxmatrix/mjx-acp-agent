/**
 * The conversations that are open, and which one is on screen.
 *
 * Every tab here is live whether or not it is the one being drawn — a
 * background conversation keeps streaming, keeps its place in a turn, and keeps
 * any question it is waiting on. So the strip has to say more than which one is
 * selected: the dot is how a user knows an agent they are not watching is still
 * working.
 */

import { tabKey, type Tab } from "../useSessions";

export function TabStrip({
  tabs,
  focused,
  nameOf,
  agentNameOf,
  busy,
  onFocus,
  onClose,
  onAdd,
}: {
  tabs: Tab[];
  focused?: Tab;
  nameOf(tab: Tab): string;
  agentNameOf(tab: Tab): string;
  busy(tab: Tab): boolean;
  onFocus(tab: Tab): void;
  onClose(tab: Tab): void;
  onAdd(): void;
}) {
  // A tab is named after its conversation, so which agent it is on needs
  // saying — but only when there is another agent to confuse it with. The same
  // for the directory. Either would otherwise be the same word on every tab.
  const showAgent = new Set(tabs.map((tab) => tab.agentId)).size > 1;
  const showCwd = new Set(tabs.map((tab) => tab.cwd)).size > 1;
  const sublabel = (tab: Tab) =>
    [showAgent ? agentNameOf(tab) : "", showCwd ? basename(tab.cwd) : ""]
      .filter(Boolean)
      .join(" · ");
  const on = focused && tabKey(focused);

  return (
    <nav className="tabs" aria-label="Open conversations">
      {tabs.map((tab) => {
        const key = tabKey(tab);
        const current = key === on;
        return (
          <span key={key} className={`tab ${current ? "tab--on" : ""}`}>
            <button
              type="button"
              className="tab__pick"
              aria-current={current ? "page" : undefined}
              onClick={() => onFocus(tab)}
              title={`${nameOf(tab)} — ${agentNameOf(tab)} in ${tab.cwd}`}
            >
              {busy(tab) && (
                // Not hidden from screen readers: that an agent nobody is
                // watching is still working is the whole point of the strip.
                <span className="tab__busy" role="img" aria-label="working">
                  ●
                </span>
              )}
              <span className="tab__name">{nameOf(tab)}</span>
              {sublabel(tab) && <span className="tab__cwd dim">{sublabel(tab)}</span>}
            </button>
            <button
              type="button"
              className="tab__close"
              // The agent keeps the conversation — this puts it away rather
              // than ending it, and the history is where it comes back from.
              aria-label={`Close ${nameOf(tab)}`}
              onClick={() => onClose(tab)}
            >
              ×
            </button>
          </span>
        );
      })}
      <button type="button" className="tab__add" aria-label="Connect another agent" onClick={onAdd}>
        +
      </button>
    </nav>
  );
}

/** The last segment of a path, for a label with no room for the rest. */
export function basename(cwd: string): string {
  const trimmed = cwd.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || trimmed;
}
