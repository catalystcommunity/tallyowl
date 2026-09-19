// Package tallyowl is the Go app driver.
//
// The generated client provides types, codecs, and routing seams. This driver
// provides buffering, batching, configuration, and host integration. It is an
// app driver: never an adapter, and never an SDK.
//
// # The promise, and the one thing it will not do
//
// Capture buffers. Flush returns only after collector intake acknowledges, and
// that acknowledgement means Corndogs durably accepted the batch. Nothing here
// reports success for data it discarded.
//
// When the unacknowledged bound is reached, a durable send returns a typed
// backpressure error. It does not silently become best effort. See D19 and
// docs/DELIVERY.md section 8.
package tallyowl

import (
	"fmt"
	"math/big"
	"strings"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

// ValueKind names which kind of value a Value holds.
type ValueKind string

const (
	KindNull    ValueKind = "null"
	KindBool    ValueKind = "bool"
	KindInt     ValueKind = "int"
	KindUint    ValueKind = "uint"
	KindFloat   ValueKind = "float"
	KindDecimal ValueKind = "decimal"
	KindText    ValueKind = "text"
	KindBytes   ValueKind = "bytes"
)

// Value is one typed property value.
//
// The wire type carries an explicit kind and one optional field for each type,
// because a bare choice cannot be discriminated in a dynamically typed language
// and the three maintained languages must agree byte for byte. See
// docs/IMPLEMENTATION_LOG.md L017. This type is the readable form above it.
type Value struct {
	Kind ValueKind

	Bool    bool
	Int     int64
	Uint    uint64
	Float   float64
	Decimal api.CsilDecimal
	Text    string
	Bytes   []byte
}

// Null is the value a property holds when it is present and empty. It is not
// the same as an absent property.
func Null() Value { return Value{Kind: KindNull} }

// Bool is a true or false.
func Bool(v bool) Value { return Value{Kind: KindBool, Bool: v} }

// Int is a whole number that can be negative.
func Int(v int64) Value { return Value{Kind: KindInt, Int: v} }

// Uint is a whole number that is not negative.
func Uint(v uint64) Value { return Value{Kind: KindUint, Uint: v} }

// Float is a number that does not have to be whole. Never use it for money.
func Float(v float64) Value { return Value{Kind: KindFloat, Float: v} }

// Text is a string.
func Text(v string) Value { return Value{Kind: KindText, Text: v} }

// Bytes is raw data.
func Bytes(v []byte) Value { return Value{Kind: KindBytes, Bytes: v} }

// Decimal is an exact number from its canonical text form, such as "19.99".
// Money uses this and never uses Float.
func Decimal(text string) (Value, error) {
	d, err := api.ParseCsilDecimal(text)
	if err != nil {
		return Value{}, fmt.Errorf("`%s` is not a number. Send a number such as 19.99", text)
	}
	return Value{Kind: KindDecimal, Decimal: d}, nil
}

// MustDecimal is Decimal for a literal the caller wrote. It panics on text that
// is not a number, which is a mistake in the calling code rather than in data.
func MustDecimal(text string) Value {
	v, err := Decimal(text)
	if err != nil {
		panic(err)
	}
	return v
}

// Wire writes a Value as the shape that crosses the wire, with the discriminant
// and the field always in agreement.
func (v Value) Wire() api.TypedValue {
	out := api.TypedValue{Kind: api.TypedValueKind(v.Kind)}
	switch v.Kind {
	case KindBool:
		b := v.Bool
		out.BoolValue = &b
	case KindInt:
		n := v.Int
		out.IntValue = &n
	case KindUint:
		n := v.Uint
		out.UintValue = &n
	case KindFloat:
		f := v.Float
		out.FloatValue = &f
	case KindDecimal:
		d := v.Decimal
		out.DecimalValue = &d
	case KindText:
		s := v.Text
		out.TextValue = &s
	case KindBytes:
		b := v.Bytes
		out.BytesValue = &b
	}
	return out
}

// ReadValue reads a wire value.
//
// A kind that names a field the message did not carry is a rejection, never a
// substituted default. A value that arrives as a number where the sender said
// text is the kind of fault that produces a wrong answer quietly.
func ReadValue(v api.TypedValue) (Value, error) {
	missing := func(named string) error {
		return fmt.Errorf("a value says it holds %s and carries no %s", named, named)
	}
	switch ValueKind(v.Kind) {
	case KindNull:
		return Null(), nil
	case KindBool:
		if v.BoolValue == nil {
			return Value{}, missing("a true or false")
		}
		return Bool(*v.BoolValue), nil
	case KindInt:
		if v.IntValue == nil {
			return Value{}, missing("a whole number")
		}
		return Int(*v.IntValue), nil
	case KindUint:
		if v.UintValue == nil {
			return Value{}, missing("a whole number that is not negative")
		}
		return Uint(*v.UintValue), nil
	case KindFloat:
		if v.FloatValue == nil {
			return Value{}, missing("a number")
		}
		return Float(*v.FloatValue), nil
	case KindDecimal:
		if v.DecimalValue == nil {
			return Value{}, missing("an exact number")
		}
		return Value{Kind: KindDecimal, Decimal: *v.DecimalValue}, nil
	case KindText:
		if v.TextValue == nil {
			return Value{}, missing("text")
		}
		return Text(*v.TextValue), nil
	case KindBytes:
		if v.BytesValue == nil {
			return Value{}, missing("data")
		}
		return Bytes(*v.BytesValue), nil
	}
	return Value{}, fmt.Errorf("a value names a kind this software does not know: %q", v.Kind)
}

// String renders the value the way a person expects to read it. A decimal keeps
// its exact digits.
func (v Value) String() string {
	switch v.Kind {
	case KindNull:
		return ""
	case KindBool:
		if v.Bool {
			return "true"
		}
		return "false"
	case KindInt:
		return fmt.Sprintf("%d", v.Int)
	case KindUint:
		return fmt.Sprintf("%d", v.Uint)
	case KindFloat:
		return strings.TrimRight(strings.TrimRight(fmt.Sprintf("%f", v.Float), "0"), ".")
	case KindDecimal:
		return v.Decimal.String()
	case KindText:
		return v.Text
	case KindBytes:
		var b strings.Builder
		for _, x := range v.Bytes {
			fmt.Fprintf(&b, "%02x", x)
		}
		return b.String()
	}
	return ""
}

// Property builds one property with its origin.
func Property(key string, value Value, origin api.PropertyOrigin) api.Property {
	return api.Property{Key: key, Value: value.Wire(), Origin: origin}
}

// Measurement builds one measurement. A value that is not a number is refused
// rather than converted, because a measure that silently became something else
// is a wrong number on a chart.
func Measurement(key string, value Value, unit string) (api.Measurement, error) {
	out := api.Measurement{Key: key}
	if unit != "" {
		u := unit
		out.Unit = &u
	}
	switch value.Kind {
	case KindFloat:
		out.Kind = api.MeasurementKind("float")
		f := value.Float
		out.FloatValue = &f
	case KindInt:
		out.Kind = api.MeasurementKind("int")
		n := value.Int
		out.IntValue = &n
	case KindDecimal:
		out.Kind = api.MeasurementKind("decimal")
		d := value.Decimal
		out.DecimalValue = &d
	default:
		return api.Measurement{}, fmt.Errorf(
			"the measurement `%s` was given %s and a measurement holds a number",
			key, value.Kind)
	}
	return out, nil
}

// decimalFromParts is the inverse of the text form, used by tests that build a
// known exponent and mantissa rather than parsing.
func decimalFromParts(exponent int64, mantissa int64) api.CsilDecimal {
	return api.CsilDecimal{Exponent: exponent, Mantissa: big.NewInt(mantissa)}
}
