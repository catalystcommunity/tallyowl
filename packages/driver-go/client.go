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

	mu     sync.Mutex
	conn   net.Conn
	client *transport.RpcClient
}

// NewClient builds a client that connects on its first call.
func NewClient(address string, maxFrameBytes int) *Client {
	return &Client{
		address:        address,
		maxFrameBytes:  maxFrameBytes,
		connectTimeout: 5 * time.Second,
	}
}

// Address is the collector this client reaches.
func (c *Client) Address() string { return c.address }

func (c *Client) connect() error {
	conn, err := net.DialTimeout("tcp", c.address, c.connectTimeout)
	if err != nil {
		return fmt.Errorf("we could not reach %s. %w", c.address, err)
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
				return transport.RpcResponse{}, err
			}
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
