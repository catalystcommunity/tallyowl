import { CsilDecimal } from "./collector-api.ts";
import type { Measurement, Property, PropertyOrigin, TypedValue } from "./collector-api.ts";
/** One typed value, in the form calling code writes. */
export type Value = {
    readonly kind: "null";
} | {
    readonly kind: "bool";
    readonly value: boolean;
} | {
    readonly kind: "int";
    readonly value: number;
} | {
    readonly kind: "uint";
    readonly value: number;
} | {
    readonly kind: "float";
    readonly value: number;
} | {
    readonly kind: "decimal";
    readonly value: CsilDecimal;
} | {
    readonly kind: "text";
    readonly value: string;
} | {
    readonly kind: "bytes";
    readonly value: Uint8Array;
};
/** The value a property holds when it is present and empty. Not the same as an
 * absent property. */
export declare const nullValue: () => Value;
/** A true or false. */
export declare const bool: (value: boolean) => Value;
/** A whole number that can be negative. */
export declare const int: (value: number) => Value;
/** A whole number that is not negative. */
export declare const uint: (value: number) => Value;
/** A number that does not have to be whole. Never use it for money. */
export declare const float: (value: number) => Value;
/** A string. */
export declare const text: (value: string) => Value;
/** Raw data. */
export declare const bytes: (value: Uint8Array) => Value;
/**
 * An exact number from its canonical text form, such as `19.99`.
 *
 * Money uses this and never uses `float`. It throws on text that is not a
 * number, because a caller that cannot parse a value must not substitute one.
 */
export declare function decimal(value: string): Value;
/** Write a value as the shape that crosses the wire. */
export declare function write(value: Value): TypedValue;
/**
 * Read a wire value.
 *
 * A kind that names a field the message did not carry is a rejection, never a
 * substituted default. A value that arrives as a number where the sender said
 * text is the kind of fault that produces a wrong answer quietly.
 */
export declare function read(value: TypedValue): Value;
/** The value rendered the way a person expects to read it. */
export declare function display(value: Value): string;
/** One property, with its origin. */
export declare function property(key: string, value: Value, origin: PropertyOrigin): Property;
/**
 * One measurement. A value that is not a number is refused rather than
 * converted, because a measure that silently became something else is a wrong
 * number on a chart.
 */
export declare function measurement(key: string, value: Value, unit?: string): Measurement;
