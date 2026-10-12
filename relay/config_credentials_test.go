package main

import (
	"strings"
	"testing"
)

func TestCredentialErrorNamesFailingConfigKey(t *testing.T) {
	base := authConfig(t)
	if err := base.Validate(); err != nil {
		t.Fatalf("base fixture rejected: %v", err)
	}
	for _, test := range []struct {
		key    string
		mutate func(*Config)
	}{
		{"token", func(c *Config) { c.Token = "" }},
		{"hostGateToken", func(c *Config) { c.HostGateToken = strings.Repeat("h", 31) }},
		{"adminToken", func(c *Config) { c.AdminToken = strings.Repeat("a", 31) + "\n" }},
		{"token", func(c *Config) { c.Token = strings.Repeat("t", 8193) }},
	} {
		cfg := base
		test.mutate(&cfg)
		err := cfg.Validate()
		if err == nil {
			t.Fatalf("%s: invalid credential accepted", test.key)
		}
		want := `relay config key "` + test.key + `" must be 32 to 8192 characters without line breaks`
		if err.Error() != want {
			t.Fatalf("error %q, want %q", err.Error(), want)
		}
	}
}
