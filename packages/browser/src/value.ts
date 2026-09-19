// Typed values, above the generated shape.
//
// The wire type carries an explicit `kind` and one optional field for each type.
// A bare choice cannot be discriminated in a dynamically typed language: every
// `int`, `uint`, and `float` is one JavaScript `number`, so an encoder had to
// guess and always guessed the first arm. See docs/IMPLEMENTATION_LOG.md L017.
//
// This module is the readable form above that shape. A caller names the kind
// once, and the discriminant and the field can never disagree.

import { CsilDecimal } from "./collector-api.ts";
import type {
  Measurement,
  Property,
  PropertyOrigin,
  TypedValue,
} from "./collector-api.ts";

/** One typed value, in the form calling code writes. */
export type Value =
  | { readonly kind: "null" }
  | { readonly kind: "bool"; readonly value: boolean }
  | { readonly kind: "int"; readonly value: number }
  | { readonly kind: "uint"; readonly value: number }
  | { readonly kind: "float"; readonly value: number }
  | { readonly kind: "decimal"; readonly value: CsilDecimal }
  | { readonly kind: "text"; readonly value: string }
  | { readonly kind: "bytes"; readonly value: Uint8Array };

/** The value a property holds when it is present and empty. Not the same as an
 * absent property. */
export const nullValue = (): Value => ({ kind: "null" });
/** A true or false. */
export const bool = (value: boolean): Value => ({ kind: "bool", value });
/** A whole number that can be negative. */
export const int = (value: number): Value => ({ kind: "int", value });
/** A whole number that is not negative. */
export const uint = (value: number): Value => ({ kind: "uint", value });
/** A number that does not have to be whole. Never use it for money. */
export const float = (value: number): Value => ({ kind: "float", value });
/** A string. */
export const text = (value: string): Value => ({ kind: "text", value });
/** Raw data. */
export const bytes = (value: Uint8Array): Value => ({ kind: "bytes", value });

/**
 * An exact number from its canonical text form, such as `19.99`.
 *
 * Money uses this and never uses `float`. It throws on text that is not a
 * number, because a caller that cannot parse a value must not substitute one.
 */
export function decimal(value: string): Value {
  const trimmed = value.trim();
  const negative = trimmed.startsWith("-");
  const digits = negative || trimmed.startsWith("+") ? trimmed.slice(1) : trimmed;
  const dot = digits.indexOf(".");
  const whole = dot < 0 ? digits : digits.slice(0, dot);
  const fraction = dot < 0 ? "" : digits.slice(dot + 1);
  if (whole === "" && fraction === "") {
    throw new Error(`\`${value}\` is not a number. Send a number such as 19.99.`);
  }
  if (!/^\d*$/.test(whole) || !/^\d*$/.test(fraction)) {
    throw new Error(`\`${value}\` is not a number. Send a number such as 19.99.`);
  }
  let mantissa = BigInt(`${whole}${fraction}` || "0");
  if (negative) mantissa = -mantissa;
  return { kind: "decimal", value: new CsilDecimal(-fraction.length, mantissa) };
}

/** Write a value as the shape that crosses the wire. */
export function write(value: Value): TypedValue {
  const out: TypedValue = { kind: value.kind };
  switch (value.kind) {
    case "null":
      return out;
    case "bool":
      return { ...out, boolValue: value.value };
    case "int":
      return { ...out, intValue: value.value };
    case "uint":
      return { ...out, uintValue: value.value };
    case "float":
      return { ...out, floatValue: value.value };
    case "decimal":
      return { ...out, decimalValue: value.value };
    case "text":
      return { ...out, textValue: value.value };
    case "bytes":
      return { ...out, bytesValue: value.value };
  }
}

/**
 * Read a wire value.
 *
 * A kind that names a field the message did not carry is a rejection, never a
 * substituted default. A value that arrives as a number where the sender said
 * text is the kind of fault that produces a wrong answer quietly.
 */
export function read(value: TypedValue): Value {
  const missing = (named: string): never => {
    throw new Error(`A value says it holds ${named} and carries no ${named}.`);
  };
  switch (value.kind) {
    case "null":
      return nullValue();
    case "bool":
      return value.boolValue === undefined ? missing("a true or false") : bool(value.boolValue);
    case "int":
      return value.intValue === undefined ? missing("a whole number") : int(value.intValue);
    case "uint":
      return value.uintValue === undefined
        ? missing("a whole number that is not negative")
        : uint(value.uintValue);
    case "float":
      return value.floatValue === undefined ? missing("a number") : float(value.floatValue);
    case "decimal":
      return value.decimalValue === undefined
        ? missing("an exact number")
        : { kind: "decimal", value: value.decimalValue };
    case "text":
      return value.textValue === undefined ? missing("text") : text(value.textValue);
    case "bytes":
      return value.bytesValue === undefined ? missing("data") : bytes(value.bytesValue);
    default:
      throw new Error(`A value names a kind this software does not know: ${String(value.kind)}`);
  }
}

/** The value rendered the way a person expects to read it. */
export function display(value: Value): string {
  switch (value.kind) {
    case "null":
      return "";
    case "bool":
      return value.value ? "true" : "false";
    case "int":
    case "uint":
    case "float":
      return String(value.value);
    case "decimal":
      return value.value.toString();
    case "text":
      return value.value;
    case "bytes":
      return Array.from(value.value)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
  }
}

/** One property, with its origin. */
export function property(key: string, value: Value, origin: PropertyOrigin): Property {
  return { key, value: write(value), origin };
}

/**
 * One measurement. A value that is not a number is refused rather than
 * converted, because a measure that silently became something else is a wrong
 * number on a chart.
 */
export function measurement(key: string, value: Value, unit?: string): Measurement {
  const base = { key, ...(unit === undefined ? {} : { unit }) };
  switch (value.kind) {
    case "float":
      return { ...base, kind: "float", floatValue: value.value };
    case "int":
      return { ...base, kind: "int", intValue: value.value };
    case "decimal":
      return { ...base, kind: "decimal", decimalValue: value.value };
    default:
      throw new Error(
        `The measurement \`${key}\` was given ${value.kind} and a measurement holds a number. ` +
          "Send a number, or send the value as a property instead.",
      );
  }
}
