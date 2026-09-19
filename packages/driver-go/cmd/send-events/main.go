// Send telemetry to a running collector, from the Go app driver.
//
// This is what the cross-language integration test runs. It exists as a program
// rather than as a Go test so that a Rust test can start the real collector and
// the real head, then prove that the Go driver reaches them over a real socket.
//
//	go run ./cmd/send-events <collector-address> <credential> <occurred-at-ms>
//
// It writes one line of JSON to standard output, so the caller can compare what
// it sent with what a query returns.
package main

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"strconv"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

func main() {
	if len(os.Args) < 4 {
		fmt.Fprintln(os.Stderr, "usage: send-events <collector-address> <credential> <occurred-at-ms>")
		os.Exit(2)
	}
	address, credential := os.Args[1], os.Args[2]
	at, err := strconv.ParseInt(os.Args[3], 10, 64)
	if err != nil {
		fmt.Fprintf(os.Stderr, "the third argument is a time in milliseconds: %v\n", err)
		os.Exit(2)
	}

	driver := tallyowl.NewDriver(
		tallyowl.NewSettings(address, credential).WithProperty("service", "seedstore-api"))

	// One of each shape the projection has to tell apart. A page view that
	// arrived as an event, or an unsigned property that arrived signed, is the
	// failure this whole path exists to rule out.
	captures := []*tallyowl.Capture{
		tallyowl.Event("checkout-started").At(at).WithProperty("plan", tallyowl.Text("pro")),
		tallyowl.PageView("/pricing").At(at + 1).WithProperty("attempts", tallyowl.Uint(2)),
		tallyowl.Conversion("purchase", ptr(tallyowl.MustDecimal("19.99")), "USD").At(at + 2),
		tallyowl.Error("TypeError", "x is not a function", false).At(at + 3),
	}

	ids := make([]string, 0, len(captures))
	for _, capture := range captures {
		if err := driver.Capture(capture); err != nil {
			fmt.Fprintf(os.Stderr, "capture: %v\n", err)
			os.Exit(1)
		}
		ids = append(ids, hex.EncodeToString(capture.EventID()))
	}

	receipt, err := driver.Flush()
	if err != nil {
		fmt.Fprintf(os.Stderr, "flush: %v\n", err)
		os.Exit(1)
	}
	if receipt == nil {
		fmt.Fprintln(os.Stderr, "the flush sent nothing")
		os.Exit(1)
	}

	out, _ := json.Marshal(map[string]any{
		"accepted":       receipt.Accepted,
		"durable_copies": receipt.DurableCopies,
		"batch_id":       hex.EncodeToString(receipt.BatchID),
		"event_ids":      ids,
	})
	fmt.Println(string(out))

	if unsent := driver.Shutdown(); unsent != 0 {
		fmt.Fprintf(os.Stderr, "%d items did not go\n", unsent)
		os.Exit(1)
	}
}

func ptr(v tallyowl.Value) *tallyowl.Value { return &v }
