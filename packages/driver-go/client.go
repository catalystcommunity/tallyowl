package tallyowl

import (
	"fmt"
	"net"
	"sync"
	"time"

	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// Client is a persistent, reconnecting CSIL-RPC client for one address.
//
// The connection is the unit of reconnection, not the call. A call that fails on
// a broken connection is retried once on a fresh one, because a peer that
// restarted is the ordinary case and a caller should not have to write that
// loop. A call that fails twice returns an error, and the caller decides what to
// do next.
//
// Native TallyOwl server-to-server traffic uses CSIL over TCP. There is no
// generic HTTP ingest API here, and there is not going to be one.
type Client struct {
	address        string
	maxFrameBytes  int
	connectTimeout time.Duration
	// callTimeout bounds one send and its reply. Zero means no bound, which is
	// what a collector that accepts a connection and never answers turns into a
	// caller that never returns.
	callTimeout time.Duration
	security    Transport

	mu     sync.Mutex
	conn   net.Conn
	client *transport.RpcClient
}

// NewClient builds a client that connects on its first call.
func NewClient(address string, maxFrameBytes int) *Client {
	return NewClientWith(address, maxFrameBytes, Transport{})
}

// NewClientWith builds a client that reaches the address by the given transport.
func NewClientWith(address string, maxFrameBytes int, security Transport) *Client {
	return &Client{
		address:        address,
		maxFrameBytes:  maxFrameBytes,
		connectTimeout: 5 * time.Second,
		security:       security,
	}
}

// Address is the collector this client reaches.
func (c *Client) Address() string { return c.address }

// SetCallTimeout bounds every later call: the connect, the send, and the wait
// for the reply. A shutdown lowers it to the time it has left.
func (c *Client) SetCallTimeout(timeout time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.callTimeout = timeout
}

// UnreachableError reports that no connection opened, so nothing was sent. A
// caller that counts attempts against a batch does not count this one, because
// the batch never left.
type UnreachableError struct {
	Address string
	Err     error
}

func (e *UnreachableError) Error() string {
	return fmt.Sprintf("we could not reach %s. %v", e.Address, e.Err)
}

func (e *UnreachableError) Unwrap() error { return e.Err }

// dialTimeout is the shorter of the connect timeout and the call timeout, so a
// short call deadline is not spent waiting on a connect.
func dialTimeout(connect, call time.Duration) time.Duration {
	if call > 0 && call < connect {
		return call
	}
	return connect
}

func (c *Client) connect() error {
	conn, err := dial(c.address, c.security, dialTimeout(c.connectTimeout, c.callTimeout))
	if err != nil {
		return err
	}
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, c.maxFrameBytes)
	if err != nil {
		_ = conn.Close()
		return fmt.Errorf("the connection could not be prepared: %w", err)
	}
	c.conn = conn
	// Correlated batches pipeline on one connection, so every request carries
	// an id.
	c.client = transport.NewRpcClient(carrier, true)
	return nil
}

func (c *Client) dropLocked() {
	if c.conn != nil {
		_ = c.conn.Close()
	}
	c.conn = nil
	c.client = nil
}

// Call invokes service/op. The reply carries its variant, so the caller can
// tell a typed result from a typed error.
func (c *Client) Call(service, op string, payload []byte, auth *string) (transport.RpcResponse, error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	var last error
	for attempt := 0; attempt < 2; attempt++ {
		if c.client == nil {
			if err := c.connect(); err != nil {
				if last != nil {
					// The first attempt sent the call and then lost the
					// connection, so whether the peer took it is unknown. The
					// failed reconnect does not change that.
					return transport.RpcResponse{}, fmt.Errorf(
						"we lost the connection to %s during `%s`, and could not open another. %w",
						c.address, op, last)
				}
				return transport.RpcResponse{}, err
			}
		}
		if c.callTimeout > 0 {
			_ = c.conn.SetDeadline(time.Now().Add(c.callTimeout))
		}
		response, err := c.client.Call(service, op, payload, auth)
		if err == nil {
			return response, nil
		}
		// The connection is now suspect either way. Drop it, so the retry runs
		// on a fresh one and a later call does not inherit a half-read frame.
		c.dropLocked()
		last = err
	}
	return transport.RpcResponse{}, fmt.Errorf(
		"we could not reach %s for `%s`. %w", c.address, op, last)
}

// Close forgets the current connection. The next call opens a fresh one.
func (c *Client) Close() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.dropLocked()
}
