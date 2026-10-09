/**
 * Editors for each kind of setting control (toggle, select, number with unit,
 * byte threshold with unit select, path, glob list), plus the row that frames
 * one setting with its title, badges, description, default, modified state,
 * reset and validation message.
 */
import { useId, useState, type ReactNode } from "react";
import { Icon } from "../components/icons";
import { formatBytes, formatCount } from "../lib/format";
import {
  BADGES,
  BYTE_UNITS,
  bestByteUnit,
  highlightRuns,
  type ByteUnit,
  type SettingControl,
  type SettingDef,
} from "./registry";

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/** Text with the query words highlighted. */
export function Highlight({ text, words }: { text: string; words: readonly string[] }) {
  const runs = highlightRuns(text, words);
  return (
    <>
      {runs.map((r, i) =>
        r.hit ? (
          <mark key={i} className="hl">
            {r.text}
          </mark>
        ) : (
          r.text
        ),
      )}
    </>
  );
}

function trimNumber(n: number): string {
  return String(Number(n.toFixed(2)));
}

/**
 * Describes a value for display ("On", "10 GB", "1,000 ms").
 *
 * @param control - The setting's control.
 * @param value - Value to describe.
 */
export function describeValue(control: SettingControl, value: unknown): string {
  switch (control.kind) {
    case "toggle":
      return value ? "On" : "Off";
    case "select":
      return control.options.find((o) => o.value === value)?.label ?? String(value);
    case "number":
      if (value === 0 && control.zeroMeans) return control.zeroMeans;
      return typeof value === "number" ? `${control.integer ? formatCount(value) : trimNumber(value)} ${control.unit}` : String(value);
    case "bytes":
      return typeof value === "number" ? formatBytes(value) : String(value);
    case "path":
      return typeof value === "string" ? value : control.defaultLabel;
    case "globs":
      return Array.isArray(value) && value.length > 0 ? `${value.length} ${value.length === 1 ? "pattern" : "patterns"}` : "None";
  }
}

/** Shared props of every editor. */
interface EditorProps<T> {
  id: string;
  value: T;
  onChange: (value: T) => void;
  describedBy: string;
  invalid: boolean;
  disabled: boolean;
  label: string;
}

// -----------------------------------------------------------------------------
// Editors
// -----------------------------------------------------------------------------

function ToggleEditor({ id, value, onChange, describedBy, disabled }: EditorProps<boolean>) {
  return (
    <input
      id={id}
      type="checkbox"
      role="switch"
      className="switch"
      checked={value}
      disabled={disabled}
      aria-describedby={describedBy}
      onChange={(e) => {
        onChange(e.target.checked);
      }}
    />
  );
}

function SelectEditor({ id, value, onChange, describedBy, disabled, options }: EditorProps<string> & { options: readonly { value: string; label: string }[] }) {
  return (
    <select
      id={id}
      className="field-select"
      value={value}
      disabled={disabled}
      aria-describedby={describedBy}
      onChange={(e) => {
        onChange(e.target.value);
      }}
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

function NumberEditor({ id, value, onChange, describedBy, invalid, disabled, control }: EditorProps<number> & { control: Extract<SettingControl, { kind: "number" }> }) {
  const [text, setText] = useState(Number.isFinite(value) ? String(value) : "");
  return (
    <span className="field-num">
      <input
        id={id}
        type="number"
        inputMode={control.integer ? "numeric" : "decimal"}
        className="field-num__input"
        value={text}
        min={control.min}
        max={control.max}
        step={control.step}
        disabled={disabled}
        aria-invalid={invalid}
        aria-describedby={describedBy}
        onChange={(e) => {
          setText(e.target.value);
          const t = e.target.value.trim();
          onChange(t === "" ? Number.NaN : Number(t));
        }}
      />
      <span className="field-num__unit" aria-hidden="true">
        {control.unit}
      </span>
    </span>
  );
}

function BytesEditor({ id, value, onChange, describedBy, invalid, disabled, label, control }: EditorProps<number> & { control: Extract<SettingControl, { kind: "bytes" }> }) {
  const [unit, setUnit] = useState<ByteUnit>(() => bestByteUnit(value, control.units));
  const [text, setText] = useState(() => (Number.isFinite(value) ? trimNumber(value / BYTE_UNITS[unit]) : ""));
  const commit = (t: string, u: ByteUnit) => {
    const n = t.trim() === "" ? Number.NaN : Number(t);
    onChange(Number.isFinite(n) ? Math.round(n * BYTE_UNITS[u]) : Number.NaN);
  };
  return (
    <span className="field-num field-num--bytes">
      <input
        id={id}
        type="number"
        inputMode="decimal"
        className="field-num__input"
        value={text}
        min={0}
        step="any"
        disabled={disabled}
        aria-invalid={invalid}
        aria-describedby={describedBy}
        onChange={(e) => {
          setText(e.target.value);
          commit(e.target.value, unit);
        }}
      />
      <select
        className="field-num__select"
        aria-label={`${label} unit`}
        value={unit}
        disabled={disabled}
        onChange={(e) => {
          const u = e.target.value as ByteUnit;
          setUnit(u);
          commit(text, u);
        }}
      >
        {control.units.map((u) => (
          <option key={u} value={u}>
            {u}
          </option>
        ))}
      </select>
    </span>
  );
}

function PathEditor({ id, value, onChange, describedBy, invalid, disabled, control }: EditorProps<string | null> & { control: Extract<SettingControl, { kind: "path" }> }) {
  const name = useId();
  const [custom, setCustom] = useState(value ?? "");
  const isDefault = value === null;
  return (
    <fieldset className="field-path" aria-describedby={describedBy} disabled={disabled}>
      <legend className="visually-hidden">Location</legend>
      <label className="radio">
        <input
          type="radio"
          name={name}
          checked={isDefault}
          onChange={() => {
            onChange(null);
          }}
        />
        {control.defaultLabel}
      </label>
      <label className="radio">
        <input
          type="radio"
          name={name}
          checked={!isDefault}
          onChange={() => {
            onChange(custom);
          }}
        />
        Custom folder
      </label>
      <input
        id={id}
        type="text"
        className="field-text"
        aria-label="Custom folder path"
        placeholder={control.placeholder}
        spellCheck={false}
        value={isDefault ? custom : value}
        disabled={disabled || isDefault}
        aria-invalid={invalid}
        onChange={(e) => {
          setCustom(e.target.value);
          onChange(e.target.value);
        }}
      />
    </fieldset>
  );
}

function GlobsEditor({ id, value, onChange, describedBy, invalid, disabled, control }: EditorProps<readonly string[]> & { control: Extract<SettingControl, { kind: "globs" }> }) {
  const [draft, setDraft] = useState("");
  const add = () => {
    const t = draft.trim();
    if (t === "") return;
    onChange([...value, t]);
    setDraft("");
  };
  const dupes = new Set(value.filter((g, i) => value.indexOf(g) !== i));
  return (
    <div className="globs" aria-describedby={describedBy}>
      {value.length > 0 && (
        <ul className="globs__list" aria-label="Patterns">
          {value.map((g, i) => (
            <li key={i} className="globs__item">
              <input
                type="text"
                className="field-text field-text--mono"
                aria-label={`Pattern ${i + 1}`}
                spellCheck={false}
                value={g}
                disabled={disabled}
                aria-invalid={g.trim() === "" || invalid}
                onChange={(e) => {
                  onChange(value.map((x, j) => (j === i ? e.target.value : x)));
                }}
              />
              {dupes.has(g) && <span className="globs__note">Duplicate</span>}
              <button
                type="button"
                className="icon-btn icon-btn--sm"
                aria-label={`Remove pattern ${g || i + 1}`}
                disabled={disabled}
                onClick={() => {
                  onChange(value.filter((_, j) => j !== i));
                }}
              >
                <Icon name="close" size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="globs__add">
        <input
          id={id}
          type="text"
          className="field-text field-text--mono"
          aria-label="New pattern"
          placeholder={control.placeholder}
          spellCheck={false}
          value={draft}
          disabled={disabled}
          onChange={(e) => {
            setDraft(e.target.value);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              add();
            }
          }}
        />
        <button type="button" className="btn btn--sm" disabled={disabled || draft.trim() === "" || value.length >= control.maxItems} onClick={add}>
          <Icon name="plus" size={14} />
          Add pattern
        </button>
      </div>
      <p className="globs__count">
        {formatCount(value.length)} of {formatCount(control.maxItems)} patterns
      </p>
    </div>
  );
}

// -----------------------------------------------------------------------------
// Row
// -----------------------------------------------------------------------------

/** Props for {@link SettingRow}. */
export interface SettingRowProps {
  def: SettingDef;
  value: unknown;
  /** Validation message, or `null`. */
  error: string | null;
  modified: boolean;
  disabled: boolean;
  /** Lower-cased search words to highlight. */
  words: readonly string[];
  onChange: (value: unknown) => void;
  onReset: () => void;
}

/** One setting: title, badges, description, control, default, reset and error. */
export function SettingRow({ def, value, error, modified, disabled, words, onChange, onReset }: SettingRowProps) {
  const id = `setting-${def.key.replace(".", "-")}`;
  const descId = `${id}-desc`;
  const errId = `${id}-err`;
  const describedBy = error ? `${descId} ${errId}` : descId;
  const c = def.control;
  const common = { id: `${id}-input`, describedBy, invalid: error !== null, disabled, label: def.title };
  let editor: ReactNode;
  switch (c.kind) {
    case "toggle":
      editor = <ToggleEditor {...common} value={value === true} onChange={onChange} />;
      break;
    case "select":
      editor = <SelectEditor {...common} value={String(value)} options={c.options} onChange={onChange} />;
      break;
    case "number":
      editor = <NumberEditor {...common} value={value as number} control={c} onChange={onChange} />;
      break;
    case "bytes":
      editor = <BytesEditor {...common} value={value as number} control={c} onChange={onChange} />;
      break;
    case "path":
      editor = <PathEditor {...common} value={value as string | null} control={c} onChange={onChange} />;
      break;
    case "globs":
      editor = <GlobsEditor {...common} value={value as readonly string[]} control={c} onChange={onChange} />;
      break;
  }
  const wide = c.kind === "globs" || c.kind === "path";
  const range = c.kind === "number" ? `${c.integer ? formatCount(c.min) : trimNumber(c.min)}–${c.integer ? formatCount(c.max) : trimNumber(c.max)} ${c.unit}` : null;
  return (
    <div className={`setting${modified ? " is-modified" : ""}${error ? " has-error" : ""}${wide ? " setting--wide" : ""}`} data-key={def.key}>
      <div className="setting__text">
        <div className="setting__head">
          {c.kind === "path" || c.kind === "globs" ? (
            <h3 className="setting__title" id={`${id}-title`}>
              <Highlight text={def.title} words={words} />
            </h3>
          ) : (
            <label className="setting__title" htmlFor={common.id} id={`${id}-title`}>
              <Highlight text={def.title} words={words} />
            </label>
          )}
          {modified && <span className="visually-hidden">(modified)</span>}
          {def.badges.map((b) => (
            <span key={b} className={`sbadge-pill sbadge-pill--${b}`} data-tip={BADGES[b].hint}>
              {b === "admin" && <Icon name="shield" size={12} />}
              {BADGES[b].label}
            </span>
          ))}
        </div>
        <p className="setting__desc" id={descId}>
          <Highlight text={def.description} words={words} />
        </p>
        <p className="setting__meta">
          <code className="setting__key">
            <Highlight text={def.key} words={words} />
          </code>
          <span>Default: {describeValue(c, def.default)}</span>
          {range && <span>Range: {range}</span>}
          {c.kind === "number" && c.zeroMeans && <span>0 = {c.zeroMeans.toLowerCase()}</span>}
        </p>
        {error && (
          <p className="setting__error" id={errId} role="alert">
            <Icon name="warning" size={12} />
            {error}
          </p>
        )}
      </div>
      <div className="setting__control">
        {editor}
        <button
          type="button"
          className="icon-btn icon-btn--sm setting__reset"
          aria-label={`Reset ${def.title} to default`}
          data-tip={`Reset to default (${describeValue(c, def.default)})`}
          disabled={!modified || disabled}
          onClick={onReset}
        >
          <Icon name="reset" size={14} />
        </button>
      </div>
    </div>
  );
}
