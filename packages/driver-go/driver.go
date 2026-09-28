package tallyowl

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/rand/v2"
	"sort"
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
//	One call to the collector              10 s
//	Wait between failed attempts           100 ms, doubling to 30 s, with jitter
type Settings struct {
	CollectorAddress string
	Credential       string

	// Transport is how the driver reaches the collector. The zero value uses
	// TLS with the system's trusted authorities for a network address, and
	// plaintext for a loopback or `unix:` address. See Transport.
	Transport Transport

	MaxItems               int
	MaxBatchBytes          int
	Linger                 time.Duration
	MaxFrameBytes          int
	MaxUnacknowledgedBytes int
	ShutdownFlushDeadline  time.Duration

	// CallTimeout bounds one call to the collector: the connect, the send, and
	// the wait for the durable acknowledgement. A collector that accepts a
	// connection and never answers costs the caller this long and no longer.
	// Zero removes the bound.
	CallTimeout time.Duration
	// RetryBackoffMin and RetryBackoffMax bound the wait between failed
	// attempts. The wait doubles from the minimum to the maximum and carries
	// jitter, so that every instance of an application does not return to a
	// restarted collector in the same instant. D19: "Retries use capped jitter
	// while the process is alive."
	RetryBackoffMin time.Duration
	RetryBackoffMax time.Duration

	// MaxInFlightBatches is how many sealed batches may be outstanding at one
	// time on the pipelined path. docs/DELIVERY.md section 3 permits "a
	// configured number of correlated batch calls"; this is that number.
	//
	// One reproduces the synchronous behaviour. The default is four, which is
	// what Submit needs to stop being bounded by the round trip.
	MaxInFlightBatches int
	// MaxBatchAttempts is how many times a batch may be sent and then have its
	// fate become unknown, because the connection broke or the reply could not
	// be read, before the driver reports it as lost. It is what stops one batch
	// that a collector cannot survive from blocking every batch behind it.
	//
	// An unreachable collector does not use an attempt, because the batch never
	// left. A rejection the collector marks retryable does not use one either.
	// Those are bounded by MaxUnacknowledgedBytes and not by a count.
	MaxBatchAttempts int

	// Properties this driver adds to every item. Their origin is `driver`.
	Properties map[string]string

	// OnError hears every delivery failure, every batch the driver gave up on,
	// and every item the collector rejected. The driver calls it with no lock
	// held, so it may call the driver. Nil means nobody listens, and Stats
	// still counts.
	OnError func(error)
	// DryRun, when set, receives each sealed item as one line of JSON, and the
	// driver opens no connection. It is how a developer confirms what an
	// application records before a collector exists. A dry-run receipt reports
	// zero durable copies, because nothing was made durable.
	DryRun io.Writer
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
		CallTimeout:            10 * time.Second,
		RetryBackoffMin:        100 * time.Millisecond,
		RetryBackoffMax:        30 * time.Second,
		MaxInFlightBatches:     DefaultClientWindow,
		MaxBatchAttempts:       5,
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
//
// A flush that sent more than one batch reports them together: the accepted
// counts add, the rejected items join, the durable copies are the fewest any
// batch reported, and the batch ID is the last one sent.
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

// RetryLaterError reports that the last attempt failed and the driver is
// waiting before the next one. Nothing was sent by the call that returned it,
// and nothing was lost: the data is still held. Wait is how long is left.
type RetryLaterError struct {
	Wait time.Duration
	Err  error
}

func (e *RetryLaterError) Error() string {
	return fmt.Sprintf("the last attempt failed, and the next one is in %s. %v",
		e.Wait.Round(time.Millisecond), e.Err)
}

func (e *RetryLaterError) Unwrap() error { return e.Err }

// Stats is what the driver has done since it started. A developer reads it to
// answer "is my telemetry going anywhere" without a collector log.
type Stats struct {
	// Captured is how many items Capture took. Refused is how many it turned
	// away because the unacknowledged bound was reached.
	Captured uint64
	Refused  uint64
	// Accepted and Rejected are what the collector's receipts reported.
	Accepted uint64
	Rejected uint64
	// Lost is how many items the driver gave up on: a batch the collector
	// refused for good, or one whose fate stayed unknown past MaxBatchAttempts.
	Lost uint64
	// Buffered items are not sealed yet. Unacknowledged items are sealed and
	// held until the collector acknowledges them.
	Buffered       int
	Unacknowledged int
	// LastError is the most recent delivery failure, and nil after a success.
	LastError error
}

// batchEnvelopeBytes is room for what travels around the items in one frame:
// the batch ID, the seal time, the protocol version, and the call that carries
// them, which names the service and the operation. The credential rides in the
// same frame and is counted apart, because its length is the host's.
const batchEnvelopeBytes = 256

// frameRoom is how many bytes of one frame are left for an encoded batch.
func (d *Driver) frameRoom() int {
	return d.settings.MaxFrameBytes - batchEnvelopeBytes - len(d.settings.Credential)
}

// Driver is the Go app driver.
type Driver struct {
	settings Settings
	client   *Client
	sequence atomic.Uint64

	mu       sync.Mutex
	items    []api.TelemetryItem
	sizes    []int
	bytes    int
	openedAt time.Time
	sealNow  bool

	// Everything below outboxMu is the delivery state: what is sealed, where it
	// is, and when the next attempt may run. Capture never takes this lock, so a
	// slow collector never slows the application's own path.
	//
	// The pipelined path opens on the first Submit and stays closed for an
	// application that only ever calls Flush, so an application keeps one
	// connection either way.
	outboxMu sync.Mutex
	pipeline *Pipeline
	pending  map[uint64]*pendingBatch
	// retained holds each sealed batch that is not in flight and is not
	// acknowledged, oldest first. A failed send puts its batch here, so a
	// failure is a later resend and never a loss.
	retained    []*pendingBatch
	failures    int
	nextAttempt time.Time
	lastErr     error
	callTimeout time.Duration
	notes       []error

	// sealedBytes is the size of every sealed batch that is not acknowledged. It
	// counts against MaxUnacknowledgedBytes with the buffer, which is what keeps
	// an application's memory bounded while a collector is away.
	sealedBytes atomic.Int64

	captured atomic.Uint64
	refused  atomic.Uint64
	accepted atomic.Uint64
	rejected atomic.Uint64
	lost     atomic.Uint64

	// The clock, the wait, and the jitter are fields so that a test moves time
	// rather than waiting for it.
	now    func() time.Time
	sleep  func(time.Duration)
	jitter func(ceiling time.Duration) time.Duration
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
	items int
	bytes int
	// attempts counts the sends whose fate became unknown.
	attempts int
}

// NewDriver builds a driver that reaches one collector.
func NewDriver(settings Settings) *Driver {
	client := NewClientWith(settings.CollectorAddress, settings.MaxFrameBytes, settings.Transport)
	client.SetCallTimeout(settings.CallTimeout)
	return &Driver{
		settings:    settings,
		client:      client,
		callTimeout: settings.CallTimeout,
		now:         time.Now,
		sleep:       time.Sleep,
		jitter: func(ceiling time.Duration) time.Duration {
			if ceiling <= 0 {
				return 0
			}
			// Equal jitter: half the ceiling is certain and half is random, so
			// a wait is never close to zero and two instances rarely agree.
			return ceiling/2 + rand.N(ceiling/2+1)
		},
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
	if size > d.frameRoom() {
		return fmt.Errorf(
			"this event was not recorded. It holds %d KiB and one frame holds %d KiB. Record a smaller event, or raise the frame limit for this driver",
			size/1024, d.settings.MaxFrameBytes/1024)
	}

	d.mu.Lock()
	defer d.mu.Unlock()
	if d.bytes+size+int(d.sealedBytes.Load()) > d.settings.MaxUnacknowledgedBytes {
		d.refused.Add(1)
		return ErrBackpressure
	}
	if d.openedAt.IsZero() {
		d.openedAt = d.now()
	}
	d.bytes += size
	d.items = append(d.items, item)
	d.sizes = append(d.sizes, size)
	if capture.critical {
		d.sealNow = true
	}
	d.captured.Add(1)
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
	return !d.openedAt.IsZero() && d.now().Sub(d.openedAt) >= d.settings.Linger
}

// Stats reports what the driver has done so far.
func (d *Driver) Stats() Stats {
	d.outboxMu.Lock()
	lastErr := d.lastErr
	d.outboxMu.Unlock()
	return Stats{
		Captured:       d.captured.Load(),
		Refused:        d.refused.Load(),
		Accepted:       d.accepted.Load(),
		Rejected:       d.rejected.Load(),
		Lost:           d.lost.Load(),
		Buffered:       d.Buffered(),
		Unacknowledged: d.OutstandingItems(),
		LastError:      lastErr,
	}
}

// NewBatchID makes the identifier a batch keeps across every retry and across
// a lost connection, so final storage deduplicates it to one logical commit.
func NewBatchID() []byte { return NewEventID() }

// Flush sends everything buffered and waits for each durable acknowledgement.
//
// It sends what an earlier failure left behind first, and then the buffer, one
// batch of at most MaxItems and MaxBatchBytes at a time. It returns a nil
// receipt when there was nothing to send.
//
// A failure loses nothing. The batch stays held, the driver waits before the
// next attempt, and a call inside that wait returns a RetryLaterError at once.
func (d *Driver) Flush() (*Receipt, error) {
	d.outboxMu.Lock()
	receipt, err := d.flushLocked()
	notes := d.takeNotesLocked()
	d.outboxMu.Unlock()
	d.report(notes)
	return receipt, err
}

func (d *Driver) flushLocked() (*Receipt, error) {
	var merged *Receipt
	for {
		if len(d.retained) == 0 && d.Buffered() == 0 {
			return merged, nil
		}
		if err := d.waitingLocked(); err != nil {
			return merged, err
		}
		batch, err := d.nextBatchLocked()
		if err != nil || batch == nil {
			return merged, err
		}
		receipt, err := d.callLocked(batch)
		if err != nil {
			return merged, err
		}
		merged = mergeReceipts(merged, receipt)
	}
}

func mergeReceipts(merged, next *Receipt) *Receipt {
	if merged == nil {
		return next
	}
	merged.BatchID = next.BatchID
	merged.Accepted += next.Accepted
	if next.DurableCopies < merged.DurableCopies {
		merged.DurableCopies = next.DurableCopies
	}
	merged.Rejected = append(merged.Rejected, next.Rejected...)
	return merged
}

// nextBatchLocked is the oldest batch an earlier failure left behind, or a new
// one sealed from the buffer, or nil when there is neither.
func (d *Driver) nextBatchLocked() (*pendingBatch, error) {
	if len(d.retained) > 0 {
		batch := d.retained[0]
		d.retained = d.retained[1:]
		return batch, nil
	}
	return d.sealLocked()
}

// sealLocked takes one batch from the front of the buffer and encodes it, or
// returns nil when the buffer is empty.
//
// A batch holds at most MaxItems and MaxBatchBytes, and always at least one
// item. The rest stays buffered for the next batch. Taking the whole buffer is
// how a buffer of 8 MiB met a frame of 1 MiB and lost all of it.
func (d *Driver) sealLocked() (*pendingBatch, error) {
	d.mu.Lock()
	take, bytes := 0, 0
	for take < len(d.items) && take < max(d.settings.MaxItems, 1) {
		if take > 0 && bytes+d.sizes[take] > d.settings.MaxBatchBytes {
			break
		}
		bytes += d.sizes[take]
		take++
	}
	items := append([]api.TelemetryItem(nil), d.items[:take]...)
	d.mu.Unlock()
	if take == 0 {
		return nil, nil
	}

	batchID := NewBatchID()
	encoded := encodeBatch(batchID, items)
	// The sizes are of items, and a frame also carries the batch around them. A
	// driver configured with a batch limit above its frame limit lands here, and
	// the answer is a smaller batch and never a discarded one.
	for len(encoded) > d.frameRoom() && len(items) > 1 {
		items = items[:len(items)/2]
		encoded = encodeBatch(batchID, items)
	}
	take = len(items)

	// Only a seal removes from the front, and every seal holds outboxMu, so the
	// first `take` items are still the ones encoded above.
	d.mu.Lock()
	bytes = 0
	for _, size := range d.sizes[:take] {
		bytes += size
	}
	d.items = d.items[take:]
	d.sizes = d.sizes[take:]
	d.bytes -= bytes
	if len(d.items) == 0 {
		d.items, d.sizes = nil, nil
		d.openedAt = time.Time{}
		// A critical item may still be in what is left, so the flag outlives a
		// seal that did not empty the buffer.
		d.sealNow = false
	} else {
		d.openedAt = d.now()
	}
	d.mu.Unlock()

	if len(encoded) > d.frameRoom() {
		err := fmt.Errorf(
			"one event was not recorded. Its batch holds %d KiB and one frame holds %d KiB. Record a smaller event, or raise the frame limit for this driver",
			len(encoded)/1024, d.settings.MaxFrameBytes/1024)
		d.lost.Add(uint64(take))
		d.lastErr = err
		d.notes = append(d.notes, err)
		return nil, err
	}
	d.sealedBytes.Add(int64(bytes))
	return &pendingBatch{batchID: batchID, encoded: encoded, items: take, bytes: bytes}, nil
}

func encodeBatch(batchID []byte, items []api.TelemetryItem) []byte {
	protocolVersion := uint64(ProtocolVersion)
	return api.EncodeSubmitBatchRequest(api.SubmitBatchRequest{
		Batch: api.Batch{
			BatchId:  batchID,
			Items:    items,
			SealedAt: api.Timestamp(nowMs()),
		},
		// What this driver speaks. A collector and the head each accept the
		// current version and the one before it, so an application that
		// upgrades after the installation keeps being accepted.
		ProtocolVersion: &protocolVersion,
	})
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

// callLocked sends one batch on the synchronous path and settles it.
func (d *Driver) callLocked(batch *pendingBatch) (*Receipt, error) {
	if d.settings.DryRun != nil {
		return d.dryRunLocked(batch)
	}
	response, err := d.client.Call(
		"TallyOwlCollector", "submit-batch", batch.encoded, &d.settings.Credential)
	if err != nil {
		d.retainLocked(batch, err)
		d.backoffLocked(err)
		return nil, err
	}
	return d.settleLocked(batch, &response)
}

// dryRunLocked writes each item as one line of JSON and opens no connection.
func (d *Driver) dryRunLocked(batch *pendingBatch) (*Receipt, error) {
	request, err := api.DecodeSubmitBatchRequest(batch.encoded)
	if err != nil {
		d.loseLocked(batch, fmt.Errorf("a sealed batch could not be read back: %w", err))
		return nil, err
	}
	for _, item := range request.Batch.Items {
		line, err := json.Marshal(item)
		if err != nil {
			continue
		}
		_, _ = d.settings.DryRun.Write(append(line, '\n'))
	}
	receipt := &Receipt{BatchID: batch.batchID, Accepted: uint64(batch.items)}
	d.ackedLocked(batch, receipt)
	return receipt, nil
}

// settleLocked reads the collector's answer for one batch and decides what
// becomes of it: acknowledged, held for another attempt, or given up.
func (d *Driver) settleLocked(batch *pendingBatch, response *transport.RpcResponse) (*Receipt, error) {
	receipt, err := readReceipt(batch.batchID, response)
	if err == nil {
		d.ackedLocked(batch, receipt)
		return receipt, nil
	}
	var service *ServiceError
	if errors.As(err, &service) && !service.Retryable {
		// The collector said it will never take this batch. Sending it again
		// would block every batch behind it for nothing.
		d.loseLocked(batch, fmt.Errorf(
			"%d items were not recorded. The collector refused them and will not take them later. %s%w",
			batch.items, refusalHint(service), err))
		return nil, err
	}
	d.retainLocked(batch, err)
	d.backoffLocked(err)
	return nil, err
}

// refusalHint says what a developer does about the refusals a developer causes.
func refusalHint(service *ServiceError) string {
	switch service.Code {
	case "unauthenticated", "permission-denied":
		return "Check the credential this driver was given. "
	}
	return ""
}

func (d *Driver) ackedLocked(batch *pendingBatch, receipt *Receipt) {
	d.sealedBytes.Add(-int64(batch.bytes))
	d.accepted.Add(receipt.Accepted)
	d.rejected.Add(uint64(len(receipt.Rejected)))
	d.failures = 0
	d.nextAttempt = time.Time{}
	d.lastErr = nil
	if len(receipt.Rejected) > 0 {
		first := receipt.Rejected[0]
		d.notes = append(d.notes, fmt.Errorf(
			"the collector did not record %d items. The first reason: %s (%s)",
			len(receipt.Rejected), first.Message, first.Code))
	}
}

func (d *Driver) loseLocked(batch *pendingBatch, err error) {
	d.sealedBytes.Add(-int64(batch.bytes))
	d.lost.Add(uint64(batch.items))
	d.lastErr = err
	d.notes = append(d.notes, err)
}

// retainLocked puts a batch that was not acknowledged back at the front, so it
// goes again before anything newer. A batch whose fate has been unknown
// MaxBatchAttempts times is given up instead.
func (d *Driver) retainLocked(batch *pendingBatch, err error) {
	if fateUnknown(err) {
		batch.attempts++
		if batch.attempts >= max(d.settings.MaxBatchAttempts, 1) {
			d.loseLocked(batch, fmt.Errorf(
				"%d items were not recorded after %d attempts. %w",
				batch.items, batch.attempts, err))
			return
		}
	}
	d.retained = append([]*pendingBatch{batch}, d.retained...)
}

// fateUnknown reports whether a failure leaves open whether the collector took
// the batch. An unreachable collector took nothing, a failed handshake sent
// nothing, and a typed rejection says exactly what it did.
func fateUnknown(err error) bool {
	var unreachable *UnreachableError
	var handshake *HandshakeError
	var service *ServiceError
	return !errors.As(err, &unreachable) && !errors.As(err, &handshake) &&
		!errors.As(err, &service)
}

// retainPendingLocked moves every batch in flight back to the held list. The
// connection that carried them is gone, and each keeps its ID, so sending it
// again stays one logical commit.
func (d *Driver) retainPendingLocked(err error) {
	ids := make([]uint64, 0, len(d.pending))
	for id := range d.pending {
		ids = append(ids, id)
	}
	// Newest first, because each one goes to the front: the oldest ends there.
	sort.Slice(ids, func(a, b int) bool { return ids[a] > ids[b] })
	for _, id := range ids {
		batch := d.pending[id]
		delete(d.pending, id)
		d.retainLocked(batch, err)
	}
}

// backoffLocked sets when the next attempt may run: the minimum wait doubled
// for each failure in a row, capped, with jitter.
func (d *Driver) backoffLocked(err error) {
	d.failures++
	floor := d.settings.RetryBackoffMin
	if floor <= 0 {
		floor = 100 * time.Millisecond
	}
	ceiling := d.settings.RetryBackoffMax
	if ceiling < floor {
		ceiling = floor
	}
	wait := floor
	for step := 1; step < d.failures && wait < ceiling; step++ {
		wait *= 2
	}
	if wait > ceiling {
		wait = ceiling
	}
	d.nextAttempt = d.now().Add(d.jitter(wait))
	d.lastErr = err
	d.notes = append(d.notes, err)
}

// waitingLocked is a RetryLaterError while the wait after a failure is open.
func (d *Driver) waitingLocked() error {
	if wait := d.nextAttempt.Sub(d.now()); wait > 0 {
		return &RetryLaterError{Wait: wait, Err: d.lastErr}
	}
	return nil
}

func (d *Driver) takeNotesLocked() []error {
	notes := d.notes
	d.notes = nil
	return notes
}

// report tells the host what went wrong. It runs with no lock held, and a host
// callback that panics does not take the driver's caller down with it.
func (d *Driver) report(notes []error) {
	if d.settings.OnError == nil {
		return
	}
	for _, note := range notes {
		func() {
			defer func() { _ = recover() }()
			d.settings.OnError(note)
		}()
	}
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
//
// A failure loses nothing, as with Flush: every batch in flight is held and goes
// again after the wait.
func (d *Driver) Submit() ([]*Receipt, error) {
	d.outboxMu.Lock()
	receipts, err := d.submitLocked()
	notes := d.takeNotesLocked()
	d.outboxMu.Unlock()
	d.report(notes)
	return receipts, err
}

func (d *Driver) submitLocked() ([]*Receipt, error) {
	if d.settings.DryRun != nil {
		receipt, err := d.flushLocked()
		if receipt == nil {
			return nil, err
		}
		return []*Receipt{receipt}, err
	}

	var receipts []*Receipt
	for {
		if len(d.retained) == 0 && d.Buffered() == 0 {
			return receipts, nil
		}
		if err := d.waitingLocked(); err != nil {
			return receipts, err
		}
		if d.pipeline == nil {
			d.pipeline = NewPipeline(
				d.settings.CollectorAddress,
				d.settings.MaxFrameBytes,
				d.settings.MaxInFlightBatches,
			).WithCredential(d.settings.Credential).
				WithCallTimeout(d.callTimeout).
				WithTransport(d.settings.Transport)
			d.pending = map[uint64]*pendingBatch{}
		}
		// Make room in the window before adding to it. Collecting a receipt is
		// what frees a slot, so a caller that never collects never sends.
		for !d.pipeline.HasRoom() {
			receipt, err := d.collectOneLocked()
			if err != nil {
				return receipts, err
			}
			receipts = append(receipts, receipt)
		}
		batch, err := d.nextBatchLocked()
		if err != nil || batch == nil {
			return receipts, err
		}
		if err := d.sendLocked(batch); err != nil {
			return receipts, err
		}
	}
}

// Drain waits for every batch in flight and returns its receipt.
func (d *Driver) Drain() ([]*Receipt, error) {
	d.outboxMu.Lock()
	receipts, err := d.drainLocked()
	notes := d.takeNotesLocked()
	d.outboxMu.Unlock()
	d.report(notes)
	return receipts, err
}

func (d *Driver) drainLocked() ([]*Receipt, error) {
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
// acknowledgement, in flight or held for another attempt.
func (d *Driver) Outstanding() int {
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	return len(d.pending) + len(d.retained)
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
	for _, batch := range d.retained {
		total += batch.items
	}
	return total
}

// sendLocked sends one batch on the pipelined path. A failure holds this batch
// and every batch that was in flight on the same connection, and starts the
// wait. It does not send again here: a tight resend loop is how a collector
// that restarts meets every instance of an application in the same millisecond.
func (d *Driver) sendLocked(batch *pendingBatch) error {
	id, err := d.pipeline.Send("TallyOwlCollector", "submit-batch", batch.encoded)
	if err == nil {
		d.pending[id] = batch
		return nil
	}
	d.retainLocked(batch, err)
	d.retainPendingLocked(err)
	d.backoffLocked(err)
	return err
}

// collectOneLocked waits for the next acknowledgement and matches it to the
// batch it answers.
func (d *Driver) collectOneLocked() (*Receipt, error) {
	id, response, err := d.pipeline.Recv()
	if err == nil && response == nil {
		return nil, errors.New("there was no batch waiting for an acknowledgement")
	}
	if err != nil {
		// The connection took everything in flight with it.
		d.retainPendingLocked(err)
		d.backoffLocked(err)
		return nil, err
	}
	batch, ok := d.pending[id]
	if !ok {
		// A reply for a call this driver never made. The connection is no
		// longer trustworthy.
		err := errors.New("the collector answered a batch this application did not send")
		d.pipeline.Reset()
		d.retainPendingLocked(err)
		d.backoffLocked(err)
		return nil, err
	}
	delete(d.pending, id)
	return d.settleLocked(batch, response)
}

// FlushIfSealed flushes when a seal condition is reached, and does nothing
// otherwise.
func (d *Driver) FlushIfSealed() (*Receipt, error) {
	if d.ShouldFlush() {
		return d.Flush()
	}
	return nil, nil
}

// Run sends what the application captures until the context ends, then shuts
// the driver down and returns what Shutdown returns.
//
// It is optional. Capture only buffers, so an application that never calls
// Flush, Submit, or Run sends nothing until it exits. A long-running service
// starts this once:
//
//	go driver.Run(ctx)
//
// Run owns no connection of its own and no state: it calls Submit when a seal
// condition is reached or a held batch is due, and Drain when only
// acknowledgements are outstanding. Failures reach Settings.OnError. A host
// that already has a scheduler calls ShouldFlush and Submit from it instead.
func (d *Driver) Run(ctx context.Context) int {
	interval := d.settings.Linger / 2
	interval = min(max(interval, 10*time.Millisecond), time.Second)
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return d.Shutdown()
		case <-ticker.C:
			d.tick()
		}
	}
}

// tick is one pass of Run.
func (d *Driver) tick() {
	switch {
	case d.ShouldFlush() || d.heldBatchDue():
		_, _ = d.Submit()
	case d.Outstanding() > 0:
		_, _ = d.Drain()
	}
}

func (d *Driver) heldBatchDue() bool {
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	return len(d.retained) > 0 && d.waitingLocked() == nil
}

// Shutdown drains until the deadline and reports what did not go. The count of
// unsent items is the return value, because a shutdown that reports nothing is
// a shutdown that hides data loss.
//
// The count holds what is still buffered, what is sealed and not acknowledged,
// and what the driver gave up on during the shutdown. It returns by the
// deadline whatever the collector does: every call it makes is bounded by the
// time that is left.
func (d *Driver) Shutdown() int {
	deadline := d.now().Add(d.settings.ShutdownFlushDeadline)
	lostBefore := d.lost.Load()
	for {
		remaining := deadline.Sub(d.now())
		if remaining <= 0 {
			break
		}
		d.boundCalls(remaining)
		// A batch that was already sent is not in the buffer, so a shutdown that
		// ignored the pipeline would report zero unsent items while several
		// batches were still waiting for their acknowledgement.
		_, drainErr := d.Drain()
		_, err := d.Flush()
		if drainErr == nil && err == nil && d.Buffered() == 0 && d.Outstanding() == 0 {
			break
		}
		wait := 10 * time.Millisecond
		var later *RetryLaterError
		if errors.As(err, &later) {
			wait = later.Wait
		}
		if wait >= deadline.Sub(d.now()) {
			// The next attempt is past the deadline, so waiting for it would
			// only spend the host's exit on a send that will not happen.
			break
		}
		d.sleep(wait)
	}
	left := d.Buffered() + d.OutstandingItems() + int(d.lost.Load()-lostBefore)
	d.closeAll()
	return left
}

// boundCalls lowers the call timeout to the time a shutdown has left.
func (d *Driver) boundCalls(remaining time.Duration) {
	timeout := remaining
	if d.settings.CallTimeout > 0 && d.settings.CallTimeout < timeout {
		timeout = d.settings.CallTimeout
	}
	d.client.SetCallTimeout(timeout)
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	d.callTimeout = timeout
	if d.pipeline != nil {
		d.pipeline.SetCallTimeout(timeout)
	}
}

func (d *Driver) closeAll() {
	d.client.Close()
	d.outboxMu.Lock()
	defer d.outboxMu.Unlock()
	if d.pipeline != nil {
		d.pipeline.Reset()
	}
}
