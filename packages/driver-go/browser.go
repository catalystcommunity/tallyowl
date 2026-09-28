package tallyowl

import (
	"crypto/sha256"
	"fmt"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
)

// BrowserOption changes how FromBrowser reads one browser item.
type BrowserOption func(*browserOptions)

type browserOptions struct {
	keepEventID bool
}

// KeepBrowserEventID keeps the event ID exactly as the browser sent it.
//
// A browser is not trusted, and deduplication has project scope, so one browser
// that sends an ID early suppresses a later legitimate event that carries the
// same ID. Keep the ID only when the producer is trusted, as a test simulator
// is. See docs/DELIVERY.md section 2.
func KeepBrowserEventID() BrowserOption {
	return func(o *browserOptions) { o.keepEventID = true }
}

// FromBrowser turns one item a browser sent on the application's own connection
// into a capture this driver sends on.
//
// It takes every kind of item and every envelope field the browser package
// produces. The two generated packages hold their own copies of the same shared
// types, and the golden vectors prove the copies encode identically, so the
// conversion goes through the encoding. A kind that is added to the contract
// arrives here with no change to this function.
//
// What a browser says about itself is not trusted:
//
//   - The tenancy and the receive time are discarded. The collector stamps them.
//   - Every property arrives with a `client` origin, whatever origin it claimed.
//   - The event ID is namespaced to the browser's session, or to its anonymous
//     ID when there is no session, so one browser cannot suppress the events of
//     another. The same browser item maps to the same ID every time, which
//     keeps a browser retry one logical event. An item with neither a session
//     nor an anonymous ID gets a new ID.
func FromBrowser(item ingest.TelemetryItem, options ...BrowserOption) (*Capture, error) {
	var chosen browserOptions
	for _, option := range options {
		option(&chosen)
	}

	converted, err := api.DecodeTelemetryItem(ingest.EncodeTelemetryItem(item))
	if err != nil {
		return nil, fmt.Errorf("this browser item was not recorded. It could not be read: %w", err)
	}
	if _, err := payloadName(converted); err != nil {
		return nil, fmt.Errorf("this browser item was not recorded: %w", err)
	}

	envelope := &converted.Envelope
	envelope.ReceivedAt = nil
	envelope.WorkspaceId = nil
	envelope.ProjectId = nil
	envelope.SourceId = nil
	for i := range envelope.Properties {
		envelope.Properties[i].Origin = "client"
	}
	if envelope.Properties == nil {
		envelope.Properties = api.PropertyList{}
	}
	if !chosen.keepEventID {
		envelope.EventId = namespacedEventID(envelope)
	}
	// The same priority rule the driver's own constructors apply: a conversion
	// and an unhandled error seal the batch. See docs/DELIVERY.md section 8.
	critical := converted.Conversion != nil ||
		(converted.Error != nil && !converted.Error.Handled)
	return &Capture{item: converted, critical: critical}, nil
}

// namespacedEventID derives the ID a browser item travels under from who sent
// it and the ID they gave it.
func namespacedEventID(envelope *api.Envelope) []byte {
	scope := ""
	switch {
	case envelope.SessionId != nil && *envelope.SessionId != "":
		scope = "session:" + string(*envelope.SessionId)
	case envelope.AnonymousId != nil && *envelope.AnonymousId != "":
		scope = "anonymous:" + *envelope.AnonymousId
	default:
		return NewEventID()
	}
	hash := sha256.New()
	hash.Write([]byte(scope))
	hash.Write([]byte{0})
	hash.Write(envelope.EventId)
	id := hash.Sum(nil)[:16]
	// Version 8 is the UUID form for an ID an application derives itself.
	id[6] = (id[6] & 0x0f) | 0x80
	id[8] = (id[8] & 0x3f) | 0x80
	return id
}
