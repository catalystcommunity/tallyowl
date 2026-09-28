package tallyowl

import (
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"testing"
	"time"

	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
)

// alertVector is golden/alert-callback.json: one callback the head signed. The
// head, this driver, and the Rust app driver each test against it.
type alertVector struct {
	Secret    string `json:"secret"`
	SignedAt  int64  `json:"signed_at"`
	Body      string `json:"body"`
	Signature string `json:"signature"`
	Request   string `json:"request"`
}

func loadAlertVector(t *testing.T) (alertVector, ingest.AlertNotifyRequest) {
	t.Helper()
	raw, err := os.ReadFile("../../golden/alert-callback.json")
	if err != nil {
		t.Fatalf("the shared vector: %v", err)
	}
	var v alertVector
	if err := json.Unmarshal(raw, &v); err != nil {
		t.Fatalf("the shared vector: %v", err)
	}
	encoded, err := hex.DecodeString(v.Request)
	if err != nil {
		t.Fatalf("the request bytes: %v", err)
	}
	request, err := ingest.DecodeAlertNotifyRequest(encoded)
	if err != nil {
		t.Fatalf("the head's request does not decode in Go: %v", err)
	}
	return v, request
}

func TestTheHeadsSignedCallbackVerifiesInGo(t *testing.T) {
	v, request := loadAlertVector(t)
	if string(request.Body) != v.Body || request.Signature != v.Signature || int64(request.SignedAt) != v.SignedAt {
		t.Fatalf("the decoded request is not the vector: %+v", request)
	}
	// A second BLAKE3 implementation, and it agrees with the head's.
	if got := alertSignature(v.Secret, v.SignedAt, []byte(v.Body)); hex.EncodeToString(got[:]) != v.Signature {
		t.Fatalf("Go computes %x, and the head signed %s", got, v.Signature)
	}
	body, err := VerifyAlertCallback(v.Secret, request, time.UnixMilli(v.SignedAt))
	if err != nil {
		t.Fatalf("verify: %v", err)
	}
	if string(body) != v.Body {
		t.Fatalf("the verified body is %q", body)
	}
}

func TestAChangedCallbackOrAWrongSecretDoesNotVerify(t *testing.T) {
	v, request := loadAlertVector(t)
	at := time.UnixMilli(v.SignedAt)
	reason := func(err error) string {
		var failure *AlertCallbackError
		if !errors.As(err, &failure) {
			t.Fatalf("the refusal arrived as %T: %v", err, err)
		}
		return failure.Reason
	}

	changed := request
	changed.Body = append([]byte(nil), request.Body...)
	changed.Body[0] ^= 1
	if _, err := VerifyAlertCallback(v.Secret, changed, at); reason(err) != "signature" {
		t.Error("a changed body verified")
	}
	moved := request
	moved.SignedAt++
	if _, err := VerifyAlertCallback(v.Secret, moved, time.UnixMilli(int64(moved.SignedAt))); reason(err) != "signature" {
		t.Error("a changed time verified, so the time is not inside the signature")
	}
	if _, err := VerifyAlertCallback("another secret", request, at); reason(err) != "signature" {
		t.Error("a wrong secret verified")
	}
	garbled := request
	garbled.Signature = "not hexadecimal at all"
	if _, err := VerifyAlertCallback(v.Secret, garbled, at); reason(err) != "signature" {
		t.Error("a signature that is not hexadecimal verified")
	}
	if _, err := VerifyAlertCallback("", request, at); reason(err) != "no-secret" {
		t.Error("an empty secret was used")
	}
}

func TestACallbackOutsideTheWindowIsRefusedEitherWay(t *testing.T) {
	v, request := loadAlertVector(t)
	signed := time.UnixMilli(v.SignedAt)
	if _, err := VerifyAlertCallback(v.Secret, request, signed.Add(AlertCallbackWindow)); err != nil {
		t.Fatalf("the window is inclusive: %v", err)
	}
	for _, now := range []time.Time{
		signed.Add(AlertCallbackWindow + time.Millisecond),
		signed.Add(-AlertCallbackWindow - time.Millisecond),
	} {
		_, err := VerifyAlertCallback(v.Secret, request, now)
		var failure *AlertCallbackError
		if !errors.As(err, &failure) || failure.Reason != "time" {
			t.Fatalf("a callback %s from its signing time verified: %v", now.Sub(signed), err)
		}
	}
}
