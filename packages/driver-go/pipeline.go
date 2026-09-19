package tallyowl

import (
	"fmt"
	"net"
	"time"

	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// DefaultClientWindow is how many correlated calls a pipelining client keeps
// outstanding. Four covers the measured round trip at the D19 batch defaults.
// It is a window, not a buffer: the driver still refuses at its unacknowledged
// bound.
const DefaultClientWindow = 4

// Pipeline is a pipelining client: several correlated calls outstanding on one
// connection.
//
// Client.Call sends one request and waits for its reply, so a caller with one
// worker is bounded by the round trip rather than by the service. This type
// separates the two halves, so a caller can have a window of calls in flight
// and collect the replies as they arrive.
//
// A reply may arrive out of order. Send gives back the correlation ID and Recv
// reports which ID it answered. docs/DELIVERY.md section 3 permits this, and a
// stable batch ID is what makes it safe: final storage deduplicates, so a retry
// after a lost connection stays one logical commit.
//
// Pipeline is not safe for concurrent use. It alternates between sending and
// receiving on one connection, which needs no background goroutine and no
// shared state, and the window bound is what stops it from sending without
// limit. A caller that wants concurrency guards it, as Driver does.
type Pipeline struct {
	address        string
	credential     *string
	connectTimeout time.Duration
	maxFrameBytes  int
	window         int

	conn     net.Conn
	carrier  *transport.StreamCarrier
	nextID   uint64
	inFlight int
}

// NewPipeline builds a pipeline to address that keeps at most window calls
// outstanding. It connects on the first Send.
func NewPipeline(address string, maxFrameBytes, window int) *Pipeline {
	if window < 1 {
		window = 1
	}
	return &Pipeline{
		address:        address,
		connectTimeout: 5 * time.Second,
		maxFrameBytes:  maxFrameBytes,
		window:         window,
		nextID:         1,
	}
}

// WithCredential presents this credential on every call.
func (p *Pipeline) WithCredential(credential string) *Pipeline {
	if credential != "" {
		p.credential = &credential
	}
	return p
}

// Address is the peer this pipeline reaches.
func (p *Pipeline) Address() string { return p.address }

// InFlight reports how many calls are outstanding.
func (p *Pipeline) InFlight() int { return p.inFlight }

// Window reports how many calls this pipeline keeps outstanding.
func (p *Pipeline) Window() int { return p.window }

// HasRoom reports whether another call fits inside the window.
func (p *Pipeline) HasRoom() bool { return p.inFlight < p.window }

func (p *Pipeline) connect() error {
	conn, err := net.DialTimeout("tcp", p.address, p.connectTimeout)
	if err != nil {
		return fmt.Errorf("we could not reach %s. %w", p.address, err)
	}
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, p.maxFrameBytes)
	if err != nil {
		_ = conn.Close()
		return fmt.Errorf("the connection could not be prepared: %w", err)
	}
	p.conn = conn
	p.carrier = carrier
	return nil
}

// Send sends one call and returns its correlation ID, without waiting for a
// reply.
//
// It does not enforce the window. A caller that wants the window enforced calls
// Recv until HasRoom reports true, which is what gives the caller the chance to
// do something with each reply.
func (p *Pipeline) Send(service, op string, payload []byte) (uint64, error) {
	if p.carrier == nil {
		if err := p.connect(); err != nil {
			return 0, err
		}
	}
	id := p.nextID
	request := transport.NewRpcRequest(service, op, payload).WithID(id)
	request.Auth = p.credential
	frame, err := request.Encode()
	if err != nil {
		return 0, fmt.Errorf("the request could not be encoded: %w", err)
	}
	if err := p.carrier.SendFrame(frame); err != nil {
		// Every outstanding call on this connection now has an unknown fate.
		// The caller retries them by their stable IDs.
		p.Reset()
		return 0, fmt.Errorf("we could not reach %s for `%s`. %w", p.address, op, err)
	}
	p.nextID++
	p.inFlight++
	return id, nil
}

// Recv waits for the next reply, whichever call it answers. It returns a nil
// response when nothing is outstanding.
func (p *Pipeline) Recv() (uint64, *transport.RpcResponse, error) {
	if p.inFlight == 0 {
		return 0, nil, nil
	}
	if p.carrier == nil {
		p.Reset()
		return 0, nil, fmt.Errorf(
			"the connection to %s closed with replies outstanding", p.address)
	}
	frame, err := p.carrier.RecvFrame()
	if err != nil {
		p.Reset()
		return 0, nil, fmt.Errorf("we lost the connection to %s. %w", p.address, err)
	}
	if frame == nil {
		p.Reset()
		return 0, nil, fmt.Errorf(
			"%s closed the connection with replies outstanding", p.address)
	}
	response, err := transport.DecodeRpcResponse(frame)
	if err != nil {
		p.Reset()
		return 0, nil, fmt.Errorf("%s sent a reply we could not read: %w", p.address, err)
	}
	p.inFlight--
	// A reply with no correlation ID cannot be matched to a call. That is a
	// protocol failure rather than an application error, so the connection does
	// not continue.
	if response.ID == nil {
		p.Reset()
		return 0, nil, fmt.Errorf("%s sent a reply with no correlation ID", p.address)
	}
	return *response.ID, &response, nil
}

// Reset drops the connection and forgets what was outstanding. The caller owns
// retrying those calls; a stable batch ID is what makes that safe.
func (p *Pipeline) Reset() {
	if p.conn != nil {
		_ = p.conn.Close()
	}
	p.conn = nil
	p.carrier = nil
	p.inFlight = 0
}
