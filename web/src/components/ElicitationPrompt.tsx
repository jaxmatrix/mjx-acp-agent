/**
 * A structured question the agent asked, and the form to answer it with.
 *
 * Not a permission prompt: the answer is data rather than a choice between
 * buttons, so the schema decides what controls appear. The turn is suspended
 * until one of Accept, Decline or Cancel is pressed — those three are the whole
 * of `CreateElicitationResponse`.
 *
 * Uncontrolled inputs would be simpler, but the Accept button has to know
 * whether the required fields are filled in, so the values live in state.
 */

import { useState } from "react";
import {
  ElicitationPropertySchema,
  MultiSelectItems,
  type ElicitationSchema,
  type EnumOption,
} from "@agentclientprotocol/sdk";

import type { Elicitation, ElicitationAnswer, ElicitationValue } from "../acp/types";

export function ElicitationPrompt({
  elicitation,
  onAnswer,
}: {
  elicitation: Elicitation;
  onAnswer(requestId: string | number, answer: ElicitationAnswer): void;
}) {
  const settled = elicitation.state !== "pending";

  return (
    <section className="elicitation" aria-label="The agent needs some input">
      <p className="elicitation__message">{elicitation.message}</p>
      {elicitation.toolCallId && (
        <p className="dim elicitation__origin">for {elicitation.toolCallId}</p>
      )}

      {settled ? (
        <Settled elicitation={elicitation} />
      ) : elicitation.mode.mode === "url" ? (
        <UrlMode url={elicitation.mode.url} onAnswer={(a) => onAnswer(elicitation.requestId, a)} />
      ) : (
        <FormMode
          schema={elicitation.mode.requestedSchema}
          onAnswer={(a) => onAnswer(elicitation.requestId, a)}
        />
      )}
    </section>
  );
}

/**
 * What is left once the question is over.
 *
 * Kept on screen rather than removed: the exchange is part of the conversation,
 * and an agent's later message often only makes sense next to the answer it was
 * given. This is also what a reload shows, because the answer is thread state.
 */
function Settled({ elicitation }: { elicitation: Elicitation }) {
  const entries = Object.entries(elicitation.content ?? {});

  return (
    <div className={`elicitation__settled elicitation__settled--${elicitation.state}`}>
      <p className="dim">{VERDICT[elicitation.state]}</p>
      {entries.length > 0 && (
        <dl className="elicitation__answers">
          {entries.map(([name, value]) => (
            <div key={name}>
              <dt>{name}</dt>
              <dd>{show(value)}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}

const VERDICT: Record<Elicitation["state"], string> = {
  pending: "Waiting for you.",
  accepted: "Answered.",
  declined: "You declined this.",
  cancelled: "Nobody answered this — the turn ended first.",
};

function show(value: ElicitationValue): string {
  return Array.isArray(value) ? value.join(", ") : String(value);
}

/**
 * The link mode: go somewhere, do something, come back.
 *
 * There is no Accept here, because the user pressing anything is not how this
 * ends — the agent says when it is finished, with `elicitation/complete`. Cancel
 * stays, so a link that leads nowhere is not a trap.
 */
function UrlMode({
  url,
  onAnswer,
}: {
  url: string;
  onAnswer(answer: ElicitationAnswer): void;
}) {
  return (
    <div className="elicitation__url">
      <a href={url} target="_blank" rel="noreferrer" className="elicitation__link">
        {url}
      </a>
      <p className="dim">This carries on by itself once you are done over there.</p>
      <div className="elicitation__actions">
        <button
          type="button"
          className="permission__button permission__button--dismiss"
          onClick={() => onAnswer({ action: "cancel" })}
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

function FormMode({
  schema,
  onAnswer,
}: {
  schema: ElicitationSchema;
  onAnswer(answer: ElicitationAnswer): void;
}) {
  const fields = Object.entries(schema.properties ?? {});
  const required = schema.required ?? [];
  const [values, setValues] = useState<Record<string, ElicitationValue>>(() => defaults(fields));

  const missing = required.filter((name) => isBlank(values[name]));

  function submit(event: React.FormEvent) {
    event.preventDefault();
    if (missing.length > 0) return;
    onAnswer({ action: "accept", content: values });
  }

  return (
    <form className="elicitation__form" data-testid="elicitation-form" onSubmit={submit}>
      {schema.description && <p className="dim">{schema.description}</p>}

      {fields.map(([name, property]) => (
        <Field
          key={name}
          name={name}
          property={property}
          required={required.includes(name)}
          value={values[name]}
          onChange={(value) => setValues((current) => ({ ...current, [name]: value }))}
        />
      ))}

      <div className="elicitation__actions">
        <button
          type="submit"
          className="permission__button permission__button--allow_once"
          data-testid="elicitation-send"
          disabled={missing.length > 0}
          // Saying which field is missing beats a button that just refuses.
          title={missing.length > 0 ? `Still needed: ${missing.join(", ")}` : undefined}
        >
          Send
        </button>
        <button
          type="button"
          className="permission__button permission__button--reject_once"
          onClick={() => onAnswer({ action: "decline" })}
        >
          Decline
        </button>
        {/* Distinct from declining, and the protocol keeps them distinct:
            declining is an answer, cancelling is refusing to give one. */}
        <button
          type="button"
          className="permission__button permission__button--dismiss"
          onClick={() => onAnswer({ action: "cancel" })}
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

function Field({
  name,
  property,
  required,
  value,
  onChange,
}: {
  name: string;
  property: ElicitationPropertySchema;
  required: boolean;
  value: ElicitationValue | undefined;
  onChange(value: ElicitationValue): void;
}) {
  const id = `elicit-${name}`;
  const label = title(property) ?? name;
  const description = describe(property);

  return (
    <div className="elicitation__field">
      {property.type === "boolean" ? (
        <label className="config-option config-option--toggle" htmlFor={id}>
          <input
            id={id}
            type="checkbox"
            checked={value === true}
            onChange={(event) => onChange(event.target.checked)}
          />
          <span>{label}</span>
        </label>
      ) : (
        <>
          <label className="dim" htmlFor={id}>
            {label}
            {required && <span aria-hidden="true"> *</span>}
          </label>
          <Input id={id} property={property} value={value} onChange={onChange} />
        </>
      )}
      {description && <p className="dim">{description}</p>}
    </div>
  );
}

function Input({
  id,
  property,
  value,
  onChange,
}: {
  id: string;
  property: ElicitationPropertySchema;
  value: ElicitationValue | undefined;
  onChange(value: ElicitationValue): void;
}) {
  // Narrowed with the SDK's own guards rather than by reading `type`. The union
  // ends in a catch-all for future variants, so a plain `switch` widens every
  // other member's fields to nothing.
  if (ElicitationPropertySchema.isString(property)) {
    const choices = options(property.oneOf, property.enum);
    if (choices) {
      return (
        <select id={id} value={String(value ?? "")} onChange={(e) => onChange(e.target.value)}>
          {/* An empty first entry, so a required field with no default does not
              look answered before it has been touched. */}
          <option value="">—</option>
          {choices.map((choice) => (
            <option key={choice.const} value={choice.const}>
              {choice.title}
            </option>
          ))}
        </select>
      );
    }
    return (
      <input
        id={id}
        // `format` is a hint the browser already knows how to honour: an email
        // field gets email validation and the right keyboard for free.
        type={INPUT_TYPES[property.format ?? ""] ?? "text"}
        value={String(value ?? "")}
        minLength={property.minLength ?? undefined}
        maxLength={property.maxLength ?? undefined}
        pattern={property.pattern ?? undefined}
        onChange={(event) => onChange(event.target.value)}
      />
    );
  }

  if (
    ElicitationPropertySchema.isNumber(property) ||
    ElicitationPropertySchema.isInteger(property)
  ) {
    return (
      <input
        id={id}
        type="number"
        // An integer field must not accept 1.5, and the step is what says so.
        step={property.type === "integer" ? 1 : "any"}
        value={value === undefined ? "" : String(value)}
        min={property.minimum ?? undefined}
        max={property.maximum ?? undefined}
        onChange={(event) => {
          const parsed = Number(event.target.value);
          // An unparseable box is empty, not zero. Sending 0 for "" would put a
          // number in front of the agent that the user never chose.
          onChange(event.target.value === "" || Number.isNaN(parsed) ? "" : parsed);
        }}
      />
    );
  }

  if (ElicitationPropertySchema.isArray(property)) {
    const choices = items(property.items);
    const selected = Array.isArray(value) ? value : [];
    const full = property.maxItems != null && selected.length >= property.maxItems;
    return (
      <div className="elicitation__checkboxes" role="group" aria-labelledby={id}>
        {choices.map((choice) => (
          <label key={choice.const} className="config-option config-option--toggle">
            <input
              type="checkbox"
              checked={selected.includes(choice.const)}
              disabled={full && !selected.includes(choice.const)}
              onChange={(event) =>
                onChange(
                  event.target.checked
                    ? [...selected, choice.const]
                    : selected.filter((v) => v !== choice.const),
                )
              }
            />
            <span>{choice.title}</span>
          </label>
        ))}
      </div>
    );
  }

  // A type from a newer protocol version. Saying so beats drawing a text box
  // that would send the wrong shape, and beats saying nothing at all.
  return (
    <p className="callout callout--warn">
      This viewer cannot render a “{property.type}” field.
    </p>
  );
}

/** The `format` values that map onto an input type the browser validates. */
const INPUT_TYPES: Record<string, string | undefined> = {
  email: "email",
  uri: "url",
  date: "date",
  "date-time": "datetime-local",
};

/**
 * A single-select's choices, whichever way they were spelled.
 *
 * `oneOf` carries titles and `enum` does not, so an untitled value is shown as
 * itself rather than as nothing.
 */
function options(
  oneOf: EnumOption[] | null | undefined,
  enumValues: string[] | null | undefined,
): EnumOption[] | null {
  if (oneOf?.length) return oneOf;
  if (enumValues?.length) return enumValues.map((value) => ({ const: value, title: value }));
  return null;
}

/** A multi-select's choices, from either spelling of its items. */
function items(spec: MultiSelectItems): EnumOption[] {
  if (MultiSelectItems.isTitled(spec)) return spec.anyOf;
  if (MultiSelectItems.isString(spec)) return spec.enum.map((v) => ({ const: v, title: v }));
  return [];
}

/** Every property variant has these, but the union has to be narrowed first. */
function title(property: ElicitationPropertySchema): string | undefined {
  return read(property, "title");
}

function describe(property: ElicitationPropertySchema): string | undefined {
  return read(property, "description");
}

function read(property: ElicitationPropertySchema, key: string): string | undefined {
  const value = (property as Record<string, unknown>)[key];
  return typeof value === "string" ? value : undefined;
}

/** The values a form starts with, from whatever defaults the schema names. */
function defaults(
  fields: Array<[string, ElicitationPropertySchema]>,
): Record<string, ElicitationValue> {
  const values: Record<string, ElicitationValue> = {};
  for (const [name, property] of fields) {
    const fallback = (property as { default?: unknown }).default;
    if (fallback !== undefined && fallback !== null) values[name] = fallback as ElicitationValue;
    else if (property.type === "boolean") values[name] = false;
    else if (property.type === "array") values[name] = [];
  }
  return values;
}

/**
 * Whether a required field still needs filling in.
 *
 * `false` and `0` are answers; an empty string and an empty list are not.
 */
function isBlank(value: ElicitationValue | undefined): boolean {
  if (value === undefined) return true;
  if (value === "") return true;
  return Array.isArray(value) && value.length === 0;
}
