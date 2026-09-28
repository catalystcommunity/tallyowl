package tallyowl

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"io"
	"net"
	"strings"
	"syscall"
	"time"
)

// Transport is how the driver reaches the collector. The zero value is the
// secure default, D62:
//
//   - a `unix:<path>` address or a loopback address is reached in plaintext,
//     because nothing crosses a network;
//   - any other address is reached over TLS and the collector's certificate is
//     checked against the system's trusted authorities;
//   - plaintext to any other address needs AllowPlaintext.
//
// The project key is still how the application proves itself. TLS is how the
// collector proves itself, and it keeps the key off the network in the clear.
type Transport struct {
	// TLS replaces the default TLS configuration. Set RootCAs when the
	// collector's certificate comes from a private authority. Set ServerName
	// when the address is not the name on the certificate.
	TLS *tls.Config
	// AllowPlaintext sends plaintext to an address that is not loopback. Use it
	// only on a network that something else protects.
	AllowPlaintext bool
}

// dialPlan is what one address and one transport setting mean.
type dialPlan struct {
	network string
	address string
	tls     *tls.Config
}

// planDial decides how to reach an address. It opens nothing, so the whole
// decision is testable without a network.
func planDial(address string, transport Transport) (dialPlan, error) {
	if path, ok := strings.CutPrefix(address, "unix:"); ok {
		if path == "" {
			return dialPlan{}, errors.New("the address `unix:` names no socket file. Write it as unix:/path/to/socket")
		}
		return dialPlan{network: "unix", address: path, tls: transport.TLS}, nil
	}
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return dialPlan{}, fmt.Errorf("the collector address %q is not host:port or unix:<path>. %w", address, err)
	}
	plan := dialPlan{network: "tcp", address: address}
	switch {
	case transport.TLS != nil:
		plan.tls = transport.TLS.Clone()
	case isLoopback(host) || transport.AllowPlaintext:
		return plan, nil
	default:
		plan.tls = &tls.Config{}
	}
	if plan.tls.ServerName == "" {
		plan.tls.ServerName = host
	}
	if plan.tls.MinVersion == 0 {
		plan.tls.MinVersion = tls.VersionTLS12
	}
	return plan, nil
}

func isLoopback(host string) bool {
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

// dial opens one connection by the plan, and finishes the TLS handshake inside
// the same timeout.
func dial(address string, transport Transport, timeout time.Duration) (net.Conn, error) {
	plan, err := planDial(address, transport)
	if err != nil {
		return nil, &UnreachableError{Address: address, Err: err}
	}
	conn, err := net.DialTimeout(plan.network, plan.address, timeout)
	if err != nil {
		return nil, &UnreachableError{Address: address, Err: err}
	}
	if plan.tls == nil {
		return conn, nil
	}
	secured := tls.Client(conn, plan.tls)
	if timeout > 0 {
		_ = secured.SetDeadline(time.Now().Add(timeout))
	}
	if err := secured.Handshake(); err != nil {
		_ = conn.Close()
		return nil, &HandshakeError{Address: address, Err: err}
	}
	_ = secured.SetDeadline(time.Time{})
	return secured, nil
}

// HandshakeError reports that the collector answered and could not prove who
// it is, or would not agree a secure connection. Nothing was sent. Trying again
// does not help until a setting changes, so a driver waits its full backoff.
type HandshakeError struct {
	Address string
	Err     error
}

func (e *HandshakeError) Error() string {
	var unknown x509.UnknownAuthorityError
	var hostname x509.HostnameError
	var invalid x509.CertificateInvalidError
	switch {
	case errors.As(e.Err, &unknown):
		return fmt.Sprintf(
			"the collector at %s showed a certificate that no trusted authority signed. If the collector uses a private authority, put that authority's certificate in Transport.TLS.RootCAs",
			e.Address)
	case errors.As(e.Err, &hostname):
		return fmt.Sprintf(
			"the collector at %s showed a certificate for a different name (%v). Set Transport.TLS.ServerName to the name on the certificate, or use that name in the address",
			e.Address, e.Err)
	case errors.As(e.Err, &invalid):
		return fmt.Sprintf(
			"the collector at %s showed a certificate that is not valid now (%v). Check the clock on this host, and ask the operator to renew the certificate",
			e.Address, e.Err)
	}
	if strings.Contains(e.Err.Error(), "first record does not look like a TLS handshake") {
		return fmt.Sprintf(
			"the collector at %s does not use TLS on this address. Give the collector certificates, or set Transport.AllowPlaintext if something else protects this network",
			e.Address)
	}
	// A plaintext listener cannot read a TLS greeting, and it usually closes the
	// connection rather than answering, so this is the likely cause but not a
	// certain one.
	if errors.Is(e.Err, io.EOF) || errors.Is(e.Err, syscall.ECONNRESET) {
		return fmt.Sprintf(
			"the collector at %s closed the connection during the TLS handshake. It probably does not use TLS on this address. Give the collector certificates, or set Transport.AllowPlaintext if something else protects this network",
			e.Address)
	}
	return fmt.Sprintf("a secure connection to the collector at %s could not be made. %v", e.Address, e.Err)
}

func (e *HandshakeError) Unwrap() error { return e.Err }
