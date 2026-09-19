/**
 * An exact base-10 decimal, carried on the wire as a CBOR tag-4 decimal
 * fraction `[exponent, mantissa]` (value = `mantissa * 10 ** exponent`).
 *
 * Emitted only when the spec uses `decimal` and `decimal_mapping` is `"csil"`
 * (the default), so the generated output depends on no third-party package.
 * Bridge to/from `decimal.js` via the canonical string:
 * `new Decimal(d.toString())` / `CsilDecimal.fromString(dec.toString())`.
 */
export declare class CsilDecimal {
    readonly exponent: number;
    readonly mantissa: bigint;
    /** CBOR semantic tag for a decimal fraction. */
    static readonly CBOR_TAG = 4;
    constructor(exponent: number, mantissa: bigint);
    /** Reconstruct from the CBOR tag-4 payload `[exponent, mantissa]`. */
    static fromTag4(payload: readonly [number | bigint, number | bigint]): CsilDecimal;
    /**
     * The CBOR tag-4 payload `[exponent, mantissa]`. The transport owns CBOR
     * encoding; hand this (tagged 4) to the encoder, e.g.
     * `new Tagged(CsilDecimal.CBOR_TAG, d.toTag4())`.
     */
    toTag4(): [number, bigint];
    /**
     * Sign of `this - other` as `-1`, `0`, or `1`. Exact: both values are rescaled
     * to a shared exponent and compared as bigints, so no float rounding occurs.
     * Drives generated validation guards (`d.compare(bound) >= 0`).
     */
    compare(other: CsilDecimal): number;
    /** Parse a decimal string (`"-12.340"`, `"5e-3"`) without loss of precision. */
    static fromString(text: string): CsilDecimal;
    /** Canonical decimal string; round-trips through `decimal.js` via its `toString`. */
    toString(): string;
    /** JSON form is the exact canonical string so structured logs stay lossless. */
    toJSON(): string;
}
export type Compression = "none" | "zstd";
export interface Batch {
    batchId: BatchId;
    items: TelemetryItem[];
    commonProperties?: PropertyList;
    sealedAt: Timestamp;
    compression?: Compression;
}
export interface SubmitBatchRequest {
    batch: Batch;
    policyVersion?: number;
    protocolVersion?: number;
}
/**
 * The receipt names the durability that the collector actually reached. It
 * never claims a stronger boundary than the configuration required.
 */
export interface SubmitBatchResponse {
    batchId: BatchId;
    accepted: number;
    durableCopies: number;
    queuedAt: Timestamp;
    rejected?: RejectedItem[];
    policyVersion?: number;
}
export type ReceiptPolicy = "local-one" | "local-quorum" | "remote-one" | "custom";
export interface CommitBatchRequest {
    batch: Batch;
    sourceId: SourceId;
    attempt?: number;
    protocolVersion?: number;
}
export interface CommitBatchResponse {
    batchId: BatchId;
    accepted: number;
    committedAt: Timestamp;
    satisfiedPolicy: ReceiptPolicy;
    commitWatermark: number;
    protocolVersion: number;
    projectorVersion: number;
    rejected?: RejectedItem[];
    deduplicated?: boolean;
}
export interface DeliveryTask {
    taskVersion: number;
    batchId: BatchId;
    sourceId: SourceId;
    batch: Uint8Array;
    compression: Compression;
    uncompressedBytes: number;
    attempts: number;
    acceptedAt: Timestamp;
    lastAttemptAt?: Timestamp;
    nextAttemptAt?: Timestamp;
    lastFailure?: string;
}
export interface ResolveKeyRequest {
    credential: string;
}
export interface ResolveKeyResponse {
    keyId: string;
    workspaceId: WorkspaceId;
    projectId: ProjectId;
    sourceId: SourceId;
    cacheTtlMs: DurationMs;
    expiresAt?: Timestamp;
}
export type RetentionClass = "provisional" | "raw" | "detailed" | "rollup" | "audit";
export interface RetentionRule {
    class: RetentionClass;
    durationMs: DurationMs;
    kind?: TelemetryKind;
}
export interface SamplingClause {
    mode: "keep-all" | "keep-percent" | "keep-first-n";
    percent?: number;
    count?: number;
    intervalMs?: DurationMs;
}
/**
 * A tail rule uses the query expression tree. TallyOwl defines no second
 * expression language. See D45. The expression travels as canonical CBOR
 * because the control specification owns the expression schema.
 */
export interface TailRule {
    name: string;
    expression: Uint8Array;
    sampling: SamplingClause;
}
export interface ProtectedKey {
    key: string;
    origin: PropertyOrigin;
}
/**
 * How much of a campaign touch is joined to the person who made it. The same
 * three values as `CampaignLinking` in tallyowl-control.csil, and the same
 * meaning. See D30.
 */
export type CampaignLinking = "linked" | "unlinked" | "none";
export interface CollectionPolicy {
    policyVersion: number;
    enabledKinds: TelemetryKind[];
    headSampleRate: number;
    tailRules?: TailRule[];
    tailDecisionWindowMs?: DurationMs;
    lateSpanGraceMs?: DurationMs;
    alwaysKeepExpressions?: Uint8Array[];
    retention: RetentionRule[];
    protectedKeys: ProtectedKey[];
    stampedProperties: PropertyList;
    maxEventBytes: number;
    maxBatchBytes: number;
    maxProperties: number;
    sessionMaxLifetimeMs: DurationMs;
    redactKeys?: string[];
    blockedEventNames?: string[];
    blockedPropertyKeys?: string[];
    campaignLinking?: CampaignLinking;
    killSwitch?: boolean;
}
export interface FetchPolicyRequest {
    sourceId: SourceId;
    knownVersion?: number;
}
export interface FetchPolicyResponse {
    policy?: CollectionPolicy;
    unchanged: boolean;
}
export interface CollectorHealth {
    role: "intake" | "forwarder" | "compatibility-receiver";
    ready: boolean;
    queueDepth: number;
    oldestTaskAgeMs: DurationMs;
    quarantineCount: number;
    appliedPolicyVersion: number;
    policyAgeMs?: DurationMs;
    lastSweepAt?: Timestamp;
}
export interface HealthRequest {
}
export type EventId = Uint8Array;
export type BatchId = Uint8Array;
export type WorkspaceId = Uint8Array;
export type ProjectId = Uint8Array;
export type SourceId = Uint8Array;
export type SessionId = string;
export type TraceId = Uint8Array;
export type SpanId = Uint8Array;
export type Timestamp = number;
export type DurationMs = number;
export type TypedValueKind = "null" | "bool" | "int" | "uint" | "float" | "decimal" | "text" | "bytes";
/**
 * One typed value. `kind` names which value field carries it. A `kind` of
 * `null` carries no value field, which is how a null property differs from an
 * absent one.
 */
export interface TypedValue {
    kind: TypedValueKind;
    boolValue?: boolean;
    intValue?: number;
    uintValue?: number;
    floatValue?: number;
    decimalValue?: CsilDecimal;
    textValue?: string;
    bytesValue?: Uint8Array;
}
/**
 * Where a property came from. An operator can trust a `collector` origin,
 * because the collector stamps it from its own configuration and refuses a
 * client value for a protected name. See D38.
 */
export type PropertyOrigin = "client" | "driver" | "collector";
export interface Property {
    key: string;
    value: TypedValue;
    origin: PropertyOrigin;
}
export type PropertyList = Property[];
/**
 * A measure is a number, so its kinds are the three numeric ones. It carries
 * its own discriminant for the same reason `TypedValue` does.
 */
export type MeasurementKind = "float" | "decimal" | "int";
/**
 * A numeric observation with a unit, kept apart from a property so that a
 * measure never has to guess whether a value is a dimension or a number.
 */
export interface Measurement {
    key: string;
    kind: MeasurementKind;
    floatValue?: number;
    decimalValue?: CsilDecimal;
    intValue?: number;
    unit?: string;
}
export type MeasurementList = Measurement[];
export type ConsentState = "granted" | "denied" | "absent";
export interface Consent {
    marketing: ConsentState;
    analytics: ConsentState;
    policyVersion?: string;
}
export type TelemetryKind = "event" | "page-view" | "session-start" | "session-end" | "session-heartbeat" | "interaction" | "feature-exposure" | "identify" | "alias" | "group" | "conversion" | "error" | "span" | "metric-point" | "campaign-touch" | "campaign-cost";
export interface Envelope {
    eventId: EventId;
    kind: TelemetryKind;
    schemaVersion: number;
    occurredAt: Timestamp;
    observedAt?: Timestamp;
    receivedAt?: Timestamp;
    workspaceId?: WorkspaceId;
    projectId?: ProjectId;
    sourceId?: SourceId;
    sequence?: number;
    release?: string;
    serviceName?: string;
    requestId?: string;
    sessionId?: SessionId;
    endUserId?: string;
    anonymousId?: string;
    traceId?: TraceId;
    spanId?: SpanId;
    consent?: Consent;
    sdkName: string;
    sdkVersion: string;
    properties: PropertyList;
    measurements?: MeasurementList;
}
export type ErrorCode = "invalid-argument" | "unauthenticated" | "permission-denied" | "not-found" | "already-exists" | "resource-exhausted" | "failed-precondition" | "unavailable" | "schema-unsupported" | "budget-exceeded" | "incomplete-result" | "internal";
export interface ServiceError {
    code: ErrorCode;
    message: string;
    retryable: boolean;
    detail?: PropertyList;
}
/**
 * A named product or behavior event.
 */
export interface EventPayload {
    name: string;
    route?: string;
    pageTitle?: string;
}
/**
 * A page view or a screen view.
 */
export interface PageViewPayload {
    route: string;
    pageTitle?: string;
    referrer?: string;
    campaign?: CampaignParameters;
}
export interface CampaignParameters {
    source?: string;
    medium?: string;
    campaign?: string;
    term?: string;
    content?: string;
    clickId?: string;
}
/**
 * The session lifecycle works on every surface, including a terminal user
 * interface. The client library issues the ID. See D11.
 */
export interface SessionStartPayload {
    entryRoute?: string;
}
export interface SessionEndPayload {
    reason: "explicit" | "timeout" | "maximum-lifetime";
}
export interface InteractionPayload {
    target: string;
    action: string;
}
export interface FeatureExposurePayload {
    feature: string;
    variant: string;
}
export interface IdentifyPayload {
    endUserId: string;
}
export interface AliasPayload {
    fromId: string;
    toId: string;
}
export interface GroupPayload {
    groupId: string;
    groupKind?: string;
}
/**
 * A conversion carries an exact decimal value. It never uses a float.
 *
 * `order_id` is what makes a conversion idempotent. A checkout that retried,
 * a webhook that arrived twice, and a person who refreshed the receipt page
 * all produce the same order. TallyOwl counts one conversion and one value for
 * one `goal` and `order_id` pair, and keeps the earliest. A conversion without
 * an order identifier is its own event and is never folded into another.
 *
 * `campaign` and `touch_event_id` are the optional links DATA_MODEL.md
 * section 3.6 describes. An application that already knows which touch earned
 * a conversion says so; attribution uses the link and does not have to find
 * the touch again.
 */
export interface ConversionPayload {
    goal: string;
    value?: CsilDecimal;
    currency?: string;
    orderId?: string;
    campaign?: CampaignParameters;
    touchEventId?: EventId;
}
export interface StackFrame {
    module?: string;
    function?: string;
    file?: string;
    line?: number;
    inApp: boolean;
}
export interface ErrorPayload {
    errorType: string;
    message: string;
    handled: boolean;
    severity: "fatal" | "error" | "warning" | "info";
    mechanism?: string;
    runtime?: string;
    frames?: StackFrame[];
    breadcrumbs?: EventId[];
}
export type SpanKind = "internal" | "server" | "client" | "producer" | "consumer";
export interface SpanLink {
    traceId: TraceId;
    spanId: SpanId;
}
export interface SpanPayload {
    operation: string;
    kind: SpanKind;
    startAt: Timestamp;
    durationMs: DurationMs;
    status: "ok" | "error" | "unset";
    resource?: string;
    parentSpanId?: SpanId;
    links?: SpanLink[];
    errorEventId?: EventId;
    samplingReason?: string;
}
export type MetricKind = "counter" | "gauge" | "histogram";
export interface HistogramValue {
    count: number;
    sum: number;
    bounds: number[];
    counts: number[];
}
export interface MetricPointPayload {
    metricName: string;
    metricKind: MetricKind;
    unit?: string;
    description?: string;
    monotonic: boolean;
    temporality: "delta" | "cumulative";
    startAt: Timestamp;
    endAt: Timestamp;
    labels: PropertyList;
    numberValue?: number;
    histogramValue?: HistogramValue;
    exemplarTraceId?: TraceId;
}
/**
 * One touch. DATA_MODEL.md section 3.6 lists what it records, and every
 * field here is one of them. Two are absent on purpose:
 *
 * - the **channel classification and its classifier version** are derived
 * from `campaign` and `referrer_domain` rather than sent. A producer that
 * could name its own channel could put paid traffic in the organic column;
 * - **first and last touch position** is a property of the set of touches a
 * conversion looked back over, not of one touch. It is decided when the
 * question is asked.
 *
 * `referrer` is the whole referring address and `referrer_domain` is the host
 * from it. Both travel because a classifier reads the host and a person
 * reading one touch wants the address.
 */
export interface CampaignTouchPayload {
    campaign: CampaignParameters;
    referrer?: string;
    referrerDomain?: string;
    landingRoute?: string;
}
export interface CampaignCostPayload {
    campaign: string;
    platform?: string;
    cost: CsilDecimal;
    currency: string;
    periodStart: Timestamp;
    periodEnd: Timestamp;
}
export interface TelemetryItem {
    envelope: Envelope;
    event?: EventPayload;
    pageView?: PageViewPayload;
    sessionStart?: SessionStartPayload;
    sessionEnd?: SessionEndPayload;
    interaction?: InteractionPayload;
    featureExposure?: FeatureExposurePayload;
    identify?: IdentifyPayload;
    alias?: AliasPayload;
    group?: GroupPayload;
    conversion?: ConversionPayload;
    error?: ErrorPayload;
    span?: SpanPayload;
    metricPoint?: MetricPointPayload;
    campaignTouch?: CampaignTouchPayload;
    campaignCost?: CampaignCostPayload;
}
export interface CaptureRequest {
    items: TelemetryItem[];
}
export interface CaptureResponse {
    accepted: number;
    rejected?: RejectedItem[];
}
export interface RejectedItem {
    eventId: EventId;
    code: ErrorCode;
    message: string;
}
export interface CaptureCriticalRequest {
    items: TelemetryItem[];
}
export interface CaptureCriticalResponse {
    accepted: number;
    durable: boolean;
    batchId?: BatchId;
    rejected?: RejectedItem[];
}
/**
 * A policy snapshot version that the client holds. The application can pass a
 * newer snapshot to the browser package over its own connection.
 */
export interface PolicyVersionRequest {
}
export interface PolicyVersionResponse {
    policyVersion: number;
    samplingRate: number;
    enabledKinds: TelemetryKind[];
}
export declare function validateDeliveryTask(value: DeliveryTask): string[];
export declare function validateResolveKeyRequest(value: ResolveKeyRequest): string[];
export declare function validateResolveKeyResponse(value: ResolveKeyResponse): string[];
export declare function validateTailRule(value: TailRule): string[];
export declare function validateProtectedKey(value: ProtectedKey): string[];
export declare function validateEventId(value: EventId): string[];
export declare function validateBatchId(value: BatchId): string[];
export declare function validateWorkspaceId(value: WorkspaceId): string[];
export declare function validateProjectId(value: ProjectId): string[];
export declare function validateSourceId(value: SourceId): string[];
export declare function validateSessionId(value: SessionId): string[];
export declare function validateTraceId(value: TraceId): string[];
export declare function validateSpanId(value: SpanId): string[];
export declare function validateProperty(value: Property): string[];
export declare function validateMeasurement(value: Measurement): string[];
export declare function validateConsent(value: Consent): string[];
export declare function validateEnvelope(value: Envelope): string[];
export declare function validateEventPayload(value: EventPayload): string[];
export declare function validatePageViewPayload(value: PageViewPayload): string[];
export declare function validateCampaignParameters(value: CampaignParameters): string[];
export declare function validateSessionStartPayload(value: SessionStartPayload): string[];
export declare function validateInteractionPayload(value: InteractionPayload): string[];
export declare function validateFeatureExposurePayload(value: FeatureExposurePayload): string[];
export declare function validateIdentifyPayload(value: IdentifyPayload): string[];
export declare function validateAliasPayload(value: AliasPayload): string[];
export declare function validateGroupPayload(value: GroupPayload): string[];
export declare function validateConversionPayload(value: ConversionPayload): string[];
export declare function validateStackFrame(value: StackFrame): string[];
export declare function validateErrorPayload(value: ErrorPayload): string[];
export declare function validateSpanPayload(value: SpanPayload): string[];
export declare function validateMetricPointPayload(value: MetricPointPayload): string[];
export declare function validateCampaignTouchPayload(value: CampaignTouchPayload): string[];
export declare function validateCampaignCostPayload(value: CampaignCostPayload): string[];
