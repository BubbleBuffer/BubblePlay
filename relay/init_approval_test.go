package main

import (
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func approvalSetupInputs(t *testing.T) (string, string) {
	t.Helper()
	dir := t.TempDir()
	password, token := filepath.Join(dir, "password.txt"), filepath.Join(dir, "token.txt")
	for path, value := range map[string]string{password: syntheticPassword, token: "synthetic-ntfy-token"} {
		if err := os.WriteFile(path, []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
	}
	return password, token
}

func loadApprovalSetup(t *testing.T, dir string) Config {
	t.Helper()
	cfg, err := LoadConfig(filepath.Join(dir, "config.json"))
	if err != nil {
		t.Fatal(err)
	}
	return cfg
}

func TestInitApprovalMethodsAndProvisioning(t *testing.T) {
	password, token := approvalSetupInputs(t)
	for _, test := range []struct {
		name    string
		methods string
		want    []string
	}{
		{"default", "", []string{"totp"}},
		{"totp and host", "totp,host", []string{"totp", "host"}},
		{"host only", "host", []string{"host"}},
		{"ntfy only", "ntfy", []string{"ntfy"}},
		{"notifiers", "ntfy,host", []string{"ntfy", "host"}},
	} {
		t.Run(test.name, func(t *testing.T) {
			dir := filepath.Join(t.TempDir(), "private")
			args := []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--password-file", password}
			if test.methods != "" {
				args = append(args, "--methods", test.methods)
			}
			if strings.Contains(test.methods, "ntfy") {
				args = append(args, "--ntfy-url", "https://ntfy.example.invalid", "--ntfy-topic", "approvals", "--ntfy-token-file", token)
			}
			if err := setupCommand("init", args); err != nil {
				t.Fatal(err)
			}
			cfg := loadApprovalSetup(t, dir)
			if !reflect.DeepEqual(cfg.ApprovalMethods, test.want) {
				t.Fatalf("methods = %v, want %v", cfg.ApprovalMethods, test.want)
			}
			if (cfg.TOTPSecret != "") != cfg.methodEnabled("totp") {
				t.Fatal("authenticator secret did not follow enabled method")
			}
			data, err := os.ReadFile(filepath.Join(dir, "provision.txt"))
			if err != nil {
				t.Fatal(err)
			}
			text := string(data)
			if !strings.Contains(text, "Approval methods: "+strings.Join(test.want, ",")) {
				t.Fatal("provisioning did not list offered methods")
			}
			if strings.Contains(text, "otpauth://") != cfg.methodEnabled("totp") || strings.Contains(text, "Authenticator secret:") != cfg.methodEnabled("totp") {
				t.Fatal("provisioning included enrollment for a disabled method or omitted enabled enrollment")
			}
		})
	}
}

func TestInitRejectsInvalidApprovalAndProxyFlagsWithoutSaving(t *testing.T) {
	password, token := approvalSetupInputs(t)
	for _, args := range [][]string{
		{"--methods", ""}, {"--methods", "unknown"}, {"--methods", "host,host"}, {"--methods", "host,"},
		{"--methods", "ntfy"}, {"--methods", "ntfy", "--ntfy-url", "https://ntfy.example.invalid"},
		{"--methods", "ntfy", "--ntfy-topic", "approvals", "--ntfy-token-file", token},
		{"--methods", "host", "--ntfy-url", "https://ntfy.example.invalid", "--ntfy-topic", "approvals", "--ntfy-token-file", token},
		{"--ntfy-url", ""},
		{"--trusted-proxy", "not-a-cidr"},
	} {
		t.Run(strings.Join(args, " "), func(t *testing.T) {
			dir := filepath.Join(t.TempDir(), "private")
			base := []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--password-file", password}
			if err := setupCommand("init", append(base, args...)); err == nil {
				t.Fatal("invalid setup succeeded")
			}
			if _, err := os.Stat(filepath.Join(dir, "config.json")); !os.IsNotExist(err) {
				t.Fatal("invalid setup saved a config")
			}
		})
	}
}

func TestInitRepeatableTrustedProxies(t *testing.T) {
	password, _ := approvalSetupInputs(t)
	dir := filepath.Join(t.TempDir(), "private")
	args := []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--methods", "host", "--password-file", password,
		"--trusted-proxy", "127.0.0.0/8", "--trusted-proxy", "10.0.0.0/8", "--trusted-proxy", "::1/128"}
	if err := setupCommand("init", args); err != nil {
		t.Fatal(err)
	}
	cfg := loadApprovalSetup(t, dir)
	if !reflect.DeepEqual(cfg.TrustedProxyCIDRs, []string{"127.0.0.0/8", "10.0.0.0/8", "::1/128"}) {
		t.Fatalf("trusted proxies = %v", cfg.TrustedProxyCIDRs)
	}
	r := httptest.NewRequest("GET", cfg.PublicURL, nil)
	r.RemoteAddr = "127.0.0.1:1234"
	r.Header.Set("X-Forwarded-For", "203.0.113.7, 10.0.0.1")
	if got := ClientIP(r, cfg); got != "203.0.113.7" {
		t.Fatalf("provisioned proxy chain client = %q", got)
	}
	if err := setupCommand("rotate", []string{"--state-dir", dir, "--methods", "host", "--trusted-proxy", "192.0.2.0/24"}); err == nil {
		t.Fatal("rotate silently accepted init-only proxy flags")
	}
}

func TestRotateApprovalMethodsAndCompatibleCredentialFlags(t *testing.T) {
	password, token := approvalSetupInputs(t)
	dir := filepath.Join(t.TempDir(), "private")
	if err := setupCommand("init", []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--methods", "host", "--password-file", password}); err != nil {
		t.Fatal(err)
	}
	original := loadApprovalSetup(t, dir)
	initialFiles := make(map[string][]byte)
	for _, name := range []string{"config.json", "provision.txt"} {
		data, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil {
			t.Fatal(err)
		}
		initialFiles[name] = data
	}
	if err := setupCommand("rotate", []string{"--state-dir", dir, "--methods", "host,ntfy"}); err == nil {
		t.Fatal("enabling ntfy without provisioning settings succeeded")
	}
	for name, before := range initialFiles {
		after, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil || string(before) != string(after) {
			t.Fatalf("failed ntfy setup changed saved %s", name)
		}
	}
	rotate := func(args ...string) Config {
		t.Helper()
		if err := setupCommand("rotate", append([]string{"--state-dir", dir}, args...)); err != nil {
			t.Fatal(err)
		}
		return loadApprovalSetup(t, dir)
	}
	totp := rotate("--methods", "totp,host")
	if len(totp.TOTPSecret) != 32 {
		t.Fatal("enabling totp did not generate a secret")
	}
	unchanged := rotate("--methods", "totp,host")
	if unchanged.TOTPSecret != totp.TOTPSecret {
		t.Fatal("selecting existing totp replaced its secret without --totp")
	}
	host := rotate("--methods", "host")
	if host.TOTPSecret != "" {
		t.Fatal("disabling totp retained its secret")
	}
	if err := setupCommand("rotate", []string{"--state-dir", dir, "--totp"}); err == nil {
		t.Fatal("--totp silently enabled a disabled approval method")
	}
	legacyFlag := rotate("--methods", "totp,host", "--totp")
	if !legacyFlag.methodEnabled("totp") || len(legacyFlag.TOTPSecret) != 32 || legacyFlag.TOTPSecret == totp.TOTPSecret {
		t.Fatal("--totp with an enabled method did not generate a fresh authenticator secret")
	}
	notifiers := rotate("--methods", "host,ntfy", "--ntfy-url", "https://ntfy.example.invalid", "--ntfy-topic", "approvals", "--ntfy-token-file", token)
	if notifiers.TOTPSecret != "" || notifiers.NtfyToken != "synthetic-ntfy-token" || !reflect.DeepEqual(notifiers.approvalNotifiers(), []string{"host", "ntfy"}) {
		t.Fatal("rotation did not add ntfy and disable totp")
	}
	newPassword := "Changed synthetic password 456"
	if err := os.WriteFile(password, []byte(newPassword), 0600); err != nil {
		t.Fatal(err)
	}
	final := rotate("--password", "--password-file", password)
	if !reflect.DeepEqual(final.ApprovalMethods, notifiers.ApprovalMethods) || final.NtfyToken != notifiers.NtfyToken || final.TOTPSecret != "" {
		t.Fatal("password rotation changed approval settings")
	}
	hash, err := parsePasswordHash(final.PasswordHash)
	if err != nil || !hash.verify(newPassword) || final.PasswordHash == original.PasswordHash {
		t.Fatal("password rotation did not update password")
	}
	if final.Token != original.Token || final.HostGateToken != original.HostGateToken || final.AdminToken != original.AdminToken {
		t.Fatal("rotation changed host credentials")
	}
	before, err := os.ReadFile(filepath.Join(dir, "config.json"))
	if err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{{}, {"--methods", ""}, {"--methods", "host", "--totp"}, {"--methods", "host", "--ntfy-topic", "approvals"}} {
		if err := setupCommand("rotate", append([]string{"--state-dir", dir}, args...)); err == nil {
			t.Fatalf("invalid rotation accepted: %v", args)
		}
		after, err := os.ReadFile(filepath.Join(dir, "config.json"))
		if err != nil || string(after) != string(before) {
			t.Fatal("failed rotation changed saved config")
		}
	}
}
