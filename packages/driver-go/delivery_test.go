package tallyowl

// What happens to a batch when the collector does not take it.
//
// D5 says the app retains the stable batch until the acknowledgement. Every
// test here is a way that used to lose the batch instead, and then report that
// nothing was lost. Time is a field of the driver, so these tests move it and
// never wait for it.

import (
	"bytes"
	"context"
	"errors"
	"net"
	"strings"
	"sync"
	"testing"
	"time"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// fakeClock is the driver's time. Sleeping moves it.
type fakeClock struct {
	mu   sync.Mutex
	at   time.Time
	naps []time.Duration
}

func (c *fakeClock) now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.at
}

func (c *fakeClock) sleep(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.naps = append(c.naps, d)
	c.at = c.at.Add(d)
}

func (c *fakeClock) advance(d time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.at = c.at.Add(d)
}

// onFakeTime gives a driver a clock the test moves, and a wait with no jitter,
// so a test can say exactly how long the driver waits.
func onFakeTime(driver *Driver) *fakeClock {
	clock := &fakeClock{at: time.Unix(1_700_000_000, 0)}
	driver.now = clock.now
	driver.sleep = clock.sleep
	driver.jitter = func(ceiling time.Duration) time.Duration { return ceiling }
	return clock
}

func captureEvents(t *testing.T, driver *Driver, count int) {
	t.Helper()
	for i := 0; i < count; i++ {
		if err := driver.Capture(Event("a")); err != nil {
			t.Fatalf("capture %d: %v", i, err)
		}
	}
}

func TestAShutdownWithNoCollectorReportsEveryItemItHeld(t *testing.T) {
	// This returned zero: the first flush failed and dropped the batch, and the
	// second found an empty buffer and called that success.
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	onFakeTime(driver)
	captureEvents(t, driver, 7)

	if left := driver.Shutdown(); left != 7 {
		t.Fatalf("seven items did not go, and the shutdown reported %d", left)
	}
	if lost := driver.Stats().Lost; lost != 0 {
		t.Errorf("an unreachable collector is not a lost batch, got %d lost", lost)
	}
}

func TestAShutdownNeverWaitsPastItsDeadline(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.ShutdownFlushDeadline = 2 * time.Second
	driver := NewDriver(settings)
	clock := onFakeTime(driver)
	started := clock.now()
	captureEvents(t, driver, 1)

	driver.Shutdown()
	if spent := clock.now().Sub(started); spent > settings.ShutdownFlushDeadline {
		t.Fatalf("the shutdown spent %s of a %s deadline", spent, settings.ShutdownFlushDeadline)
	}
}

func TestAFailedFlushKeepsTheBatchAndItArrivesOnceTheCollectorReturns(t *testing.T) {
	fake := startFakeCollector(t)
	address := fake.address()
	fake.stop()

	driver := NewDriver(NewSettings(address, "key-a"))
	clock := onFakeTime(driver)
	captureEvents(t, driver, 3)

	if _, err := driver.Flush(); err == nil {
		t.Fatal("a stopped collector reported success")
	}
	if held := driver.OutstandingItems(); held != 3 {
		t.Fatalf("the failed batch should still be held, got %d items", held)
	}

	// Inside the wait, a flush says so at once and does not reach for the
	// collector: this is what stops a retry storm.
	_, err := driver.Flush()
	var later *RetryLaterError
	if !errors.As(err, &later) {
		t.Fatalf("a flush inside the wait returned %T: %v", err, err)
	}
	if later.Wait != 100*time.Millisecond {
		t.Errorf("the first wait should be the minimum, got %s", later.Wait)
	}

	listener, listenErr := net.Listen("tcp", address)
	if listenErr != nil {
		t.Skipf("the address did not come free again: %v", listenErr)
	}
	replacement := &fakeCollector{listener: listener}
	go replacement.accept()
	defer listener.Close()

	clock.advance(later.Wait)
	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("the held batch did not go: %v", err)
	}
	if receipt.Accepted != 3 {
		t.Errorf("the collector accepted %d of 3", receipt.Accepted)
	}
	if got := replacement.received(); len(got) != 1 || len(got[0].Items) != 3 {
		t.Fatalf("the collector should have seen one batch of three, got %d batches", len(got))
	}
	stats := driver.Stats()
	if stats.Lost != 0 || stats.Unacknowledged != 0 || stats.LastError != nil {
		t.Errorf("a delivered batch left %+v", stats)
	}
}

func TestABufferLargerThanOneBatchGoesAsSeveralAndLosesNothing(t *testing.T) {
	// A seal used to take the whole buffer. Past one frame, all of it was
	// refused with advice the caller had no way to follow.
	fake := startFakeCollector(t)
	settings := NewSettings(fake.address(), "key-a")
	settings.MaxItems = 10
	driver := NewDriver(settings)
	captureEvents(t, driver, 35)

	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("flush: %v", err)
	}
	if receipt.Accepted != 35 {
		t.Errorf("the receipt reports %d of 35", receipt.Accepted)
	}
	sizes := []int{}
	for _, batch := range fake.received() {
		sizes = append(sizes, len(batch.Items))
	}
	if want := []int{10, 10, 10, 5}; !equalInts(sizes, want) {
		t.Fatalf("the batches held %v, want %v", sizes, want)
	}
}

func TestABatchLimitAboveTheFrameLimitStillSendsEverything(t *testing.T) {
	fake := startFakeCollector(t)
	settings := NewSettings(fake.address(), "key-a")
	settings.MaxItems = 10_000
	settings.MaxBatchBytes = 64 * 1024 * 1024
	settings.MaxFrameBytes = 8 * 1024
	driver := NewDriver(settings)
	captureEvents(t, driver, 400)

	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("flush: %v", err)
	}
	if receipt.Accepted != 400 {
		t.Fatalf("the receipt reports %d of 400", receipt.Accepted)
	}
	if driver.Stats().Lost != 0 {
		t.Error("a batch that did not fit a frame was discarded rather than split")
	}
}

func TestAnEventThatCannotFitAFrameIsRefusedAtCapture(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxFrameBytes = 512
	driver := NewDriver(settings)

	err := driver.Capture(Event(strings.Repeat("n", 1024)))
	if err == nil {
		t.Fatal("an event larger than a frame was buffered")
	}
	if !contains(err.Error(), "frame") {
		t.Errorf("the refusal does not say why: %v", err)
	}
}

func TestAPermanentRefusalIsCountedLostReportedAndNotSentAgain(t *testing.T) {
	fake := startFakeCollector(t)
	fake.failWith = &collector.ServiceError{
		Code:      "unauthenticated",
		Message:   "This credential is not valid.",
		Retryable: false,
	}
	var heard []error
	settings := NewSettings(fake.address(), "wrong-key")
	settings.OnError = func(err error) { heard = append(heard, err) }
	driver := NewDriver(settings)
	captureEvents(t, driver, 2)

	if _, err := driver.Flush(); err == nil {
		t.Fatal("a refused batch reported success")
	}
	stats := driver.Stats()
	if stats.Lost != 2 || stats.Unacknowledged != 0 {
		t.Fatalf("a permanent refusal left %+v", stats)
	}
	if len(heard) != 1 || !contains(heard[0].Error(), "credential") {
		t.Fatalf("the host should hear once, and be told to check the credential: %v", heard)
	}
	if _, err := driver.Flush(); err != nil {
		t.Errorf("a batch the collector refused for good went again: %v", err)
	}
	if len(fake.received()) != 1 {
		t.Errorf("the collector saw the refused batch %d times", len(fake.received()))
	}
	if left := driver.Shutdown(); left != 0 {
		t.Errorf("items lost before the shutdown are not part of what it left, got %d", left)
	}
}

func TestARetryableRefusalKeepsTheBatchForTheNextAttempt(t *testing.T) {
	fake := startFakeCollector(t)
	fake.failWith = &collector.ServiceError{
		Code:      "unavailable",
		Message:   "The durable queue is away.",
		Retryable: true,
	}
	driver := NewDriver(NewSettings(fake.address(), "key-a"))
	clock := onFakeTime(driver)
	captureEvents(t, driver, 2)

	if _, err := driver.Flush(); err == nil {
		t.Fatal("a refused batch reported success")
	}
	if stats := driver.Stats(); stats.Lost != 0 || stats.Unacknowledged != 2 {
		t.Fatalf("a retryable refusal left %+v", stats)
	}

	fake.mu.Lock()
	fake.failWith = nil
	fake.mu.Unlock()
	clock.advance(time.Second)
	receipt, err := driver.Flush()
	if err != nil || receipt.Accepted != 2 {
		t.Fatalf("the held batch did not go: %v, %+v", err, receipt)
	}
	batches := fake.received()
	if len(batches) != 2 || !bytes.Equal(batches[0].BatchId, batches[1].BatchId) {
		t.Fatal("the second attempt should carry the same batch ID, so storage keeps one copy")
	}
}

func TestTheWaitDoublesFromItsMinimumToItsMaximum(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	clock := onFakeTime(driver)
	failure := errors.New("away")

	var waits []time.Duration
	for i := 0; i < 11; i++ {
		driver.backoffLocked(failure)
		waits = append(waits, driver.nextAttempt.Sub(clock.now()))
	}
	want := []time.Duration{
		100 * time.Millisecond, 200 * time.Millisecond, 400 * time.Millisecond,
		800 * time.Millisecond, 1600 * time.Millisecond, 3200 * time.Millisecond,
		6400 * time.Millisecond, 12800 * time.Millisecond, 25600 * time.Millisecond,
		30 * time.Second, 30 * time.Second,
	}
	for i := range want {
		if waits[i] != want[i] {
			t.Fatalf("wait %d was %s, want %s", i, waits[i], want[i])
		}
	}
}

func TestTheJitterKeepsAWaitBetweenHalfAndAllOfItsCeiling(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	for i := 0; i < 1000; i++ {
		wait := driver.jitter(time.Second)
		if wait < 500*time.Millisecond || wait > time.Second {
			t.Fatalf("a jittered wait of %s is outside half to all of one second", wait)
		}
	}
}

func TestSealedBatchesCountAgainstTheUnacknowledgedBound(t *testing.T) {
	// Without this, a collector that is away lets the held batches grow without
	// limit in the application's memory.
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxUnacknowledgedBytes = 600
	settings.MaxItems = 2
	driver := NewDriver(settings)
	onFakeTime(driver)

	captured := 0
	for i := 0; i < 100; i++ {
		if err := driver.Capture(Event("a")); err != nil {
			if !errors.Is(err, ErrBackpressure) {
				t.Fatalf("capture: %v", err)
			}
			break
		}
		captured++
		_, _ = driver.Flush()
	}
	if captured == 100 {
		t.Fatal("held batches never pushed back on capture")
	}
	stats := driver.Stats()
	if stats.Refused != 1 || stats.Buffered+stats.Unacknowledged != captured {
		t.Fatalf("captured %d, and the driver reports %+v", captured, stats)
	}
}

// silentCollector accepts a connection, reads what arrives, and never answers.
func silentCollector(t *testing.T, closeAfterRead bool) string {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func() {
				defer conn.Close()
				carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, 16*1024*1024)
				if err != nil {
					return
				}
				for {
					frame, err := carrier.RecvFrame()
					if err != nil || frame == nil || closeAfterRead {
						return
					}
				}
			}()
		}
	}()
	return listener.Addr().String()
}

func TestACollectorThatNeverAnswersCostsOneCallTimeoutAndLosesNothing(t *testing.T) {
	// The socket deadline is the thing under test, so this one is real time,
	// and short.
	settings := NewSettings(silentCollector(t, false), "key-a")
	settings.CallTimeout = 40 * time.Millisecond
	settings.ShutdownFlushDeadline = 60 * time.Millisecond
	driver := NewDriver(settings)
	captureEvents(t, driver, 4)

	started := time.Now()
	if _, err := driver.Flush(); err == nil {
		t.Fatal("a collector that never answered reported success")
	}
	// Two attempts, because a call that fails on its connection runs once more
	// on a fresh one.
	if spent := time.Since(started); spent > time.Second {
		t.Fatalf("the flush took %s against a %s call timeout", spent, settings.CallTimeout)
	}
	if held := driver.OutstandingItems(); held != 4 {
		t.Fatalf("the unanswered batch should still be held, got %d", held)
	}

	started = time.Now()
	if left := driver.Shutdown(); left != 4 {
		t.Errorf("the shutdown reported %d of 4 unsent", left)
	}
	if spent := time.Since(started); spent > time.Second {
		t.Fatalf("the shutdown took %s against a %s deadline", spent, settings.ShutdownFlushDeadline)
	}
}

func TestABatchWhoseFateStaysUnknownIsGivenUpAtTheAttemptLimit(t *testing.T) {
	// A batch the collector cannot survive must not block every batch behind it
	// for the life of the process.
	var heard []error
	settings := NewSettings(silentCollector(t, true), "key-a")
	settings.MaxBatchAttempts = 2
	settings.OnError = func(err error) { heard = append(heard, err) }
	driver := NewDriver(settings)
	clock := onFakeTime(driver)
	captureEvents(t, driver, 3)

	_, _ = driver.Flush()
	if stats := driver.Stats(); stats.Lost != 0 || stats.Unacknowledged != 3 {
		t.Fatalf("one unknown fate is not a loss, got %+v", stats)
	}
	clock.advance(time.Minute)
	_, _ = driver.Flush()
	stats := driver.Stats()
	if stats.Lost != 3 || stats.Unacknowledged != 0 {
		t.Fatalf("the attempt limit should give the batch up, got %+v", stats)
	}
	told := false
	for _, err := range heard {
		told = told || contains(err.Error(), "3 items were not recorded after 2 attempts")
	}
	if !told {
		t.Errorf("the host was not told what was given up, and why: %v", heard)
	}
}

func TestAnUnreachableCollectorNeverUsesUpABatchsAttempts(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxBatchAttempts = 2
	driver := NewDriver(settings)
	clock := onFakeTime(driver)
	captureEvents(t, driver, 1)

	for i := 0; i < 20; i++ {
		_, _ = driver.Flush()
		clock.advance(time.Minute)
	}
	if stats := driver.Stats(); stats.Lost != 0 || stats.Unacknowledged != 1 {
		t.Fatalf("a batch that never left was given up: %+v", stats)
	}
}

func TestAPipelinedBatchIsHeldWhenItsConnectionFails(t *testing.T) {
	driver := pipelinedDriver("127.0.0.1:1", 4)
	clock := onFakeTime(driver)
	mustCapture(t, driver)

	if _, err := driver.Submit(); err == nil {
		t.Fatal("a closed port reported success")
	}
	if held := driver.OutstandingItems(); held != 2 {
		t.Fatalf("the batch should be held, got %d items", held)
	}
	var later *RetryLaterError
	if _, err := driver.Submit(); !errors.As(err, &later) {
		t.Fatalf("a submit inside the wait returned %v", err)
	}

	fake := startPipelinedCollector(t, 0, 8)
	driver.settings.CollectorAddress = fake.address()
	driver.pipeline = nil
	clock.advance(later.Wait)
	if _, err := driver.Submit(); err != nil {
		t.Fatalf("submit: %v", err)
	}
	receipts, err := driver.Drain()
	if err != nil || len(receipts) != 1 || receipts[0].Accepted != 2 {
		t.Fatalf("the held batch did not go: %v, %v", err, receipts)
	}
}

func TestADryRunWritesEachItemAndOpensNoConnection(t *testing.T) {
	var out bytes.Buffer
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.DryRun = &out
	driver := NewDriver(settings)
	if err := driver.Capture(Event("signed-up")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if err := driver.Capture(Event("paid")); err != nil {
		t.Fatalf("capture: %v", err)
	}

	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("a dry run reached for a collector: %v", err)
	}
	if receipt.Accepted != 2 || receipt.DurableCopies != 0 {
		t.Errorf("a dry run made nothing durable, and reports %+v", receipt)
	}
	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if len(lines) != 2 || !contains(lines[0], "signed-up") || !contains(lines[1], "paid") {
		t.Fatalf("the dry run wrote %q", out.String())
	}
}

func TestOneTickSendsWhatIsSealedAndTheNextCollectsItsReceipt(t *testing.T) {
	// Run is this on a timer. Capture only buffers, so without it a service that
	// never calls Flush sends nothing until it exits.
	fake := startPipelinedCollector(t, 0, 8)
	settings := NewSettings(fake.address(), "key-a")
	settings.Linger = 0
	driver := NewDriver(settings)
	captureEvents(t, driver, 3)

	driver.tick()
	if driver.Buffered() != 0 || driver.Outstanding() != 1 {
		t.Fatalf("a tick should send what is sealed: %+v", driver.Stats())
	}
	driver.tick()
	if stats := driver.Stats(); stats.Accepted != 3 || stats.Unacknowledged != 0 {
		t.Fatalf("the next tick should collect the receipt: %+v", stats)
	}
}

func TestRunShutsTheDriverDownWhenItsContextEnds(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	onFakeTime(driver)
	captureEvents(t, driver, 2)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if left := driver.Run(ctx); left != 2 {
		t.Fatalf("run should report what its shutdown left, got %d", left)
	}
}

func TestAHostCallbackThatPanicsStaysInsideTheDriver(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.OnError = func(error) { panic("the host's own bug") }
	driver := NewDriver(settings)
	onFakeTime(driver)
	captureEvents(t, driver, 1)

	if _, err := driver.Flush(); err == nil {
		t.Fatal("an unreachable collector reported success")
	}
}

func equalInts(a, b []int) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
