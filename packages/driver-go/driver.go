package tallyowl

import (
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// Settings holds the driver configuration. The defaults are D19's, and every
// one of them is configurable.
//
//	Seal a batch at                        256 items
//	Seal a batch at                        512 KiB
//	Seal a batch after                     100 ms
//	Maximum ordinary frame                 1 MiB
//	Unacknowledged data on one connection  8 MiB
//	Shutdown flush deadline                2 s
type Settings struct {
	CollectorAddress string
	Credential       string

	MaxItems               int
	MaxBatchBytes          int
	Linger                 time.Duration
	MaxFrameBytes          int
	MaxUnacknowledgedBytes int
	ShutdownFlushDeadline  time.Duration

	// MaxInFlightBatches is how many sealed batches may be outstanding at one
	// time on the pipelined path. docs/DELIVERY.md section 3 permits "a
	// configured number of correlated batch calls"; this is that number.
	//
	// One reproduces the synchronous behaviour. The default is four, which is
	// what Submit needs to stop being bounded by the round trip.
	MaxInFlightBatches int
	// MaxBatchAttempts is how many times a batch is sent again after a
	// connection failure before the driver reports it as lost.
	MaxBatchAttempts int

	// Properties this driver adds to every item. Their origin is `driver`.
	Properties map[string]string
}

// NewSettings returns the D19 defaults for one collector and one credential.
func NewSettings(collectorAddress, credential string) Settings {
	return Settings{
		CollectorAddress:       collectorAddress,
		Credential:             credential,
		MaxItems:               256,
		MaxBatchBytes:          512 * 1024,
		Linger:                 100 * time.Millisecond,
		MaxFrameBytes:          1024 * 1024,
		MaxUnacknowledgedBytes: 8 * 1024 * 1024,
		ShutdownFlushDeadline:  2 * time.Second,
		MaxInFlightBatches:     DefaultClientWindow,
		MaxBatchAttempts:       3,
		Properties:             map[string]string{},
	}
}

// WithMaxInFlightBatches sets how many sealed batches may be outstanding at one
// time. One reproduces the synchronous behaviour of Flush.
func (s Settings) WithMaxInFlightBatches(batches int) Settings {
	if batches < 1 {
		batches = 1
	}
	s.MaxInFlightBatches = batches
	return s
}

// WithProperty adds a driver-origin property to every item.
func (s Settings) WithProperty(key, value string) Settings {
	next := map[string]string{}
	for k, v := range s.Properties {
		next[k] = v
	}
	next[key] = value
	s.Properties = next
	return s
}

// Receipt is what one flush produced.
type Receipt struct {
	BatchID  []byte
	Accepted uint64
	// DurableCopies is what the collector reported. A caller that needs a
	// stronger boundary reads this rather than assuming one.
	DurableCopies uint64
	Rejected      []RejectedItem
}

// RejectedItem names one item the collector would not take, and why.
type RejectedItem struct {
	EventID []byte
	Code    string
	Message string
}

// ErrBackpressure is returned when the unacknowledged bound is reached. The
// item was not recorded, and the driver says so rather than reporting success
// for data it discarded.
var ErrBackpressure = errors.New(
	"this application is producing telemetry faster than TallyOwl is accepting it")

// ServiceError is a typed rejection from the collector. Retryable is a fact: a
// caller builds automation on it, so a wrong value produces either a retry
// storm or lost data.
type ServiceError struct {
	Code      string
	Message   string
	Retryable bool
}

func (e *ServiceError) Error() string { return e.Message }

// Driver is the Go app driver.
type Driver struct {
	settings Settings
	client   *Client
	sequence atomic.Uint64

	mu       sync.Mutex
	items    []api.TelemetryItem
	bytes    int
	openedAt time.Time
	sealNow  bool

	// The pipelined path. It opens on the first Submit and stays closed for an
	// application that only ever calls Flush, so an application keeps one
	// connection either way.
	outboxMu sync.Mutex
	pipeline *Pipeline
	pending  map[uint64]*pendingBatch
}

// pendingBatch is one sealed batch, retained until it is acknowledged.
//
// D5: "The app retains the stable batch until that acknowledgement, then moves
// on." Retaining the encoded frame is what lets a connection failure be a
// resend rather than a loss, and the batch ID does not change across it, so
// final storage still deduplicates to one logical commit.
type pendingBatch struct {
	batchID []byte
	encoded []byte
	// items is how many items this batch holds, so a shutdown reports unsent
	// items rather than unsent batches.
	items    int
	attempts int
}

// NewDriver builds a driver that reaches one collector.
func NewDriver(settings Settings) *Driver {
	return &Driver{
		settings: settings,
		client:   NewClient(settings.CollectorAddress, settings.MaxFrameBytes),
	}
}

// Settings reports the configuration this driver runs with.
func (d *Driver) Settings() Settings { return d.settings }

// Capture buffers one item. This does not reach the collector, and it makes no
// durability claim.
//
// It returns a backpressure error rather than accepting data it would then
// drop, because a driver that reports success for a discarded event makes every
// count downstream wrong in a way nobody can find.
func (d *Driver) Capture(capture *Capture) error {
	item := capture.item
	seq := d.sequence.Add(1) - 1
	item.Envelope.Sequence = &seq
	for key, value := range d.settings.Properties {
		item.Envelope.Properties = append(
			item.Envelope.Properties, Property(key, Text(value), "driver"))
	}
	if _, err := payloadName(item); err != nil {
		return fmt.Errorf("this event was not recorded: %w", err)
	}

	size := len(api.EncodeTelemetryItem(item))

	d.mu.Lock()
	defer d.mu.Unlock()
	if d.bytes+size > d.settings.MaxUnacknowledgedBytes {
		return ErrBackpressure
	}
	if d.openedAt.IsZero() {
		d.openedAt = time.Now()
	}
	d.bytes += size
	d.items = append(d.items, item)
	if capture.critical {
		d.sealNow = true
	}
	return nil
}

// Buffered reports how many items are waiting.
func (d *Driver) Buffered() int {
	d.mu.Lock()
	defer d.mu.Unlock()
	return len(d.items)
}

// ShouldFlush reports whether the buffer has reached a seal condition.
func (d *Driver) ShouldFlush() bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.sealReached()
}

func (d *Driver) sealReached() bool {
	if len(d.items) == 0 {
		return false
	}
	if d.sealNow || len(d.items) >= d.settings.MaxItems {
		return true
	}
	if d.bytes >= d.settings.MaxBatchBytes {
		return true
	}
	return !d.openedAt.IsZero() && time.Since(d.openedAt) >= d.settings.Linger
}

// NewBatchID makes the identifier a batch keeps across every retry and across
// a lost connection, so final storage deduplicates it to one logical commit.
func NewBatchID() []byte { return NewEventID() }

// Flush sends everything buffered and waits for the durable acknowledgement.
//
// It returns a nil receipt when there was nothing to send.
func (d *Driver) Flush() (*Receipt, error) {
	batch, err := d.seal()
	if err != nil || batch == nil {
		return nil, err
	}
	response, err := d.client.Call(
		"TallyOwlCollector", "submit-batch", batch.encoded, &d.settings.Credential)
	if err != nil {
		return nil, err
	}
	return readReceipt(batch.batchID, &response)
}

// seal takes everything buffered and encodes it, or returns nil when the buffer
// is empty.
func (d *Driver) seal() (*pendingBatch, error) {
	d.mu.Lock()
	if len(d.items) == 0 {
		d.mu.Unlock()
		return nil, nil
	}
	items := d.items
	d.items = nil
	d.bytes = 0
	d.openedAt = time.Time{}
	d.sealNow = false
	d.mu.Unlock()

	batchID := NewBatchID()
	protocolVersion := uint64(ProtocolVersion)
	request := api.SubmitBatchRequest{
		Batch: api.Batch{
			BatchId:  batchID,
			Items:    items,
			SealedAt: api.Timestamp(nowMs()),
		},
		// What this driver speaks. A collector and the head each accept the
		// current version and the one before it, so an application that
		// upgrades after the installation keeps being accepted.
		ProtocolVersion: &protocolVersion,
	}

	encoded := api.EncodeSubmitBatchRequest(request)
	if len(encoded) > d.settings.MaxFrameBytes {
		return nil, fmt.Errorf(
			"batch rejected. It holds %d KiB and the limit is %d KiB. Seal a smaller batch, or raise the frame limit for this driver",
			len(encoded)/1024, d.settings.MaxFrameBytes/1024)
	}
	return &pendingBatch{batchID: batchID, encoded: encoded, items: len(items)}, nil
}

// readReceipt reads one collector reply into a receipt, or into the typed error
// it carried.
func readReceipt(batchID []byte, response *transport.RpcResponse) (*Receipt, error) {
	if response.Variant != nil && *response.Variant == "ServiceError" {
		wire, decodeErr := api.DecodeServiceError(response.Payload)
		if decodeErr != nil {
			return nil, fmt.Errorf("the collector returned an error we could not read: %w", decodeErr)
		}
		return nil, &ServiceError{
			Code:      string(wire.Code),
			Message:   wire.Message,
			Retryable: wire.Retryable,
		}
	}

	receipt, err := api.DecodeSubmitBatchResponse(response.Payload)
	if err != nil {
		return nil, fmt.Errorf("the collector returned a receipt we could not read: %w", err)
	}

	out := &Receipt{
		BatchID:       batchID,
		Accepted:      receipt.Accepted,
		DurableCopies: receipt.DurableCopies,
	}
	for _, r := range receipt.Rejected {
		out.Rejected = append(out.Rejected, RejectedItem{
			EventID: r.EventId,
			Code:    string(r.Code),
			Message: r.Message,
		})
	}
	return out, nil
}

// Submit seals the buffer and sends it without waiting for its acknowledgement.
//
// This is the pipelined path, and it is the one an application with a single
// telemetry worker wants. Flush waits for the durable receipt of the batch it
// sealed, so one worker is bounded by the round trip rather than by anything in
// TallyOwl: at the D19 defaults that is about 5,400 events each second against
// a measured ceiling of 36,525. See docs/ALPHA_REPORT.md section 3.3.
//
// The returned receipts are for batches that finished while this one was making
// room in the window. It is normal for the list to be empty, and it is normal
// for a receipt to arrive for a batch sealed several calls ago. Nothing here
// reports a batch acknowledged before the collector did.
func (d *Driver) Submit() ([]*Receipt, error) {
	batch, err := d.seal()
	if err != nil || batch == nil {
		return nil, err
	}

	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	if d.pipeline == nil {
		d.pipeline = NewPipeline(
			d.settings.CollectorAddress,
			d.settings.MaxFrameBytes,
			d.settings.MaxInFlightBatches,
		).WithCredential(d.settings.Credential)
		d.pending = map[uint64]*pendingBatch{}
	}

	var receipts []*Receipt
	// Make room in the window before adding to it. Collecting a receipt is what
	// frees a slot, so a caller that never collects never sends.
	for !d.pipeline.HasRoom() {
		receipt, err := d.collectOneLocked()
		if err != nil {
			return receipts, err
		}
		receipts = append(receipts, receipt)
	}
	if err := d.sendPendingLocked(batch); err != nil {
		return receipts, err
	}
	return receipts, nil
}

// Drain waits for every outstanding batch and returns its receipt.
func (d *Driver) Drain() ([]*Receipt, error) {
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	if d.pipeline == nil {
		return nil, nil
	}
	var receipts []*Receipt
	for len(d.pending) > 0 {
		receipt, err := d.collectOneLocked()
		if err != nil {
			return receipts, err
		}
		receipts = append(receipts, receipt)
	}
	return receipts, nil
}

// Outstanding reports how many sealed batches are waiting for an
// acknowledgement.
func (d *Driver) Outstanding() int {
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	return len(d.pending)
}

// OutstandingItems reports how many items sit in batches that are waiting for
// an acknowledgement.
func (d *Driver) OutstandingItems() int {
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	total := 0
	for _, batch := range d.pending {
		total += batch.items
	}
	return total
}

// sendPendingLocked sends one batch, and sends every retained batch again if
// the connection failed under it.
func (d *Driver) sendPendingLocked(batch *pendingBatch) error {
	queue := []*pendingBatch{batch}
	for len(queue) > 0 {
		next := queue[len(queue)-1]
		queue = queue[:len(queue)-1]
		next.attempts++

		id, err := d.pipeline.Send("TallyOwlCollector", "submit-batch", next.encoded)
		if err == nil {
			d.pending[id] = next
			continue
		}

		// The connection took everything outstanding with it. Each retained
		// batch keeps its ID, so sending it again stays one logical commit.
		lost := 0
		for key, held := range d.pending {
			delete(d.pending, key)
			if held.attempts >= d.settings.MaxBatchAttempts {
				lost++
			} else {
				queue = append(queue, held)
			}
		}
		if next.attempts >= d.settings.MaxBatchAttempts {
			lost++
		} else {
			queue = append(queue, next)
		}
		if lost > 0 {
			d.pipeline.Reset()
			d.pending = map[uint64]*pendingBatch{}
			return fmt.Errorf(
				"%d batches were not recorded after %d attempts each. %w",
				lost, d.settings.MaxBatchAttempts, err)
		}
	}
	return nil
}

// collectOneLocked waits for the next acknowledgement and matches it to the
// batch it answers.
func (d *Driver) collectOneLocked() (*Receipt, error) {
	for {
		id, response, err := d.pipeline.Recv()
		if err == nil && response == nil {
			return nil, errors.New("there was no batch waiting for an acknowledgement")
		}
		if err == nil {
			batch, ok := d.pending[id]
			if !ok {
				// A reply for a call this driver never made. The connection is
				// no longer trustworthy.
				d.pipeline.Reset()
				d.pending = map[uint64]*pendingBatch{}
				return nil, errors.New(
					"the collector answered a batch this application did not send")
			}
			delete(d.pending, id)
			return readReceipt(batch.batchID, response)
		}

		// Everything outstanding goes again on a fresh connection.
		held := make([]*pendingBatch, 0, len(d.pending))
		for key, batch := range d.pending {
			delete(d.pending, key)
			held = append(held, batch)
		}
		if len(held) == 0 {
			return nil, err
		}
		for _, batch := range held {
			if batch.attempts >= d.settings.MaxBatchAttempts {
				return nil, fmt.Errorf(
					"a batch was not recorded after %d attempts. %w",
					d.settings.MaxBatchAttempts, err)
			}
			if resendErr := d.sendPendingLocked(batch); resendErr != nil {
				return nil, resendErr
			}
		}
	}
}

// FlushIfSealed flushes when a seal condition is reached, and does nothing
// otherwise.
func (d *Driver) FlushIfSealed() (*Receipt, error) {
	if d.ShouldFlush() {
		return d.Flush()
	}
	return nil, nil
}

// Shutdown stops accepting, drains until the deadline, and reports what did not
// go. The count of unsent items is the return value, because a shutdown that
// reports nothing is a shutdown that hides data loss.
func (d *Driver) Shutdown() int {
	deadline := time.Now().Add(d.settings.ShutdownFlushDeadline)
	// A batch that was already sent is not in the buffer, so a shutdown that
	// ignored the pipeline would report zero unsent items while several batches
	// were still waiting for their acknowledgement.
	unacknowledged := 0
	if d.Outstanding() > 0 {
		if _, err := d.Drain(); err != nil {
			unacknowledged = d.OutstandingItems()
		}
	}
	for time.Now().Before(deadline) {
		receipt, err := d.Flush()
		if err != nil {
			time.Sleep(50 * time.Millisecond)
			continue
		}
		if receipt == nil || d.Buffered() == 0 {
			d.closeAll()
			return unacknowledged
		}
	}
	remaining := d.Buffered()
	d.closeAll()
	return remaining + unacknowledged
}

func (d *Driver) closeAll() {
	d.client.Close()
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	if d.pipeline != nil {
		d.pipeline.Reset()
	}
}
