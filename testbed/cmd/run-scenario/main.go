// Run one scenario against a running installation, and write its ledger.
//
// The simulator writes the ledger **before** it sends anything, so the expected
// result cannot be influenced by what TallyOwl did with the data. The harness
// then runs the equivalent query and compares. See docs/TESTBED.md section 5.
//
//	go run ./cmd/run-scenario <collector-address> <ledger-path> <start-at-ms> \
//	    <credential> [<credential> ...]
//
// The credentials come from the caller because TallyOwl issues them. An
// application never chooses its own key and never learns its project.
//
// A scenario names its seed, so a failure reproduces exactly.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"

	"github.com/CatalystCommunity/tallyowl/testbed/api"
	"github.com/CatalystCommunity/tallyowl/testbed/simulator"
)

func main() {
	if len(os.Args) < 5 {
		fmt.Fprintln(os.Stderr,
			"usage: run-scenario <collector-address> <ledger-path> <start-at-ms> <credential>...")
		os.Exit(2)
	}
	address, ledgerPath := os.Args[1], os.Args[2]
	startAt, err := strconv.ParseInt(os.Args[3], 10, 64)
	if err != nil {
		fmt.Fprintf(os.Stderr, "the third argument is a time in milliseconds: %v\n", err)
		os.Exit(2)
	}

	scenario := simulator.FastScenario(startAt, os.Args[4:])
	stream, book, err := simulator.Expand(scenario)
	if err != nil {
		fmt.Fprintf(os.Stderr, "the scenario could not be expanded: %v\n", err)
		os.Exit(1)
	}

	// The ledger lands on disk first. A ledger written afterwards could only
	// ever agree with itself.
	if err := book.Write(ledgerPath); err != nil {
		fmt.Fprintf(os.Stderr, "the ledger could not be written: %v\n", err)
		os.Exit(1)
	}

	// One backend for each application. Each holds its own credential, and
	// nothing in the stream names a project.
	backends := map[string]*api.Backend{}
	for _, application := range scenario.Applications {
		backend := api.New(address, application.Credential, application.Name)
		if _, err := backend.Listen(); err != nil {
			fmt.Fprintf(os.Stderr, "the %s backend could not listen: %v\n", application.Name, err)
			os.Exit(1)
		}
		backends[application.Credential] = backend
	}

	sent := 0
	for _, item := range stream {
		backend := backends[item.Credential]
		if err := backend.Driver().Capture(item.Capture); err != nil {
			fmt.Fprintf(os.Stderr, "capture: %v\n", err)
			os.Exit(1)
		}
		sent++
	}

	accepted := map[string]uint64{}
	for credential, backend := range backends {
		receipt, err := backend.Flush()
		if err != nil {
			fmt.Fprintf(os.Stderr, "flush: %v\n", err)
			os.Exit(1)
		}
		if receipt != nil {
			accepted[credential] = receipt.Accepted
		}
		if unsent := backend.Close(); unsent != 0 {
			fmt.Fprintf(os.Stderr, "%d items did not go for %s\n", unsent, credential)
			os.Exit(1)
		}
	}

	out, _ := json.Marshal(map[string]any{
		"scenario": scenario.Name,
		"seed":     scenario.Seed,
		"sent":     sent,
		"accepted": accepted,
	})
	fmt.Println(string(out))
}
