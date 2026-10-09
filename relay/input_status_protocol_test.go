package main

import (
	"encoding/json"
	"testing"
)

func TestInputStatusWarningFields(t *testing.T) {
	base := map[string]any{"type": "input-status", "protocol": 1, "generation": 4,
		"receivedPackets": 12, "acceptedPackets": 11, "failedPackets": 1, "droppedPackets": 1,
		"releasedInputs": 2, "requestedEvents": 14, "appliedEvents": 13}
	validate := func(extra map[string]any, role string) bool {
		t.Helper()
		message := make(map[string]any)
		for key, value := range base {
			message[key] = value
		}
		for key, value := range extra {
			message[key] = value
		}
		raw, err := json.Marshal(message)
		if err != nil {
			t.Fatal(err)
		}
		var parsed map[string]json.RawMessage
		if err := json.Unmarshal(raw, &parsed); err != nil {
			t.Fatal(err)
		}
		return validSignalMessage(parsed, role, "", len(raw))
	}
	if !validate(nil, "host") {
		t.Fatal("old receipt rejected")
	}
	for _, reason := range []any{nil, "admin-required", "admin-unavailable"} {
		for _, route := range []any{"local", "admin", "secure"} {
			fields := map[string]any{"reason": reason, "route": route}
			if !validate(fields, "host") {
				t.Fatalf("valid warning rejected: %v", fields)
			}
			if validate(fields, "client") {
				t.Fatal("client warning accepted")
			}
		}
	}
	for _, reason := range []any{"", "policy-off", false, 1, map[string]any{}, []any{}} {
		if validate(map[string]any{"reason": reason}, "host") {
			t.Fatalf("invalid reason accepted: %v", reason)
		}
	}
	for _, route := range []any{nil, "", "system", false, 1, map[string]any{}, []any{}} {
		if validate(map[string]any{"route": route}, "host") {
			t.Fatalf("invalid route accepted: %v", route)
		}
	}
}
