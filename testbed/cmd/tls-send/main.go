// Command tls-send sends events to a collector over TLS and prints one JSON
// line that says what the app driver saw.
//
// `./tools.sh kind-check` runs it against a collector in a disposable cluster,
// once with the installation's root and once with a root the collector's
// certificate does not chain to. The key is read from a file so that it never
// appears in a process listing.
package main

import (
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"strings"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

type outcome struct {
	Accepted  uint64   `json:"accepted"`
	Durable   uint64   `json:"durable_copies"`
	Rejected  int      `json:"rejected"`
	Lost      uint64   `json:"lost"`
	Left      int      `json:"left_at_shutdown"`
	Errors    []string `json:"errors"`
	FlushFail string   `json:"flush_error,omitempty"`
}

func main() {
	address := flag.String("address", "", "the collector, host:port")
	keyFile := flag.String("key-file", "", "a file that holds the project key")
	rootFile := flag.String("root", "", "a PEM file of the authority to trust")
	serverName := flag.String("server-name", "", "the name on the collector certificate")
	count := flag.Int("count", 10, "how many events to send")
	flag.Parse()

	key, err := os.ReadFile(*keyFile)
	if err != nil {
		fail("the key file could not be read: %v", err)
	}
	pem, err := os.ReadFile(*rootFile)
	if err != nil {
		fail("the root file could not be read: %v", err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(pem) {
		fail("%s holds no certificate", *rootFile)
	}

	var result outcome
	settings := tallyowl.NewSettings(*address, strings.TrimSpace(string(key)))
	settings.Transport = tallyowl.Transport{TLS: &tls.Config{RootCAs: roots, ServerName: *serverName}}
	settings.OnError = func(err error) { result.Errors = append(result.Errors, err.Error()) }
	driver := tallyowl.NewDriver(settings)
	for i := 0; i < *count; i++ {
		_ = driver.Capture(tallyowl.Event("kind-check").
			WithSession("kind-check").
			WithProperty("n", tallyowl.Int(int64(i))))
	}
	if receipt, err := driver.Flush(); err != nil {
		result.FlushFail = err.Error()
	} else if receipt != nil {
		result.Accepted = receipt.Accepted
		result.Durable = receipt.DurableCopies
		result.Rejected = len(receipt.Rejected)
	}
	result.Left = driver.Shutdown()
	result.Lost = driver.Stats().Lost
	line, _ := json.Marshal(result)
	fmt.Println(string(line))
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "tls-send: "+format+"\n", args...)
	os.Exit(2)
}
