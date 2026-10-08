package main

import (
	"encoding/json"
	"os"
	"strings"
	"testing"
)

func TestSecureDesktopProtocolContract(t *testing.T) {
	data, err := os.ReadFile("../fixtures/secure-desktop-contract.json")
	if err != nil {
		t.Fatal(err)
	}
	var contract struct {
		DesktopStates       []json.RawMessage `json:"desktopStates"`
		RecoveredState      json.RawMessage   `json:"recoveredState"`
		SecureActions       []json.RawMessage `json:"secureActions"`
		SecureActionResults []json.RawMessage `json:"secureActionResults"`
		InvalidMessages     []struct {
			Name, Role string
			Message    json.RawMessage
		} `json:"invalidMessages"`
		RoleCases []struct {
			Role     string
			Message  json.RawMessage
			Accepted bool
		} `json:"roleCases"`
		Limits struct{ MessageMaxBytes, ReasonMaxUtf8Bytes int } `json:"limits"`
	}
	if err := json.Unmarshal(data, &contract); err != nil {
		t.Fatal(err)
	}
	validate := func(raw []byte, role string) bool {
		var message map[string]json.RawMessage
		if err := json.Unmarshal(raw, &message); err != nil {
			t.Fatal(err)
		}
		return validSignalMessage(message, role, "", len(raw))
	}
	checkValid := func(raw json.RawMessage, role string) {
		t.Helper()
		var compact json.RawMessage
		var message any
		if err := json.Unmarshal(raw, &message); err != nil {
			t.Fatal(err)
		}
		compact, err = json.Marshal(message)
		if err != nil {
			t.Fatal(err)
		}
		if !validate(compact, role) {
			t.Fatalf("valid %s message rejected: %s", role, compact)
		}
		wrongRole := "host"
		if role == "host" {
			wrongRole = "client"
		}
		if validate(compact, wrongRole) {
			t.Fatalf("wrong role accepted: %s", compact)
		}
		padded := append(append([]byte(nil), compact...), []byte(strings.Repeat(" ", contract.Limits.MessageMaxBytes-len(compact)))...)
		if !validate(padded, role) || validate(append(padded, ' '), role) {
			t.Fatalf("wire byte boundary not enforced: %s", compact)
		}
	}
	for _, message := range contract.DesktopStates {
		checkValid(message, "host")
	}
	checkValid(contract.RecoveredState, "host")
	for _, message := range contract.SecureActions {
		checkValid(message, "client")
	}
	for _, message := range contract.SecureActionResults {
		checkValid(message, "host")
	}
	for _, invalid := range contract.InvalidMessages {
		var message any
		if err := json.Unmarshal(invalid.Message, &message); err != nil {
			t.Fatal(err)
		}
		raw, err := json.Marshal(message)
		if err != nil {
			t.Fatal(err)
		}
		if validate(raw, invalid.Role) {
			t.Fatalf("invalid fixture accepted: %s", invalid.Name)
		}
	}
	for _, roleCase := range contract.RoleCases {
		var message any
		if err := json.Unmarshal(roleCase.Message, &message); err != nil {
			t.Fatal(err)
		}
		raw, err := json.Marshal(message)
		if err != nil {
			t.Fatal(err)
		}
		if got := validate(raw, roleCase.Role); got != roleCase.Accepted {
			t.Fatalf("role %s accepted=%v want=%v: %s", roleCase.Role, got, roleCase.Accepted, raw)
		}
	}
	for _, raw := range []json.RawMessage{contract.DesktopStates[0], contract.SecureActionResults[0]} {
		var message map[string]any
		if err := json.Unmarshal(raw, &message); err != nil {
			t.Fatal(err)
		}
		for _, count := range []int{48, 49} {
			message["reason"] = strings.Repeat("é", count)
			encoded, err := json.Marshal(message)
			if err != nil {
				t.Fatal(err)
			}
			if got := validate(encoded, "host"); got != (count*2 <= contract.Limits.ReasonMaxUtf8Bytes) {
				t.Fatalf("UTF-8 reason boundary not enforced: count=%d accepted=%v", count, got)
			}
		}
	}
}
