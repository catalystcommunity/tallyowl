package tallyowl

// The pipelined path, against a collector that answers correlated calls
// concurrently.
//
// The fake here is a collector, not a mock of the driver's own seam. It decodes
// a real SubmitBatchRequest off a real CSIL-RPC frame and answers with a real
// SubmitBatchResponse, and it takes a configured time to do it, because the
// durable write is what a synchronous driver waits behind.

import (
	"net"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// pipelinedCollector answers correlated requests concurrently, the way
// tallyowl-rpc serves them. A collector that answered one at a time would make
// pipelining look worthless, which is the mistake this test exists to avoid.
type pipelinedCollector struct {
	listener net.Listener
	delay    time.Duration
	served   atomic.Int64
	peak     atomic.Int64
	live     atomic.Int64
	maxServe int
}

func startPipelinedCollector(t *testing.T, delay time.Duration, maxServe int) *pipelinedCollector {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("the fake collector could not listen: %v", err)
	}
	c := &pipelinedCollector{listener: listener, delay: delay, maxServe: maxServe}
	go c.accept()
	t.Cleanup(func() { _ = listener.Close() })
	return c
}

func (c *pipelinedCollector) address() string { return c.listener.Addr().String() }

func (c *pipelinedCollector) accept() {
	for {
		conn, err := c.listener.Accept()
		if err != nil {
			return
		}
		go c.serve(conn)
	}
}

func (c *pipelinedCollector) serve(conn net.Conn) {
	defer conn.Close()
	carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, 16*1024*1024)
	if err != nil {
		return
	}
	var writeMu sync.Mutex
	permits := make(chan struct{}, c.maxServe)
	var work sync.WaitGroup
	defer work.Wait()

	for {
		frame, err := carrier.RecvFrame()
		if err != nil || frame == nil {
			return
		}
		request, err := transport.DecodeRpcRequest(frame)
		if err != nil {
			return
		}
		permits <- struct{}{}
		work.Add(1)
		go func(request transport.RpcRequest) {
			defer work.Done()
			defer func() { <-permits }()

			now := c.live.Add(1)
			for {
				peak := c.peak.Load()
				if now <= peak || c.peak.CompareAndSwap(peak, now) {
					break
				}
			}
			decoded, decodeErr := collector.DecodeSubmitBatchRequest(request.Payload)
			time.Sleep(c.delay)
			c.live.Add(-1)
			c.served.Add(1)
			if decodeErr != nil {
				return
			}

			response := transport.RpcResponse{
				ID:     request.ID,
				Status: transport.StatusOk,
				Payload: collector.EncodeSubmitBatchResponse(collector.SubmitBatchResponse{
					BatchId:       decoded.Batch.BatchId,
					Accepted:      uint64(len(decoded.Batch.Items)),
					DurableCopies: 1,
					QueuedAt:      collector.Timestamp(nowMs()),
				}),
			}
			variant := "SubmitBatchResponse"
			response.Variant = &variant
			out, encodeErr := response.Encode()
			if encodeErr != nil {
				return
			}
			writeMu.Lock()
			_ = carrier.SendFrame(out)
			writeMu.Unlock()
		}(request)
	}
}

func pipelinedDriver(address string, inFlight int) *Driver {
	settings := NewSettings(address, "key-a").WithMaxInFlightBatches(inFlight)
	settings.MaxItems = 2
	settings.Linger = time.Hour
	return NewDriver(settings)
}

func TestASubmittedBatchIsAcknowledgedAndTheReceiptNamesIt(t *testing.T) {
	fake := startPipelinedCollector(t, time.Millisecond, 8)
	driver := pipelinedDriver(fake.address(), 4)

	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	receipts, err := driver.Submit()
	if err != nil {
		t.Fatalf("submit: %v", err)
	}
	if len(receipts) != 0 {
		t.Fatalf("nothing had finished yet, got %d receipts", len(receipts))
	}
	if driver.Outstanding() != 1 {
		t.Fatalf("one batch should be outstanding, got %d", driver.Outstanding())
	}

	drained, err := driver.Drain()
	if err != nil {
		t.Fatalf("drain: %v", err)
	}
	if len(drained) != 1 || drained[0].Accepted != 1 || drained[0].DurableCopies != 1 {
		t.Fatalf("unexpected receipts: %+v", drained)
	}
	if driver.Outstanding() != 0 {
		t.Fatalf("nothing should be outstanding, got %d", driver.Outstanding())
	}
}

func TestSubmittingAnEmptyBufferSendsNothing(t *testing.T) {
	fake := startPipelinedCollector(t, time.Millisecond, 8)
	driver := pipelinedDriver(fake.address(), 4)

	receipts, err := driver.Submit()
	if err != nil || len(receipts) != 0 {
		t.Fatalf("an empty buffer should send nothing: %v %+v", err, receipts)
	}
	if fake.served.Load() != 0 {
		t.Fatalf("the collector saw %d batches", fake.served.Load())
	}
}

func TestFourBatchesInFlightBeatFourSentOneAtATime(t *testing.T) {
	// The whole point of the change. Each batch costs the collector 80 ms, so
	// four in sequence cannot finish in under 320 ms and four in flight should
	// finish in little more than 80.
	const delay = 80 * time.Millisecond

	sequentialFake := startPipelinedCollector(t, delay, 8)
	synchronous := pipelinedDriver(sequentialFake.address(), 1)
	started := time.Now()
	for i := 0; i < 4; i++ {
		mustCapture(t, synchronous)
		if _, err := synchronous.Flush(); err != nil {
			t.Fatalf("flush: %v", err)
		}
	}
	sequential := time.Since(started)

	pipelinedFake := startPipelinedCollector(t, delay, 8)
	pipelined := pipelinedDriver(pipelinedFake.address(), 4)
	started = time.Now()
	for i := 0; i < 4; i++ {
		mustCapture(t, pipelined)
		if _, err := pipelined.Submit(); err != nil {
			t.Fatalf("submit: %v", err)
		}
	}
	receipts, err := pipelined.Drain()
	if err != nil {
		t.Fatalf("drain: %v", err)
	}
	overlapped := time.Since(started)

	if len(receipts) != 4 {
		t.Fatalf("every batch should be acknowledged, got %d", len(receipts))
	}
	if sequential < 4*delay {
		t.Fatalf("the sequential run should pay for each batch: %v", sequential)
	}
	if overlapped >= sequential/2 {
		t.Fatalf("pipelining gained nothing: %v against %v", overlapped, sequential)
	}
}

func TestTheWindowBoundsWhatIsOutstandingAndEveryBatchIsStillAcknowledged(t *testing.T) {
	fake := startPipelinedCollector(t, 20*time.Millisecond, 8)
	driver := pipelinedDriver(fake.address(), 2)

	var receipts []*Receipt
	for i := 0; i < 6; i++ {
		mustCapture(t, driver)
		got, err := driver.Submit()
		if err != nil {
			t.Fatalf("submit: %v", err)
		}
		receipts = append(receipts, got...)
		if driver.Outstanding() > 2 {
			t.Fatalf("the window was exceeded: %d", driver.Outstanding())
		}
	}
	drained, err := driver.Drain()
	if err != nil {
		t.Fatalf("drain: %v", err)
	}
	receipts = append(receipts, drained...)

	if len(receipts) != 6 {
		t.Fatalf("every batch should be acknowledged exactly once, got %d", len(receipts))
	}
	distinct := map[string]bool{}
	for _, receipt := range receipts {
		distinct[string(receipt.BatchID)] = true
	}
	if len(distinct) != 6 {
		t.Fatalf("each receipt should name a different batch, got %d", len(distinct))
	}
	if fake.served.Load() != 6 {
		t.Fatalf("the collector saw %d batches", fake.served.Load())
	}
}

func TestAShutdownWaitsForWhatWasAlreadySent(t *testing.T) {
	// A shutdown that ignored the pipeline would report zero unsent items while
	// several batches were still waiting for an acknowledgement.
	fake := startPipelinedCollector(t, 30*time.Millisecond, 8)
	driver := pipelinedDriver(fake.address(), 4)

	for i := 0; i < 3; i++ {
		mustCapture(t, driver)
		if _, err := driver.Submit(); err != nil {
			t.Fatalf("submit: %v", err)
		}
	}
	if left := driver.Shutdown(); left != 0 {
		t.Fatalf("nothing should have been left behind, got %d", left)
	}
	if fake.served.Load() != 3 {
		t.Fatalf("every batch should have reached the collector, got %d", fake.served.Load())
	}
}

func TestACollectorThatNeverAnswersReportsTheLossRatherThanHanging(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxItems = 1
	settings.MaxBatchAttempts = 2
	driver := NewDriver(settings)

	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Submit(); err == nil {
		t.Fatal("a closed port should refuse rather than hang")
	}
}

func mustCapture(t *testing.T, driver *Driver) {
	t.Helper()
	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if err := driver.Capture(Event("b")); err != nil {
		t.Fatalf("capture: %v", err)
	}
}
