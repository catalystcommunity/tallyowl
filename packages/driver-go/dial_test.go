package tallyowl

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"errors"
	"math/big"
	"net"
	"path/filepath"
	"testing"
	"time"
)

func TestTheDialPlanFollowsD62(t *testing.T) {
	cases := []struct {
		name      string
		address   string
		transport Transport
		network   string
		secure    bool
		server    string
	}{
		{"a unix socket is plaintext", "unix:/run/collector.sock", Transport{}, "unix", false, ""},
		{"loopback is plaintext", "127.0.0.1:5100", Transport{}, "tcp", false, ""},
		{"IPv6 loopback is plaintext", "[::1]:5100", Transport{}, "tcp", false, ""},
		{"localhost is plaintext", "localhost:5100", Transport{}, "tcp", false, ""},
		{"a network address is TLS by default", "collector.internal:5100", Transport{}, "tcp", true, "collector.internal"},
		{"a private address is still a network address", "10.0.0.7:5100", Transport{}, "tcp", true, "10.0.0.7"},
		{"plaintext to a network address needs the setting", "collector.internal:5100", Transport{AllowPlaintext: true}, "tcp", false, ""},
		{"a TLS setting applies even on loopback", "127.0.0.1:5100", Transport{TLS: &tls.Config{ServerName: "collector"}}, "tcp", true, "collector"},
	}
	for _, c := range cases {
		plan, err := planDial(c.address, c.transport)
		if err != nil {
			t.Errorf("%s: %v", c.name, err)
			continue
		}
		if plan.network != c.network || (plan.tls != nil) != c.secure {
			t.Errorf("%s: got network %s, TLS %v", c.name, plan.network, plan.tls != nil)
		}
		if c.secure && plan.tls.ServerName != c.server {
			t.Errorf("%s: verified against %q, want %q", c.name, plan.tls.ServerName, c.server)
		}
		if c.secure && plan.tls.MinVersion < tls.VersionTLS12 {
			t.Errorf("%s: accepts a protocol older than TLS 1.2", c.name)
		}
	}
	for _, address := range []string{"unix:", "collector-with-no-port"} {
		if _, err := planDial(address, Transport{}); err == nil {
			t.Errorf("%q should be refused as an address", address)
		}
	}
}

func TestADriverSettingIsNeverChangedByTheDialPlan(t *testing.T) {
	mine := &tls.Config{}
	if _, err := planDial("collector.internal:5100", Transport{TLS: mine}); err != nil {
		t.Fatal(err)
	}
	if mine.ServerName != "" || mine.MinVersion != 0 {
		t.Error("the plan wrote into the host's own TLS configuration")
	}
}

// authority makes a certificate authority and a server certificate for
// 127.0.0.1 that it signed.
func authority(t *testing.T) (*x509.CertPool, tls.Certificate) {
	t.Helper()
	caKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	caTemplate := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "test authority"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(time.Hour),
		IsCA:                  true,
		KeyUsage:              x509.KeyUsageCertSign,
		BasicConstraintsValid: true,
	}
	caDER, err := x509.CreateCertificate(rand.Reader, caTemplate, caTemplate, &caKey.PublicKey, caKey)
	if err != nil {
		t.Fatal(err)
	}
	caCert, _ := x509.ParseCertificate(caDER)
	leafKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	leafTemplate := &x509.Certificate{
		SerialNumber: big.NewInt(2),
		Subject:      pkix.Name{CommonName: "collector"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(time.Hour),
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	leafDER, err := x509.CreateCertificate(rand.Reader, leafTemplate, caCert, &leafKey.PublicKey, caKey)
	if err != nil {
		t.Fatal(err)
	}
	pool := x509.NewCertPool()
	pool.AddCert(caCert)
	return pool, tls.Certificate{Certificate: [][]byte{leafDER}, PrivateKey: leafKey}
}

func startTLSCollector(t *testing.T, certificate tls.Certificate) *fakeCollector {
	t.Helper()
	listener, err := tls.Listen("tcp", "127.0.0.1:0", &tls.Config{Certificates: []tls.Certificate{certificate}})
	if err != nil {
		t.Fatal(err)
	}
	fake := &fakeCollector{listener: listener}
	go fake.accept()
	t.Cleanup(func() { _ = listener.Close() })
	return fake
}

func TestABatchCrossesTLSToACollectorThatProvesItself(t *testing.T) {
	roots, certificate := authority(t)
	fake := startTLSCollector(t, certificate)
	settings := NewSettings(fake.address(), "key-a")
	settings.Transport = Transport{TLS: &tls.Config{RootCAs: roots}}
	driver := NewDriver(settings)
	captureEvents(t, driver, 2)

	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("flush over TLS: %v", err)
	}
	if receipt.Accepted != 2 || len(fake.received()) != 1 {
		t.Fatalf("the collector saw %d batches, receipt %+v", len(fake.received()), receipt)
	}

	// The pipelined path dials on its own, so it is proved separately.
	captureEvents(t, driver, 2)
	if _, err := driver.Submit(); err != nil {
		t.Fatalf("submit over TLS: %v", err)
	}
	if receipts, err := driver.Drain(); err != nil || len(receipts) != 1 {
		t.Fatalf("drain over TLS: %v, %d receipts", err, len(receipts))
	}
}

func TestACollectorNoTrustedAuthoritySignedIsRefusedAndTheBatchIsKept(t *testing.T) {
	_, certificate := authority(t)
	fake := startTLSCollector(t, certificate)
	settings := NewSettings(fake.address(), "key-a")
	settings.Transport = Transport{TLS: &tls.Config{}} // the system roots only
	settings.MaxBatchAttempts = 1
	driver := NewDriver(settings)
	onFakeTime(driver)
	captureEvents(t, driver, 3)

	_, err := driver.Flush()
	var handshake *HandshakeError
	if !errors.As(err, &handshake) {
		t.Fatalf("the refusal arrived as %T: %v", err, err)
	}
	if !contains(err.Error(), "no trusted authority") || !contains(err.Error(), "RootCAs") {
		t.Errorf("the message does not say what to do: %v", err)
	}
	stats := driver.Stats()
	if stats.Lost != 0 || stats.Unacknowledged != 3 {
		t.Fatalf("a handshake sends nothing, so nothing is lost and no attempt is used: %+v", stats)
	}
	if len(fake.received()) != 0 {
		t.Fatal("a batch reached a collector that did not prove itself")
	}
}

func TestADriverThatExpectsTLSSaysSoWhenTheCollectorHasNone(t *testing.T) {
	fake := startFakeCollector(t) // plaintext
	settings := NewSettings(fake.address(), "key-a")
	settings.Transport = Transport{TLS: &tls.Config{InsecureSkipVerify: false}}
	driver := NewDriver(settings)
	onFakeTime(driver)
	captureEvents(t, driver, 1)

	_, err := driver.Flush()
	if err == nil || !contains(err.Error(), "not use TLS") || !contains(err.Error(), "AllowPlaintext") {
		t.Fatalf("the message should name the mismatch: %v", err)
	}
}

func TestABatchReachesACollectorOnAUnixSocket(t *testing.T) {
	path := filepath.Join(t.TempDir(), "collector.sock")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Skipf("this host has no unix sockets: %v", err)
	}
	fake := &fakeCollector{listener: listener}
	go fake.accept()
	t.Cleanup(func() { _ = listener.Close() })

	driver := NewDriver(NewSettings("unix:"+path, "key-a"))
	captureEvents(t, driver, 2)
	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("flush over a unix socket: %v", err)
	}
	if receipt.Accepted != 2 {
		t.Fatalf("the receipt reports %d of 2", receipt.Accepted)
	}
}
