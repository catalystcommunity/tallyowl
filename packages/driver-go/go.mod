module github.com/CatalystCommunity/tallyowl/packages/driver-go

go 1.26.3

require (
	github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api v0.2.1
	github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-control-api v0.2.1
	github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api v0.2.1
	github.com/catalystcommunity/csilgen/transports/go v0.0.0-20260801235357-d693a94d5b72
)

// The generated packages are checked in and live in this repository, so they
// resolve by path rather than through a registry. See docs/CI-CD.md section 5.
replace github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api => ../../generated/go/tallyowl-collector-api

replace github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-control-api => ../../generated/go/tallyowl-control-api

replace github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api => ../../generated/go/tallyowl-ingest-api
