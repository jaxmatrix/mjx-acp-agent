/**
 * The session config selectors — the model, the thinking level, and whatever
 * else the agent chooses to expose.
 *
 * Its own file rather than another block in `Sidebar`, because unlike the mode
 * selector this has to handle two control types and two shapes of option list.
 *
 * `category` is a UX hint the spec says must not be required for correctness,
 * so it only decides grouping and order here. An option with no category, or
 * one we have never heard of, still renders.
 */

import { selectShape, type SessionConfigOption } from "../acp/types";

/** Categories we have a heading for, in the order they should appear. */
const HEADINGS: ReadonlyArray<readonly [string, string]> = [
  ["model", "Model"],
  ["model_config", "Model settings"],
  ["thought_level", "Thinking"],
];

const OTHER = "Session";

type SelectOption = Extract<SessionConfigOption, { type: "select" }>;

export function ConfigOptions({
  options,
  onSetConfigOption,
}: {
  options: SessionConfigOption[];
  onSetConfigOption(configId: string, value: string | boolean): void;
}) {
  if (options.length === 0) return null;

  return (
    <>
      {groupByCategory(options).map(([heading, group]) => (
        <section className="sidebar__section" key={heading}>
          <h2>{heading}</h2>
          {group.map((option) => (
            <Control key={option.id} option={option} onSet={onSetConfigOption} />
          ))}
        </section>
      ))}
    </>
  );
}

function Control({
  option,
  onSet,
}: {
  option: SessionConfigOption;
  onSet(configId: string, value: string | boolean): void;
}) {
  if (option.type === "boolean") {
    return (
      <label className="config-option config-option--toggle">
        <input
          type="checkbox"
          checked={option.currentValue}
          onChange={(event) => onSet(option.id, event.target.checked)}
        />
        <span>{option.name}</span>
        {option.description && <span className="dim"> {option.description}</span>}
      </label>
    );
  }

  const shape = selectShape(option.options);
  return (
    <div className="config-option">
      <label className="dim" htmlFor={`config-${option.id}`}>
        {option.name}
      </label>
      <select
        id={`config-${option.id}`}
        value={option.currentValue}
        onChange={(event) => onSet(option.id, event.target.value)}
      >
        {shape.grouped
          ? shape.groups.map((group) => (
              <optgroup key={group.group} label={group.name}>
                {group.options.map((value) => (
                  <option key={value.value} value={value.value}>
                    {value.name}
                  </option>
                ))}
              </optgroup>
            ))
          : shape.values.map((value) => (
              <option key={value.value} value={value.value}>
                {value.name}
              </option>
            ))}
      </select>
      <p className="dim">{describe(option) ?? option.description ?? ""}</p>
    </div>
  );
}

/** The description of whichever value is currently selected, if it has one. */
function describe(option: SelectOption): string | undefined {
  const shape = selectShape(option.options);
  const values = shape.grouped ? shape.groups.flatMap((group) => group.options) : shape.values;
  return values.find((value) => value.value === option.currentValue)?.description ?? undefined;
}

/**
 * Buckets options under their heading, known categories first and in a fixed
 * order so the model selector does not move around between agents.
 */
function groupByCategory(
  options: SessionConfigOption[],
): Array<[string, SessionConfigOption[]]> {
  const buckets = new Map<string, SessionConfigOption[]>();
  for (const option of options) {
    const heading = HEADINGS.find(([category]) => category === option.category)?.[1] ?? OTHER;
    const bucket = buckets.get(heading);
    if (bucket) bucket.push(option);
    else buckets.set(heading, [option]);
  }

  const order = [...HEADINGS.map(([, heading]) => heading), OTHER];
  return [...buckets.entries()].sort(
    ([a], [b]) => order.indexOf(a) - order.indexOf(b),
  );
}
