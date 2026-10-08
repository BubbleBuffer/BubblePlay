package main

import (
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestTrustedProxyAndOriginBoundaries(t *testing.T) {
	cfg := Config{PublicURL: "https://relay.example.invalid", TrustedProxyCIDRs: []string{"127.0.0.0/8", "10.0.0.0/8"}}
	for _, test := range []struct{ peer, forwarded, want string }{
		{"192.0.2.1:123", "203.0.113.1", "192.0.2.1"},
		{"127.0.0.1:123", "203.0.113.1, 10.0.0.1", "203.0.113.1"},
		{"127.0.0.1:123", "203.0.113.1, 192.0.2.1", "192.0.2.1"},
		{"127.0.0.1:123", "broken, 10.0.0.1", "127.0.0.1"},
	} {
		r := httptest.NewRequest("GET", cfg.PublicURL, nil)
		r.RemoteAddr = test.peer
		r.Header.Set("X-Forwarded-For", test.forwarded)
		if got := ClientIP(r, cfg); got != test.want {
			t.Fatalf("client identity got %q, want %q", got, test.want)
		}
	}
	r := httptest.NewRequest("GET", cfg.PublicURL, nil)
	r.Header.Set("Origin", "https://foreign.invalid")
	if SameOrigin(r, cfg, true) {
		t.Fatal("foreign origin accepted")
	}
	r.Header.Del("Origin")
	if SameOrigin(r, cfg, false) || !SameOrigin(r, cfg, true) {
		t.Fatal("native absent-Origin policy changed")
	}
}

func TestSetupWithNtfyAndUnicodePassword(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "private")
	inputs := t.TempDir()
	password := strings.Repeat("é", 256)
	passwordPath := filepath.Join(inputs, "password.txt")
	tokenPath := filepath.Join(inputs, "ntfy.txt")
	for name, value := range map[string]string{passwordPath: password, tokenPath: "synthetic-ntfy-publishing-token\n"} {
		if err := os.WriteFile(name, []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
	}
	args := []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--assets-dir", t.TempDir(), "--password-file", passwordPath,
		"--methods", "totp,ntfy", "--ntfy-url", "https://ntfy.example.invalid", "--ntfy-topic", "synthetic-approvals", "--ntfy-token-file", tokenPath}
	if err := setupCommand("init", args); err != nil {
		t.Fatal(err)
	}
	cfg, err := LoadConfig(filepath.Join(dir, "config.json"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.NtfyURL != "https://ntfy.example.invalid" || cfg.NtfyTopic != "synthetic-approvals" || cfg.NtfyToken != "synthetic-ntfy-publishing-token" || cfg.TOTPSecret == "" {
		t.Fatal("init did not retain both approval methods")
	}
	hash, err := parsePasswordHash(cfg.PasswordHash)
	if err != nil || !hash.verify(password) {
		t.Fatal("Unicode password character boundary changed")
	}
	badDir := filepath.Join(t.TempDir(), "incomplete")
	if err := setupCommand("init", []string{"--state-dir", badDir, "--public-url", "https://relay.example.invalid", "--ntfy-url", "https://ntfy.example.invalid", "--password-file", passwordPath}); err == nil {
		t.Fatal("partial ntfy configuration accepted")
	}
	if _, err := os.Stat(filepath.Join(badDir, "config.json")); !os.IsNotExist(err) {
		t.Fatal("failed init wrote configuration")
	}
}

func TestSetupAndRotationKeepHostCredentialsPrivate(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "private")
	passwordPath := filepath.Join(t.TempDir(), "password.txt")
	if err := os.WriteFile(passwordPath, []byte(fixturePassword), 0600); err != nil {
		t.Fatal(err)
	}
	args := []string{"--state-dir", dir, "--public-url", "https://relay.example.invalid", "--assets-dir", t.TempDir(), "--password-file", passwordPath}
	if err := setupCommand("init", args); err != nil {
		t.Fatal(err)
	}
	cfg, err := LoadConfig(filepath.Join(dir, "config.json"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Token == cfg.HostGateToken || cfg.Token == cfg.AdminToken || cfg.HostGateToken == cfg.AdminToken || len(cfg.Token) != 43 || len(cfg.TOTPSecret) != 32 {
		t.Fatal("setup did not generate independent credentials")
	}
	if err := setupCommand("init", args); err == nil {
		t.Fatal("init replaced existing credentials")
	}
	if err := setupCommand("rotate", []string{"--state-dir", dir, "--totp"}); err != nil {
		t.Fatal(err)
	}
	rotated, err := LoadConfig(filepath.Join(dir, "config.json"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Token != rotated.Token || cfg.HostGateToken != rotated.HostGateToken || cfg.AdminToken != rotated.AdminToken || cfg.PasswordHash != rotated.PasswordHash || cfg.TOTPSecret == rotated.TOTPSecret {
		t.Fatal("TOTP rotation altered unrelated credentials or kept old seed")
	}
	for _, name := range []string{"config.json", "provision.txt"} {
		info, err := os.Stat(filepath.Join(dir, name))
		if err != nil {
			t.Fatal(err)
		}
		if runtime.GOOS != "windows" && info.Mode().Perm() != 0600 {
			t.Fatal("private provisioning file permissions")
		}
	}
}
