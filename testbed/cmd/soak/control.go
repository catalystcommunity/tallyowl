package main

import (
	"fmt"

	collectorapi "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	controlapi "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-control-api"
	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

// ControlClient runs the two query shapes reconciliation needs. It is the load
// harness's client with the answers read rather than only timed: the soak needs
// the count, not the latency.
type ControlClient struct {
	client  *tallyowl.Client
	session string
}

func NewControlClient(address, session string) *ControlClient {
	return &ControlClient{
		client:  tallyowl.NewClient(address, 16*1024*1024),
		session: session,
	}
}

func (c *ControlClient) Close() { c.client.Close() }

// LookupCount is how many rows an exact lookup on one request ID answers.
// One is the only right answer for an acknowledged event: zero is a loss and
// two is a duplicate.
func (c *ControlClient) LookupCount(projectID []byte, requestID string) (int64, error) {
	scan := controlapi.ScanNode{
		Scan:      "events",
		ProjectId: projectID,
		Range:     wholeRange(),
	}
	scanNode := node(controlapi.QueryNodeKind("scan"), func(b *controlapi.QueryNodeBox) {
		b.Scan = &scan
	})
	predicate := compareText("request_id", requestID)
	filter := controlapi.FilterNode{
		Filter: controlapi.ExpressionRef(controlapi.EncodeExpressionNode(predicate)),
		Input:  controlapi.QueryNodeRef(controlapi.EncodeQueryNodeBox(scanNode)),
	}
	filterNode := node(controlapi.QueryNodeKind("filter"), func(b *controlapi.QueryNodeBox) {
		b.Filter = &filter
	})
	response, err := c.run(filterNode)
	if err != nil {
		return 0, err
	}
	return int64(len(response.Rows)), nil
}

// CountIn counts the logical events in one closed window. Deduplication makes
// this a count of logical events however many physical rows carry them, so it
// proves no loss and no duplication with one number.
func (c *ControlClient) CountIn(projectID []byte, startMs, endMs int64) (int64, error) {
	scan := controlapi.ScanNode{
		Scan:      "events",
		ProjectId: projectID,
		Range: controlapi.TimeRange{
			RangeStart: controlapi.Timestamp(startMs),
			RangeEnd:   controlapi.Timestamp(endMs),
			Basis:      "occurred_at",
		},
	}
	scanNode := node(controlapi.QueryNodeKind("scan"), func(b *controlapi.QueryNodeBox) {
		b.Scan = &scan
	})
	aggregate := controlapi.AggregateNode{
		Dimensions: []controlapi.Dimension{},
		Measures: []controlapi.Measure{{
			Kind:  "count",
			Alias: "events",
		}},
		Input: controlapi.QueryNodeRef(controlapi.EncodeQueryNodeBox(scanNode)),
	}
	aggregateNode := node(controlapi.QueryNodeKind("aggregate"), func(b *controlapi.QueryNodeBox) {
		b.Aggregate = &aggregate
	})
	response, err := c.run(aggregateNode)
	if err != nil {
		return 0, err
	}
	var total int64
	for _, row := range response.Rows {
		for _, value := range row.Values {
			total += numeric(value)
		}
	}
	return total, nil
}

// numeric is the count carried by one typed value, whatever width it took.
func numeric(value controlapi.TypedValue) int64 {
	switch {
	case value.IntValue != nil:
		return *value.IntValue
	case value.UintValue != nil:
		return int64(*value.UintValue)
	case value.FloatValue != nil:
		return int64(*value.FloatValue)
	default:
		return 0
	}
}

func (c *ControlClient) run(root controlapi.QueryNodeBox) (*controlapi.QueryResponse, error) {
	encoded := controlapi.QueryNodeRef(controlapi.EncodeQueryNodeBox(root))
	request := controlapi.QueryRequest{
		AlgebraVersion: 1,
		Consistency:    "committed",
		AllowPartial:   false,
		Form:           "node",
		Node:           &encoded,
	}
	response, err := c.client.Call(
		"TallyOwlControl", "run-query",
		controlapi.EncodeQueryRequest(request), &c.session)
	if err != nil {
		return nil, err
	}
	if response.Variant != nil && *response.Variant == "ServiceError" {
		failure, decodeErr := controlapi.DecodeServiceError(response.Payload)
		if decodeErr != nil {
			return nil, decodeErr
		}
		return nil, fmt.Errorf("%s", failure.Message)
	}
	decoded, err := controlapi.DecodeQueryResponse(response.Payload)
	if err != nil {
		return nil, err
	}
	return &decoded, nil
}

func wholeRange() controlapi.TimeRange {
	// The soak can run for weeks, so the "whole" range for an exact lookup is
	// wide rather than clever: the locator makes it a probe either way.
	return controlapi.TimeRange{
		RangeStart: controlapi.Timestamp(0),
		RangeEnd:   controlapi.Timestamp(1 << 46),
		Basis:      "occurred_at",
	}
}

func node(kind controlapi.QueryNodeKind, fill func(*controlapi.QueryNodeBox)) controlapi.QueryNodeBox {
	box := controlapi.QueryNodeBox{Node: kind}
	fill(&box)
	return box
}

func compareText(field, value string) controlapi.ExpressionNode {
	fieldRef := controlapi.FieldRef{Name: field}
	left := controlapi.ExpressionNode{Expression: "field", Field: &fieldRef}
	text := value
	literalValue := controlapi.TypedValue{Kind: "text", TextValue: &text}
	right := controlapi.ExpressionNode{Expression: "literal", Literal: &literalValue}
	compare := controlapi.CompareExpr{
		Compare: "eq",
		Left:    controlapi.ExpressionRef(controlapi.EncodeExpressionNode(left)),
		Right:   controlapi.ExpressionRef(controlapi.EncodeExpressionNode(right)),
	}
	return controlapi.ExpressionNode{Expression: "compare", Compare: &compare}
}

// projectFromResolve asks the head which project a credential reaches, exactly
// as the load harness does and for the same reason: this stands in for an
// operator with the key in hand.
func projectFromResolve(head, credential string) ([]byte, error) {
	client := tallyowl.NewClient(head, 16*1024*1024)
	defer client.Close()
	request := collectorapi.ResolveKeyRequest{Credential: credential}
	response, err := client.Call(
		"TallyOwlCollector", "resolve-key",
		collectorapi.EncodeResolveKeyRequest(request), nil)
	if err != nil {
		return nil, err
	}
	if response.Variant != nil && *response.Variant == "ServiceError" {
		return nil, fmt.Errorf("the head does not know that key")
	}
	resolved, err := collectorapi.DecodeResolveKeyResponse(response.Payload)
	if err != nil {
		return nil, err
	}
	return resolved.ProjectId, nil
}
