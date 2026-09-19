import type { AliasPayload, CampaignCostPayload, CampaignParameters, CampaignTouchPayload, CaptureCriticalRequest, CaptureCriticalResponse, CaptureRequest, CaptureResponse, Consent, ConversionPayload, Envelope, ErrorPayload, EventPayload, FeatureExposurePayload, GroupPayload, HistogramValue, IdentifyPayload, InteractionPayload, Measurement, MetricPointPayload, PageViewPayload, PolicyVersionRequest, PolicyVersionResponse, Property, RejectedItem, ServiceError, SessionEndPayload, SessionStartPayload, SpanLink, SpanPayload, StackFrame, TelemetryItem, TypedValue } from "./types.gen.ts";
/** A CBOR semantic tag wrapping an inner value (e.g. tag 0 timestamp, tag 4 decimal). */
export type CborTag = {
    readonly tag: number;
    readonly value: CborValue;
};
/** A minimal canonical-CBOR value tree: a closed set of node variants. */
export type CborValue = number | bigint | boolean | null | string | Uint8Array | CborValue[] | Map<CborValue, CborValue> | CborTag;
/** Encode a CBOR value tree to canonical CSIL CBOR bytes. */
export declare function encodeValue(value: CborValue): Uint8Array;
/** Decode a CSIL CBOR byte payload into a CBOR value tree. */
export declare function decode(bytes: Uint8Array): CborValue;
/** The value for `key` in a CBOR map node, or `undefined` when absent. */
export declare function mapGet(value: CborValue, key: string): CborValue | undefined;
/** The value for a required `key`; throws when the field is missing. */
export declare function requireKey(value: CborValue, key: string): CborValue;
export declare function asNumber(value: CborValue): number;
export declare function asString(value: CborValue): string;
export declare function asBytes(value: CborValue): Uint8Array;
export declare function asBool(value: CborValue): boolean;
export declare function asArray(value: CborValue): CborValue[];
export declare function asMap(value: CborValue): Map<CborValue, CborValue>;
/** A decoded integer may surface as `bigint` (see `decInto`'s large-value path), so a
 * numeric literal's expected `number` is compared against the `bigint`-normalized form
 * rather than failing on a type mismatch that isn't a value mismatch. */
export declare function asLiteral<T extends CborValue>(value: CborValue, expected: T): T;
/** Validate a decoded scalar against a literal-enum's declared vocabulary, erroring
 * on an unknown value. The caller reads through `asNumber`/`asString`/`asBool`, which
 * already normalize a decoded `bigint` to `number`, so a plain membership check suffices. */
export declare function asEnumMember<T extends number | string | boolean | null>(value: T, members: readonly T[]): T;
/** Read a decoded CBOR scalar without narrowing to one JS type, normalizing a
 * decoded integer `bigint` to `number` the same way `asNumber` does. Used for a
 * MIXED-kind literal enum (`"a" / 1`), where no single `asNumber`/`asString`/
 * `asBool` reader fits every member's runtime type — `asEnumMember`'s membership
 * check does the real narrowing instead. */
export declare function asEnumScalar(value: CborValue): string | number | boolean | null;
/** Decode a tag-0 (RFC 3339, UTC) timestamp into a `Date`. */
export declare function asTimestamp(value: CborValue): Date;
/** Decode a tag-4 decimal fraction into its `[exponent, mantissa]` payload. */
export declare function asDecimalPayload(value: CborValue): [number | bigint, number | bigint];
/** A `Date` rendered as the canonical tag-0 text: RFC 3339, UTC, `Z` offset. */
export declare function csilTsToText(d: Date): string;
export declare function toEventPayloadCborValue(v: EventPayload): CborValue;
export declare function fromEventPayloadCborValue(value: CborValue): EventPayload;
export declare function toEventPayloadCbor(v: EventPayload): Uint8Array;
export declare function fromEventPayloadCbor(bytes: Uint8Array): EventPayload;
export declare function toPageViewPayloadCborValue(v: PageViewPayload): CborValue;
export declare function fromPageViewPayloadCborValue(value: CborValue): PageViewPayload;
export declare function toPageViewPayloadCbor(v: PageViewPayload): Uint8Array;
export declare function fromPageViewPayloadCbor(bytes: Uint8Array): PageViewPayload;
export declare function toCampaignParametersCborValue(v: CampaignParameters): CborValue;
export declare function fromCampaignParametersCborValue(value: CborValue): CampaignParameters;
export declare function toCampaignParametersCbor(v: CampaignParameters): Uint8Array;
export declare function fromCampaignParametersCbor(bytes: Uint8Array): CampaignParameters;
export declare function toSessionStartPayloadCborValue(v: SessionStartPayload): CborValue;
export declare function fromSessionStartPayloadCborValue(value: CborValue): SessionStartPayload;
export declare function toSessionStartPayloadCbor(v: SessionStartPayload): Uint8Array;
export declare function fromSessionStartPayloadCbor(bytes: Uint8Array): SessionStartPayload;
export declare function toSessionEndPayloadCborValue(v: SessionEndPayload): CborValue;
export declare function fromSessionEndPayloadCborValue(value: CborValue): SessionEndPayload;
export declare function toSessionEndPayloadCbor(v: SessionEndPayload): Uint8Array;
export declare function fromSessionEndPayloadCbor(bytes: Uint8Array): SessionEndPayload;
export declare function toInteractionPayloadCborValue(v: InteractionPayload): CborValue;
export declare function fromInteractionPayloadCborValue(value: CborValue): InteractionPayload;
export declare function toInteractionPayloadCbor(v: InteractionPayload): Uint8Array;
export declare function fromInteractionPayloadCbor(bytes: Uint8Array): InteractionPayload;
export declare function toFeatureExposurePayloadCborValue(v: FeatureExposurePayload): CborValue;
export declare function fromFeatureExposurePayloadCborValue(value: CborValue): FeatureExposurePayload;
export declare function toFeatureExposurePayloadCbor(v: FeatureExposurePayload): Uint8Array;
export declare function fromFeatureExposurePayloadCbor(bytes: Uint8Array): FeatureExposurePayload;
export declare function toIdentifyPayloadCborValue(v: IdentifyPayload): CborValue;
export declare function fromIdentifyPayloadCborValue(value: CborValue): IdentifyPayload;
export declare function toIdentifyPayloadCbor(v: IdentifyPayload): Uint8Array;
export declare function fromIdentifyPayloadCbor(bytes: Uint8Array): IdentifyPayload;
export declare function toAliasPayloadCborValue(v: AliasPayload): CborValue;
export declare function fromAliasPayloadCborValue(value: CborValue): AliasPayload;
export declare function toAliasPayloadCbor(v: AliasPayload): Uint8Array;
export declare function fromAliasPayloadCbor(bytes: Uint8Array): AliasPayload;
export declare function toGroupPayloadCborValue(v: GroupPayload): CborValue;
export declare function fromGroupPayloadCborValue(value: CborValue): GroupPayload;
export declare function toGroupPayloadCbor(v: GroupPayload): Uint8Array;
export declare function fromGroupPayloadCbor(bytes: Uint8Array): GroupPayload;
export declare function toConversionPayloadCborValue(v: ConversionPayload): CborValue;
export declare function fromConversionPayloadCborValue(value: CborValue): ConversionPayload;
export declare function toConversionPayloadCbor(v: ConversionPayload): Uint8Array;
export declare function fromConversionPayloadCbor(bytes: Uint8Array): ConversionPayload;
export declare function toStackFrameCborValue(v: StackFrame): CborValue;
export declare function fromStackFrameCborValue(value: CborValue): StackFrame;
export declare function toStackFrameCbor(v: StackFrame): Uint8Array;
export declare function fromStackFrameCbor(bytes: Uint8Array): StackFrame;
export declare function toErrorPayloadCborValue(v: ErrorPayload): CborValue;
export declare function fromErrorPayloadCborValue(value: CborValue): ErrorPayload;
export declare function toErrorPayloadCbor(v: ErrorPayload): Uint8Array;
export declare function fromErrorPayloadCbor(bytes: Uint8Array): ErrorPayload;
export declare function toSpanLinkCborValue(v: SpanLink): CborValue;
export declare function fromSpanLinkCborValue(value: CborValue): SpanLink;
export declare function toSpanLinkCbor(v: SpanLink): Uint8Array;
export declare function fromSpanLinkCbor(bytes: Uint8Array): SpanLink;
export declare function toSpanPayloadCborValue(v: SpanPayload): CborValue;
export declare function fromSpanPayloadCborValue(value: CborValue): SpanPayload;
export declare function toSpanPayloadCbor(v: SpanPayload): Uint8Array;
export declare function fromSpanPayloadCbor(bytes: Uint8Array): SpanPayload;
export declare function toHistogramValueCborValue(v: HistogramValue): CborValue;
export declare function fromHistogramValueCborValue(value: CborValue): HistogramValue;
export declare function toHistogramValueCbor(v: HistogramValue): Uint8Array;
export declare function fromHistogramValueCbor(bytes: Uint8Array): HistogramValue;
export declare function toMetricPointPayloadCborValue(v: MetricPointPayload): CborValue;
export declare function fromMetricPointPayloadCborValue(value: CborValue): MetricPointPayload;
export declare function toMetricPointPayloadCbor(v: MetricPointPayload): Uint8Array;
export declare function fromMetricPointPayloadCbor(bytes: Uint8Array): MetricPointPayload;
export declare function toCampaignTouchPayloadCborValue(v: CampaignTouchPayload): CborValue;
export declare function fromCampaignTouchPayloadCborValue(value: CborValue): CampaignTouchPayload;
export declare function toCampaignTouchPayloadCbor(v: CampaignTouchPayload): Uint8Array;
export declare function fromCampaignTouchPayloadCbor(bytes: Uint8Array): CampaignTouchPayload;
export declare function toCampaignCostPayloadCborValue(v: CampaignCostPayload): CborValue;
export declare function fromCampaignCostPayloadCborValue(value: CborValue): CampaignCostPayload;
export declare function toCampaignCostPayloadCbor(v: CampaignCostPayload): Uint8Array;
export declare function fromCampaignCostPayloadCbor(bytes: Uint8Array): CampaignCostPayload;
export declare function toTelemetryItemCborValue(v: TelemetryItem): CborValue;
export declare function fromTelemetryItemCborValue(value: CborValue): TelemetryItem;
export declare function toTelemetryItemCbor(v: TelemetryItem): Uint8Array;
export declare function fromTelemetryItemCbor(bytes: Uint8Array): TelemetryItem;
export declare function toCaptureRequestCborValue(v: CaptureRequest): CborValue;
export declare function fromCaptureRequestCborValue(value: CborValue): CaptureRequest;
export declare function toCaptureRequestCbor(v: CaptureRequest): Uint8Array;
export declare function fromCaptureRequestCbor(bytes: Uint8Array): CaptureRequest;
export declare function toCaptureResponseCborValue(v: CaptureResponse): CborValue;
export declare function fromCaptureResponseCborValue(value: CborValue): CaptureResponse;
export declare function toCaptureResponseCbor(v: CaptureResponse): Uint8Array;
export declare function fromCaptureResponseCbor(bytes: Uint8Array): CaptureResponse;
export declare function toRejectedItemCborValue(v: RejectedItem): CborValue;
export declare function fromRejectedItemCborValue(value: CborValue): RejectedItem;
export declare function toRejectedItemCbor(v: RejectedItem): Uint8Array;
export declare function fromRejectedItemCbor(bytes: Uint8Array): RejectedItem;
export declare function toCaptureCriticalRequestCborValue(v: CaptureCriticalRequest): CborValue;
export declare function fromCaptureCriticalRequestCborValue(value: CborValue): CaptureCriticalRequest;
export declare function toCaptureCriticalRequestCbor(v: CaptureCriticalRequest): Uint8Array;
export declare function fromCaptureCriticalRequestCbor(bytes: Uint8Array): CaptureCriticalRequest;
export declare function toCaptureCriticalResponseCborValue(v: CaptureCriticalResponse): CborValue;
export declare function fromCaptureCriticalResponseCborValue(value: CborValue): CaptureCriticalResponse;
export declare function toCaptureCriticalResponseCbor(v: CaptureCriticalResponse): Uint8Array;
export declare function fromCaptureCriticalResponseCbor(bytes: Uint8Array): CaptureCriticalResponse;
export declare function toPolicyVersionRequestCborValue(v: PolicyVersionRequest): CborValue;
export declare function fromPolicyVersionRequestCborValue(value: CborValue): PolicyVersionRequest;
export declare function toPolicyVersionRequestCbor(v: PolicyVersionRequest): Uint8Array;
export declare function fromPolicyVersionRequestCbor(bytes: Uint8Array): PolicyVersionRequest;
export declare function toPolicyVersionResponseCborValue(v: PolicyVersionResponse): CborValue;
export declare function fromPolicyVersionResponseCborValue(value: CborValue): PolicyVersionResponse;
export declare function toPolicyVersionResponseCbor(v: PolicyVersionResponse): Uint8Array;
export declare function fromPolicyVersionResponseCbor(bytes: Uint8Array): PolicyVersionResponse;
export declare function toTypedValueCborValue(v: TypedValue): CborValue;
export declare function fromTypedValueCborValue(value: CborValue): TypedValue;
export declare function toTypedValueCbor(v: TypedValue): Uint8Array;
export declare function fromTypedValueCbor(bytes: Uint8Array): TypedValue;
export declare function toPropertyCborValue(v: Property): CborValue;
export declare function fromPropertyCborValue(value: CborValue): Property;
export declare function toPropertyCbor(v: Property): Uint8Array;
export declare function fromPropertyCbor(bytes: Uint8Array): Property;
export declare function toMeasurementCborValue(v: Measurement): CborValue;
export declare function fromMeasurementCborValue(value: CborValue): Measurement;
export declare function toMeasurementCbor(v: Measurement): Uint8Array;
export declare function fromMeasurementCbor(bytes: Uint8Array): Measurement;
export declare function toConsentCborValue(v: Consent): CborValue;
export declare function fromConsentCborValue(value: CborValue): Consent;
export declare function toConsentCbor(v: Consent): Uint8Array;
export declare function fromConsentCbor(bytes: Uint8Array): Consent;
export declare function toEnvelopeCborValue(v: Envelope): CborValue;
export declare function fromEnvelopeCborValue(value: CborValue): Envelope;
export declare function toEnvelopeCbor(v: Envelope): Uint8Array;
export declare function fromEnvelopeCbor(bytes: Uint8Array): Envelope;
export declare function toServiceErrorCborValue(v: ServiceError): CborValue;
export declare function fromServiceErrorCborValue(value: CborValue): ServiceError;
export declare function toServiceErrorCbor(v: ServiceError): Uint8Array;
export declare function fromServiceErrorCbor(bytes: Uint8Array): ServiceError;
