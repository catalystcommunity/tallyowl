// D33 prototype: Corndogs as the collector durability boundary.
//
// DELIVERY.md makes Corndogs the single intermediate durable handoff. Two
// numbers decide whether that holds:
//
//   1. SubmitTask throughput at a realistic TallyOwl batch payload, because the
//      collector acknowledges an app driver only after that call returns.
//   2. CleanUpTimedOut cost at outage-buffer depth, because the forwarder must
//      call it on an interval and the file backend examines every live task.
//
// This is decision-support code. It is not product code.
package main

import (
	"context"
	"fmt"
	"os"
	"sort"
	"sync"
	"time"

	cd "github.com/CatalystCommunity/corndogs/clients/corndogs"
)

func percentiles(d []time.Duration) (p50, p95, p99 time.Duration) {
	sort.Slice(d, func(i, j int) bool { return d[i] < d[j] })
	at := func(q float64) time.Duration {
		if len(d) == 0 {
			return 0
		}
		return d[int(float64(len(d)-1)*q)]
	}
	return at(0.50), at(0.95), at(0.99)
}

// submitLoad runs `workers` concurrent submitters for `perWorker` tasks each.
func submitLoad(addr, queue string, payload []byte, workers, perWorker int) (float64, time.Duration, time.Duration) {
	var wg sync.WaitGroup
	lat := make([][]time.Duration, workers)
	start := time.Now()
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			c := cd.New(addr)
			ls := make([]time.Duration, 0, perWorker)
			for i := 0; i < perWorker; i++ {
				t0 := time.Now()
				_, err := c.SubmitTask(context.Background(), cd.SubmitTaskRequest{
					Queue:           queue,
					CurrentState:    "queued",
					AutoTargetState: "sending",
					Timeout:         30,
					Payload:         payload,
				})
				if err != nil {
					fmt.Fprintln(os.Stderr, "submit:", err)
					return
				}
				ls = append(ls, time.Since(t0))
			}
			lat[w] = ls
		}(w)
	}
	wg.Wait()
	el := time.Since(start)
	all := []time.Duration{}
	for _, l := range lat {
		all = append(all, l...)
	}
	p50, _, p99 := percentiles(all)
	return float64(len(all)) / el.Seconds(), p50, p99
}

// sweepCurve measures sweep time against live task count on a database that
// holds nothing else. The first delivery run confounded these two, because the
// submit phases had already left tens of thousands of tasks behind.
func sweepCurve(addr string, payloadBytes int) {
	c := cd.New(addr)
	fmt.Println("# Sweep cost against live task count")
	fmt.Printf("payload = %d bytes, one queue, nothing else in the database\n\n", payloadBytes)
	fmt.Printf("%-14s %12s %14s %16s\n", "live tasks", "sweep ms", "us per task", "fits 1s interval")

	payload := make([]byte, payloadBytes)
	live := 0
	depths := []int{1000, 5000, 20000, 50000}
	if v := os.Getenv("MAXDEPTH"); v != "" {
		var md int
		fmt.Sscanf(v, "%d", &md)
		trimmed := []int{}
		for _, d := range depths {
			if d <= md {
				trimmed = append(trimmed, d)
			}
		}
		depths = trimmed
	}
	for _, target := range depths {
		for live < target {
			if _, err := c.SubmitTask(context.Background(), cd.SubmitTaskRequest{
				Queue: "curve", CurrentState: "backoff", AutoTargetState: "queued",
				Timeout: 3600, Payload: payload,
			}); err != nil {
				fmt.Fprintln(os.Stderr, "fill:", err)
				return
			}
			live++
		}
		var best time.Duration = time.Hour
		for i := 0; i < 3; i++ {
			t0 := time.Now()
			if _, err := c.CleanUpTimedOut(context.Background(), cd.CleanUpTimedOutRequest{
				AtTime: time.Now().Add(-time.Hour).UnixNano(), Queue: "curve",
			}); err != nil {
				fmt.Fprintln(os.Stderr, "sweep:", err)
				return
			}
			if d := time.Since(t0); d < best {
				best = d
			}
		}
		fits := "yes"
		if best > time.Second {
			fits = "NO"
		}
		fmt.Printf("%-14d %12.1f %14.1f %16s\n",
			live, float64(best.Microseconds())/1000.0,
			float64(best.Microseconds())/float64(live), fits)
	}
}

// acceptPath compares the two collector accept paths end to end.
//
//	A: submit the whole batch payload to Corndogs, which is the current design.
//	B: append the payload to the spool, then submit a small reference task,
//	   which is what D4 now requires.
//
// Both must complete before the collector may acknowledge the app driver, so
// both are measured to completion.
func acceptPath(addr string) {
	spoolDir := os.Getenv("HOME") + "/.cache/tallyowl-bench/spool"
	os.MkdirAll(spoolDir, 0o755)

	fmt.Println("# Collector accept path: payload in the task against payload in the spool")
	fmt.Println("addr =", addr)
	fmt.Println("spool =", spoolDir)
	fmt.Println()
	fmt.Printf("%-26s %8s %10s %10s %12s %12s %11s\n",
		"path", "workers", "batches/s", "MiB/s", "p50", "p99", "spool grp")

	for _, sz := range []int{64 * 1024, 512 * 1024} {
		payload := make([]byte, sz)
		for i := range payload {
			payload[i] = byte(i)
		}
		for _, workers := range []int{1, 8, 32} {
			per := 60
			if sz < 512*1024 {
				per = 150
			}

			// Path A: the whole payload rides in the task.
			qa := fmt.Sprintf("accept-a-%d-%d", sz, workers)
			rateA, p50A, p99A := submitLoad(addr, qa, payload, workers, per)
			fmt.Printf("%-26s %8d %10.0f %10.1f %12s %12s %11s\n",
				fmt.Sprintf("A: in task, %d KiB", sz/1024), workers, rateA,
				rateA*float64(sz)/(1024*1024),
				p50A.Round(time.Microsecond), p99A.Round(time.Microsecond), "-")

			// Path B: spool the payload, then submit a reference.
			sp, err := newSpool(fmt.Sprintf("%s/spool-%d-%d.log", spoolDir, sz, workers), 2*time.Millisecond)
			if err != nil {
				fmt.Fprintln(os.Stderr, "spool:", err)
				return
			}
			qb := fmt.Sprintf("accept-b-%d-%d", sz, workers)
			rateB, p50B, p99B := spoolLoad(addr, qb, payload, workers, per, sp)
			_, meanG, _ := sp.stats()
			fmt.Printf("%-26s %8d %10.0f %10.1f %12s %12s %11.1f\n",
				fmt.Sprintf("B: spooled, %d KiB", sz/1024), workers, rateB,
				rateB*float64(sz)/(1024*1024),
				p50B.Round(time.Microsecond), p99B.Round(time.Microsecond), meanG)
		}
	}
	os.RemoveAll(spoolDir)
}

// spoolLoad runs path B: durable spool append, then a small reference task.
func spoolLoad(addr, queue string, payload []byte, workers, perWorker int, sp *spool) (float64, time.Duration, time.Duration) {
	var wg sync.WaitGroup
	lat := make([][]time.Duration, workers)
	start := time.Now()
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			c := cd.New(addr)
			ls := make([]time.Duration, 0, perWorker)
			for i := 0; i < perWorker; i++ {
				t0 := time.Now()
				ref, err := sp.put(payload)
				if err != nil {
					fmt.Fprintln(os.Stderr, "spool put:", err)
					return
				}
				// The task carries the batch ID, the reference, and a little
				// metadata. Roughly 256 bytes in the real design.
				task := make([]byte, 0, 256)
				task = append(task, []byte(ref)...)
				task = append(task, make([]byte, 224)...)
				if _, err := c.SubmitTask(context.Background(), cd.SubmitTaskRequest{
					Queue: queue, CurrentState: "queued", AutoTargetState: "sending",
					Timeout: 30, Payload: task,
				}); err != nil {
					fmt.Fprintln(os.Stderr, "submit:", err)
					return
				}
				ls = append(ls, time.Since(t0))
			}
			lat[w] = ls
		}(w)
	}
	wg.Wait()
	el := time.Since(start)
	all := []time.Duration{}
	for _, l := range lat {
		all = append(all, l...)
	}
	if len(all) == 0 {
		return 0, 0, 0
	}
	p50, _, p99 := percentiles(all)
	return float64(len(all)) / el.Seconds(), p50, p99
}

func main() {
	addr := os.Getenv("CORNDOGS_ADDR")
	if addr == "" {
		addr = "127.0.0.1:5080"
	}

	if os.Getenv("MODE") == "accept-path" {
		acceptPath(addr)
		return
	}

	if os.Getenv("MODE") == "sweep-curve" {
		sz := 256
		if v := os.Getenv("PAYLOAD"); v != "" {
			fmt.Sscanf(v, "%d", &sz)
		}
		sweepCurve(addr, sz)
		return
	}

	c := cd.New(addr)

	fmt.Println("# D33 Corndogs delivery benchmark")
	fmt.Println("addr =", addr)
	fmt.Printf("GROUP_MAX_DELAY = %q\n", os.Getenv("GROUP_MAX_DELAY_LABEL"))
	fmt.Println()

	// A TallyOwl batch seals at 256 items or 512 KiB (D19). Measure both the
	// small-payload case and the realistic batch case.
	sizes := []struct {
		name string
		n    int
	}{
		{"1 KiB", 1024},
		{"64 KiB", 64 * 1024},
		{"512 KiB (D19 seal)", 512 * 1024},
	}

	fmt.Println("## SubmitTask throughput (this is the collector acknowledgement path)")
	fmt.Printf("%-22s %8s %14s %14s %14s %14s\n", "payload", "workers", "tasks/s", "MiB/s", "p50", "p99")
	for _, s := range sizes {
		payload := make([]byte, s.n)
		for i := range payload {
			payload[i] = byte(i)
		}
		for _, workers := range []int{1, 8, 32} {
			per := 400
			if s.n >= 512*1024 {
				per = 100
			}
			q := fmt.Sprintf("bench-%d-%d", s.n, workers)
			rate, p50, p99 := submitLoad(addr, q, payload, workers, per)
			fmt.Printf("%-22s %8d %14.0f %14.1f %14s %14s\n",
				s.name, workers, rate, rate*float64(s.n)/(1024*1024), p50.Round(time.Microsecond), p99.Round(time.Microsecond))
		}
	}

	if os.Getenv("MODE") == "submit" {
		return
	}

	// The sweep. DELIVERY.md gives the forwarder this job on an interval, and
	// the file backend examines every live task, so the cost grows with the
	// outage buffer depth.
	fmt.Println()
	fmt.Println("## CleanUpTimedOut sweep cost against queue depth")
	fmt.Printf("%-30s %12s %10s %16s\n", "backlog depth (4 KiB payload)", "sweep ms", "swept", "per 1s interval")

	payload := make([]byte, 4*1024)
	depths := []int{2000, 10000, 40000}
	built := 0
	for _, depth := range depths {
		// Grow a parked backlog: tasks in a waiting state with a timeout, which
		// is exactly the D33 backoff shape.
		for built < depth {
			_, err := c.SubmitTask(context.Background(), cd.SubmitTaskRequest{
				Queue:           "sweep",
				CurrentState:    "backoff",
				AutoTargetState: "queued",
				Timeout:         3600,
				Payload:         payload,
			})
			if err != nil {
				fmt.Fprintln(os.Stderr, "fill:", err)
				break
			}
			built++
		}
		// Sweep with a time that expires nothing, so this measures the scan
		// itself rather than the state changes.
		var best time.Duration = time.Hour
		var swept int64
		for i := 0; i < 3; i++ {
			t0 := time.Now()
			r, err := c.CleanUpTimedOut(context.Background(), cd.CleanUpTimedOutRequest{
				AtTime: time.Now().Add(-time.Hour).UnixNano(),
				Queue:  "sweep",
			})
			if err != nil {
				fmt.Fprintln(os.Stderr, "sweep:", err)
				break
			}
			d := time.Since(t0)
			if d < best {
				best = d
			}
			swept = r.TimedOut
		}
		fmt.Printf("%-30d %12.1f %10d %15.1f%%\n",
			depth, float64(best.Microseconds())/1000.0, swept, float64(best)/float64(time.Second)*100)
	}

	// The file backend decodes every live task during a sweep, so payload size
	// is part of the sweep cost. Measure that at a fixed depth.
	fmt.Println()
	fmt.Println("## Sweep cost against payload size, at 2000 parked tasks")
	fmt.Printf("%-22s %12s\n", "payload", "sweep ms")
	for _, sz := range []int{4 * 1024, 64 * 1024, 512 * 1024} {
		q := fmt.Sprintf("sweepsz-%d", sz)
		pl := make([]byte, sz)
		for i := 0; i < 2000; i++ {
			if _, err := c.SubmitTask(context.Background(), cd.SubmitTaskRequest{
				Queue: q, CurrentState: "backoff", AutoTargetState: "queued",
				Timeout: 3600, Payload: pl,
			}); err != nil {
				fmt.Fprintln(os.Stderr, "fill:", err)
				break
			}
		}
		var best time.Duration = time.Hour
		for i := 0; i < 3; i++ {
			t0 := time.Now()
			if _, err := c.CleanUpTimedOut(context.Background(), cd.CleanUpTimedOutRequest{
				AtTime: time.Now().Add(-time.Hour).UnixNano(), Queue: q,
			}); err != nil {
				break
			}
			if d := time.Since(t0); d < best {
				best = d
			}
		}
		fmt.Printf("%-22s %12.1f\n", fmt.Sprintf("%d KiB", sz/1024), float64(best.Microseconds())/1000.0)
	}

	fmt.Println()
	fmt.Println("## Queue state after the run")
	counts, err := c.GetQueueAndStateCounts(context.Background(), cd.GetQueueAndStateCountsRequest{})
	if err == nil {
		for q, v := range counts.QueueAndStateCounts {
			fmt.Printf("%-24s total %d\n", q, v.Count)
		}
	}
}
