package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestApprovalMethodsLegacyAndExplicitSelection(t *testing.T) {
	for _, test := range []struct {
		name    string
		methods []string
		totp    bool
		ntfy    bool
		want    []string
	}{
		{"legacy totp", nil, true, false, []string{"totp"}},
		{"legacy ntfy", nil, false, true, []string{"ntfy"}},
		{"legacy both", nil, true, true, []string{"totp", "ntfy"}},
		{"explicit host overrides saved secrets", []string{"host"}, true, true, []string{"host"}},
		{"explicit all", []string{"totp", "ntfy", "host"}, true, true, []string{"totp", "ntfy", "host"}},
	} {
		t.Run(test.name, func(t *testing.T) {
			cfg := authConfig(t)
			cfg.ApprovalMethods = test.methods
			if !test.totp {
				cfg.TOTPSecret = ""
			}
			if test.ntfy {
				cfg.NtfyURL, cfg.NtfyTopic, cfg.NtfyToken = "https://ntfy.example.invalid", "approvals", "synthetic-token"
			}
			// Helpers must work without Validate for callers constructing configs directly.
			if got := cfg.effectiveApprovalMethods(); !reflect.DeepEqual(got, test.want) {
				t.Fatalf("methods = %v, want %v", got, test.want)
			}
			for _, method := range []string{"totp", "ntfy", "host", "unknown"} {
				want := false
				for _, enabled := range test.want {
					want = want || method == enabled
				}
				if cfg.methodEnabled(method) != want {
					t.Fatalf("methodEnabled(%q) did not follow the configured list", method)
				}
			}
			if err := cfg.Validate(); err != nil {
				t.Fatal(err)
			}
			if test.methods == nil && cfg.ApprovalMethods != nil {
				t.Fatal("validation changed the legacy method representation")
			}
		})
	}
}

func TestApprovalMethodsValidation(t *testing.T) {
	for _, test := range []struct {
		name    string
		methods []string
		secret  bool
		url     string
		topic   string
		token   string
		valid   bool
	}{
		{name: "host only", methods: []string{"host"}, valid: true},
		{name: "explicit empty", methods: []string{}, secret: true},
		{name: "legacy empty"},
		{name: "unknown", methods: []string{"email"}, secret: true},
		{name: "empty name", methods: []string{"host", ""}},
		{name: "duplicate", methods: []string{"host", "host"}},
		{name: "totp missing secret", methods: []string{"totp"}},
		{name: "ntfy complete", methods: []string{"ntfy"}, url: "https://ntfy.example.invalid", topic: "approvals", token: "synthetic", valid: true},
		{name: "ntfy missing url", methods: []string{"ntfy"}, topic: "approvals", token: "synthetic"},
		{name: "ntfy missing topic", methods: []string{"ntfy"}, url: "https://ntfy.example.invalid", token: "synthetic"},
		{name: "ntfy missing token", methods: []string{"ntfy"}, url: "https://ntfy.example.invalid", topic: "approvals"},
		{name: "legacy partial ntfy", secret: true, url: "https://ntfy.example.invalid"},
		{name: "ntfy insecure url", methods: []string{"ntfy"}, url: "http://ntfy.example.invalid", topic: "approvals", token: "synthetic"},
		{name: "ntfy multiline token", methods: []string{"ntfy"}, url: "https://ntfy.example.invalid", topic: "approvals", token: "synthetic\nextra"},
	} {
		t.Run(test.name, func(t *testing.T) {
			cfg := authConfig(t)
			cfg.ApprovalMethods = test.methods
			if !test.secret {
				cfg.TOTPSecret = ""
			}
			cfg.NtfyURL, cfg.NtfyTopic, cfg.NtfyToken = test.url, test.topic, test.token
			if err := cfg.Validate(); (err == nil) != test.valid {
				t.Fatalf("Validate error = %v, valid = %v", err, test.valid)
			}
		})
	}
}

func TestApprovalNotifiersAlwaysEncodeAnArray(t *testing.T) {
	for _, methods := range [][]string{nil, {"totp"}, {"ntfy", "totp", "host"}, {"host"}} {
		cfg := Config{ApprovalMethods: methods}
		notifiers := cfg.approvalNotifiers()
		if notifiers == nil {
			t.Fatal("approvalNotifiers returned nil")
		}
		encoded, err := json.Marshal(notifiers)
		if err != nil || string(encoded) == "null" {
			t.Fatalf("notifier JSON = %s, error = %v", encoded, err)
		}
		for _, notifier := range notifiers {
			if notifier != "ntfy" && notifier != "host" {
				t.Fatalf("unexpected notifier %q", notifier)
			}
		}
	}
	cfg := Config{ApprovalMethods: []string{"ntfy", "totp", "host"}}
	if got := cfg.approvalNotifiers(); !reflect.DeepEqual(got, []string{"ntfy", "host"}) {
		t.Fatalf("notifiers = %v", got)
	}
}

func TestLoadApprovalMethodsAbsentVersusEmpty(t *testing.T) {
	cfg := authConfig(t)
	data, err := json.Marshal(cfg)
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		t.Fatal(err)
	}
	delete(fields, "approvalMethods")
	path := filepath.Join(t.TempDir(), "config.json")
	for _, empty := range []bool{false, true} {
		if empty {
			fields["approvalMethods"] = json.RawMessage(`[]`)
		}
		data, err = json.Marshal(fields)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, data, 0600); err != nil {
			t.Fatal(err)
		}
		loaded, err := LoadConfig(path)
		if empty {
			if err == nil {
				t.Fatal("explicit empty method list loaded")
			}
		} else if err != nil || loaded.ApprovalMethods != nil || !loaded.methodEnabled("totp") {
			t.Fatalf("legacy config did not load: %v", err)
		}
	}
}
