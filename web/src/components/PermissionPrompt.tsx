/**
 * The authorization prompt.
 *
 * The agent's turn is suspended until one of these is clicked — ACP models
 * permission as an ordinary request, so the response *is* the answer. Grouped
 * by `kind` so the destructive option never sits where the safe one was a
 * moment ago.
 */

import type { PermissionOption } from "@agentclientprotocol/sdk";

/** Allow options first, then rejections; stable within each group. */
const ORDER: Record<PermissionOption["kind"], number> = {
  allow_once: 0,
  allow_always: 1,
  reject_once: 2,
  reject_always: 3,
};

export function PermissionPrompt({
  options,
  onChoose,
}: {
  options: PermissionOption[];
  onChoose(optionId: string | null): void;
}) {
  const sorted = [...options].sort((a, b) => (ORDER[a.kind] ?? 9) - (ORDER[b.kind] ?? 9));

  return (
    <div className="permission" role="group" aria-label="Permission required">
      <p className="permission__question">This needs your permission.</p>
      <div className="permission__options">
        {sorted.map((option) => (
          <button
            key={option.optionId}
            type="button"
            className={`permission__button permission__button--${option.kind}`}
            data-testid="permission-option"
            data-kind={option.kind}
            // The label is truncated when an agent puts a whole shell command
            // in it, so the full text has to be reachable somewhere.
            title={option.name}
            onClick={() => onChoose(option.optionId)}
          >
            {option.name}
          </button>
        ))}
        {/* Always available: an agent must never be able to trap the user in a
            choice between two things they don't want. */}
        <button
          type="button"
          className="permission__button permission__button--dismiss"
          onClick={() => onChoose(null)}
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
