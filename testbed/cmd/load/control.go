package main

import (
	"encoding/binary"
	"fmt"
	"time"

	collectorapi "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	controlapi "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-control-api"
	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

// ControlClient runs queries against the head.
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

// PointLookup runs an exact lookup on one high-cardinality value and says how
// many rows came back. The count is the honesty of the measurement: the alpha
// report timed lookups that found nothing and nobody could tell (L153), so a
// caller that ignores the count is repeating that mistake.
func (c *ControlClient) PointLookup(projectID []byte, requestID string) (int, error) {
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
	return c.run(filterNode)
}

// Trend runs a count by minute over the whole range.
func (c *ControlClient) Trend(projectID []byte) (int, error) {
	scan := controlapi.ScanNode{
		Scan:      "events",
		ProjectId: projectID,
		Range:     wholeRange(),
	}
	scanNode := node(controlapi.QueryNodeKind("scan"), func(b *controlapi.QueryNodeBox) {
		b.Scan = &scan
	})
	interval := controlapi.Interval{FixedMs: ptr(controlapi.DurationMs(60_000))}
	aggregate := controlapi.AggregateNode{
		Dimensions: []controlapi.Dimension{},
		Measures: []controlapi.Measure{{
			Kind:  "count",
			Alias: "events",
		}},
		Interval: &interval,
		Input:    controlapi.QueryNodeRef(controlapi.EncodeQueryNodeBox(scanNode)),
	}
	aggregateNode := node(controlapi.QueryNodeKind("aggregate"), func(b *controlapi.QueryNodeBox) {
		b.Aggregate = &aggregate
	})
	return c.run(aggregateNode)
}

func (c *ControlClient) run(root controlapi.QueryNodeBox) (int, error) {
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
		return 0, err
	}
	if response.Variant != nil && *response.Variant == "ServiceError" {
		failure, decodeErr := controlapi.DecodeServiceError(response.Payload)
		if decodeErr != nil {
			return 0, decodeErr
		}
		return 0, fmt.Errorf("%s", failure.Message)
	}
	decoded, err := controlapi.DecodeQueryResponse(response.Payload)
	if err != nil {
		return 0, err
	}
	return len(decoded.Rows), nil
}

func wholeRange() controlapi.TimeRange {
	now := time.Now().UnixMilli()
	return controlapi.TimeRange{
		RangeStart: controlapi.Timestamp(now - 24*60*60*1000),
		RangeEnd:   controlapi.Timestamp(now + 60_000),
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

func ptr[T any](value T) *T { return &value }

// projectFromResolve asks the head which project a credential reaches.
//
// The application above never asks and never needs to. This harness stands in
// for an operator with the key in hand, so that it can name a project in a
// query without a dashboard sign-in.
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

var _ = binary.BigEndian
