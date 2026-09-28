package tallyowl

import (
	"crypto/subtle"
	"encoding/hex"
	"fmt"
	"strconv"
	"time"

	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
	"lukechampine.com/blake3"
)

// AlertCallbackWindow is how far a callback's SignedAt may be from the
// receiver's own clock, in either direction. Five minutes covers ordinary
// clock drift. An older callback is a replay, and a newer one is from a wrong
// clock. The head signs each attempt again, so a retry is never too old.
const AlertCallbackWindow = 5 * time.Minute

// AlertCallbackError says why a callback did not verify. Answer it with a
// ServiceError whose code is `unauthenticated` and which is not retryable: the
// head does not send that notification again.
type AlertCallbackError struct {
	// Reason is "signature", "time", or "no-secret".
	Reason  string
	Message string
}

func (e *AlertCallbackError) Error() string { return e.Message }

// VerifyAlertCallback checks one alert callback and returns its body, the
// notification as JSON, when it verifies.
//
// An application that declares TallyOwlAlertReceiver receives each alert as an
// AlertNotifyRequest. The head signs it the way it signs a webhook: a keyed
// BLAKE3 hash, with the secret the alert target names, over the decimal text
// of SignedAt, a full stop, and Body. TLS proves the receiver to the head; this
// signature proves the head to the receiver. Read Body only after this returns
// it. now is the receiver's clock.
func VerifyAlertCallback(secret string, request ingest.AlertNotifyRequest, now time.Time) ([]byte, error) {
	if secret == "" {
		return nil, &AlertCallbackError{
			Reason:  "no-secret",
			Message: "this receiver has no secret, so it cannot verify an alert callback. Give it the secret that the alert target's secret_ref names",
		}
	}
	presented, err := hex.DecodeString(request.Signature)
	expected := alertSignature(secret, int64(request.SignedAt), request.Body)
	// The comparison takes the same time for every wrong signature.
	if err != nil || subtle.ConstantTimeCompare(presented, expected[:]) != 1 {
		return nil, &AlertCallbackError{
			Reason:  "signature",
			Message: "this alert callback is not signed with the secret this receiver holds. Give the receiver the secret that the alert target's secret_ref names",
		}
	}
	skew := now.UnixMilli() - int64(request.SignedAt)
	if skew < 0 {
		skew = -skew
	}
	if skew > AlertCallbackWindow.Milliseconds() {
		return nil, &AlertCallbackError{
			Reason: "time",
			Message: fmt.Sprintf(
				"this alert callback was signed %d seconds from this receiver's clock, and the limit is %d seconds. Check the clocks, or this is a replay",
				skew/1000, int64(AlertCallbackWindow.Seconds())),
		}
	}
	return request.Body, nil
}

// alertSignature is the head's signature for one time and body. The key is
// the BLAKE3 hash of the secret, because a key is exactly 32 bytes and a secret
// is text of any length.
func alertSignature(secret string, signedAt int64, body []byte) [32]byte {
	key := blake3.Sum256([]byte(secret))
	hasher := blake3.New(32, key[:])
	_, _ = hasher.Write([]byte(strconv.FormatInt(signedAt, 10)))
	_, _ = hasher.Write([]byte("."))
	_, _ = hasher.Write(body)
	var out [32]byte
	copy(out[:], hasher.Sum(nil))
	return out
}
