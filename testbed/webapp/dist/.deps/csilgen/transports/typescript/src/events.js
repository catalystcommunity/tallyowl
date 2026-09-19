// CSIL-Events transport — typed bidirectional event streams — see
// `csil-events-transport.md`. Verbose (text-keyed) and compact (positional)
// profiles, plus the control plane (service ordinal 0) lifecycle events.
import { array as cborArray, int as cborInt, text as cborText } from "./cbor.js";
import { TransportError, VERSION, canonMap, decodeValue, encodeValue, getInt, getText, getTextOpt, getUint, getUintOpt, intValue, mapGet, tag24, textValue, untag24, } from "./conventions.js";
export function parseProfile(s) {
    return s === "verbose" || s === "compact" ? s : undefined;
}
// Control-plane operation ordinals (under service ordinal 0) and their $-sigil
// verbose names.
export const control = {
    HELLO: 0,
    HELLO_ACK: 1,
    PING: 2,
    PONG: 3,
    CLOSE: 4,
    ERROR: 5,
    HELLO_NAME: "$hello",
    HELLO_ACK_NAME: "$hello-ack",
    PING_NAME: "$ping",
    PONG_NAME: "$pong",
    CLOSE_NAME: "$close",
    ERROR_NAME: "$error",
};
// One typed event flowing in either direction. Identified by service+operation
// (verbose) or by their ordinals (compact); carries an optional correlation `id`
// when it is a request expecting a reply, or that reply.
export class Event {
    service; // CSIL service name (verbose); omitted on a single-service connection
    serviceOrd; // service ordinal (compact); always present in compact frames
    event; // CSIL operation name (verbose)
    opOrd; // operation ordinal (compact)
    id;
    payload;
    constructor(payload) {
        this.payload = payload;
    }
    // A verbose event by name. `service` is omitted on a single-service connection.
    static verbose(service, event, payload) {
        const e = new Event(payload);
        e.service = service;
        e.event = event;
        return e;
    }
    // A compact event by ordinals.
    static compact(serviceOrd, opOrd, payload) {
        const e = new Event(payload);
        e.serviceOrd = serviceOrd;
        e.opOrd = opOrd;
        return e;
    }
    withId(id) {
        this.id = id;
        return this;
    }
    encode(profile) {
        return profile === "verbose" ? this.encodeVerbose() : this.encodeCompact();
    }
    encodeVerbose() {
        if (this.event === undefined) {
            throw TransportError.malformed("verbose event missing 'event' name");
        }
        const entries = [
            ["event", textValue(this.event)],
            ["payload", tag24(this.payload)],
        ];
        if (this.service !== undefined)
            entries.push(["service", textValue(this.service)]);
        if (this.id !== undefined)
            entries.push(["id", intValue(this.id)]);
        return encodeValue(canonMap(entries));
    }
    encodeCompact() {
        if (this.serviceOrd === undefined) {
            throw TransportError.malformed("compact event missing service ordinal");
        }
        if (this.opOrd === undefined) {
            throw TransportError.malformed("compact event missing op ordinal");
        }
        const arr = [cborInt(this.serviceOrd), cborInt(this.opOrd)];
        if (this.id !== undefined)
            arr.push(cborInt(this.id));
        arr.push(tag24(this.payload));
        return encodeValue(cborArray(arr));
    }
    static decode(bytes, profile) {
        return profile === "verbose" ? Event.decodeVerbose(bytes) : Event.decodeCompact(bytes);
    }
    static decodeVerbose(bytes) {
        const v = decodeValue(bytes);
        const payloadField = mapGet(v, "payload");
        if (!payloadField)
            throw TransportError.malformed("missing 'payload'");
        const e = Event.verbose(getTextOpt(v, "service"), getText(v, "event"), untag24(payloadField));
        e.id = getUintOpt(v, "id");
        return e;
    }
    static decodeCompact(bytes) {
        const v = decodeValue(bytes);
        if (v.t !== "array")
            throw TransportError.malformed("compact event is not an array");
        const arr = v.v;
        // 3 elements => [service_ord, op_ord, payload]; 4 => with correlation id.
        let id;
        let payloadVal;
        if (arr.length === 3) {
            payloadVal = arr[2];
        }
        else if (arr.length === 4) {
            id = arr[2];
            payloadVal = arr[3];
        }
        else {
            throw TransportError.malformed(`compact event array has ${arr.length} elements, expected 3 or 4`);
        }
        const asUint = (val) => {
            if (val.t === "int" && val.v >= 0)
                return val.v;
            throw TransportError.malformed("ordinal is not a non-negative integer");
        };
        const e = Event.compact(asUint(arr[0]), asUint(arr[1]), untag24(payloadVal));
        if (id !== undefined)
            e.id = asUint(id);
        return e;
    }
}
// The `$hello` payload offered by the connection initiator.
export class Hello {
    versions;
    profiles;
    service;
    auth;
    constructor(versions, profiles, service, auth) {
        this.versions = versions;
        this.profiles = profiles;
        this.service = service;
        this.auth = auth;
    }
    encode() {
        const entries = [
            ["versions", cborArray(this.versions.map((n) => cborInt(n)))],
            ["profiles", cborArray(this.profiles.map((p) => cborText(p)))],
        ];
        if (this.service !== undefined)
            entries.push(["service", textValue(this.service)]);
        if (this.auth !== undefined)
            entries.push(["auth", textValue(this.auth)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        const versionsField = mapGet(v, "versions");
        if (!versionsField || versionsField.t !== "array") {
            throw TransportError.malformed("hello missing 'versions'");
        }
        const versions = versionsField.v
            .filter((x) => x.t === "int")
            .map((x) => x.v);
        const profilesField = mapGet(v, "profiles");
        if (!profilesField || profilesField.t !== "array") {
            throw TransportError.malformed("hello missing 'profiles'");
        }
        const profiles = profilesField.v
            .filter((x) => x.t === "text")
            .map((x) => x.v);
        return new Hello(versions, profiles, getTextOpt(v, "service"), getTextOpt(v, "auth"));
    }
    // Select a profile from this hello's offers, honoring the peer's preference
    // order and what the server supports. Returns `[version, profile]`, or
    // `undefined` if nothing is mutually supported.
    negotiate(supported) {
        const version = this.versions.find((v) => v === VERSION);
        if (version === undefined)
            return undefined;
        for (const offered of this.profiles) {
            const p = parseProfile(offered);
            if (p && supported.includes(p))
                return [version, p];
        }
        return undefined;
    }
}
// The `$hello-ack` payload returned by the peer.
export class HelloAck {
    v;
    profile;
    session;
    constructor(v, profile, session) {
        this.v = v;
        this.profile = profile;
        this.session = session;
    }
    encode() {
        const entries = [
            ["v", intValue(this.v)],
            ["profile", textValue(this.profile)],
        ];
        if (this.session !== undefined)
            entries.push(["session", textValue(this.session)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        return new HelloAck(getUint(v, "v"), getText(v, "profile"), getTextOpt(v, "session"));
    }
}
// A `$ping`/`$pong` heartbeat payload.
export class Heartbeat {
    nonce;
    at;
    constructor(nonce, at) {
        this.nonce = nonce;
        this.at = at;
    }
    encode() {
        const entries = [["nonce", intValue(this.nonce)]];
        if (this.at !== undefined)
            entries.push(["at", intValue(this.at)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        return new Heartbeat(getUint(v, "nonce"), getUintOpt(v, "at"));
    }
}
// A `$close` payload. `status` is a transport-registry code.
export class Close {
    status;
    reason;
    constructor(status, reason) {
        this.status = status;
        this.reason = reason;
    }
    encode() {
        const entries = [["status", intValue(this.status)]];
        if (this.reason !== undefined)
            entries.push(["reason", textValue(this.reason)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        return new Close(getInt(v, "status"), getTextOpt(v, "reason"));
    }
}
