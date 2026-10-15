package main

import (
	"crypto/rand"
	"encoding/base32"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// pcFixture returns a registrable PC whose canonical ID is derived from a fresh
// 32-byte public key, so the record validates end to end.
func pcFixture(t *testing.T) (string, pcRecord) {
	t.Helper()
	public := make([]byte, 32)
	if _, err := rand.Read(public); err != nil {
		t.Fatal(err)
	}
	id := deviceID(public)
	if canonical, ok := canonicalDeviceID(id); !ok || canonical != id {
		t.Fatalf("derived ID %q is not canonical", id)
	}
	return id, pcRecord{Public: base64.RawURLEncoding.EncodeToString(public), FirstSeen: 1700000000000, LastSeen: 1700000000000}
}

// pcPersist stores a PC the way registration does: under auth.mu, then through
// the same atomic persist used for every other auth state change.
func pcPersist(t *testing.T, a *Auth, id string, record pcRecord) {
	t.Helper()
	a.mu.Lock()
	defer a.mu.Unlock()
	a.state.PCs[id] = record
	if err := a.persistLocked(); err != nil {
		t.Fatal(err)
	}
}

// pcWriteAuthJSON replaces only the "pcs" member of the persisted state. Every
// other field stays valid, so a rejection can only come from PC validation.
// An empty pcs argument removes the member, like a version 1 file from before PCs.
func pcWriteAuthJSON(t *testing.T, cfg Config, pcs string) {
	t.Helper()
	path := filepath.Join(cfg.StateDir, "auth.json")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		t.Fatal(err)
	}
	if pcs == "" {
		delete(fields, "pcs")
	} else {
		fields["pcs"] = json.RawMessage(pcs)
	}
	out, err := json.Marshal(fields)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, out, 0600); err != nil {
		t.Fatal(err)
	}
}

func pcJSON(t *testing.T, entries map[string]pcRecord) string {
	t.Helper()
	data, err := json.Marshal(entries)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

func TestAuthPCsRoundtripAcrossRestart(t *testing.T) {
	a, cfg := newTestAuth(t)
	id, record := pcFixture(t)
	pcPersist(t, a, id, record)

	restarted, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := restarted.state.PCs[id]; !ok || got != record || len(restarted.state.PCs) != 1 {
		t.Fatalf("PC not durable after restart: got %+v ok=%v count=%d", got, ok, len(restarted.state.PCs))
	}
	data, err := os.ReadFile(filepath.Join(cfg.StateDir, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	// Pin the wire names: the admin list and the host registration depend on them.
	for _, want := range []string{`"pcs":{"` + id + `":{`, `"public":"` + record.Public + `"`, `"firstSeen":1700000000000`, `"lastSeen":1700000000000`} {
		if !strings.Contains(string(data), want) {
			t.Fatalf("persisted state lacks %s", want)
		}
	}
}

func TestAuthPCsSurvivePasswordAndFactorChanges(t *testing.T) {
	a, cfg := newTestAuth(t)
	id, record := pcFixture(t)
	pcPersist(t, a, id, record)
	cookies := loginCookies(t, a)
	gate := withCookies(authRequest("GET", "/", ""), cookies)
	if !a.AuthorizedBrowser(gate) {
		t.Fatal("setup session rejected")
	}

	rotated, err := HashPassword("Rotated synthetic password 789")
	if err != nil {
		t.Fatal(err)
	}
	totpSeed := base32.StdEncoding.WithPadding(base32.NoPadding).EncodeToString([]byte("abcdefghijklmnopqrst"))
	steps := []struct {
		name   string
		change func(*Config)
	}{
		{"password rotation", func(c *Config) { c.PasswordHash = rotated }},
		{"admin token rotation", func(c *Config) { c.AdminToken = strings.Repeat("b", 32) }},
		{"host gate token rotation", func(c *Config) { c.HostGateToken = strings.Repeat("c", 32) }},
		{"TOTP seed change", func(c *Config) { c.TOTPSecret = totpSeed }},
	}
	for _, step := range steps {
		step.change(&cfg)
		restarted, err := NewAuth(cfg)
		if err != nil {
			t.Fatalf("%s: %v", step.name, err)
		}
		if got, ok := restarted.state.PCs[id]; !ok || got != record || len(restarted.state.PCs) != 1 {
			t.Fatalf("%s dropped the PC: got %+v ok=%v", step.name, got, ok)
		}
		// The change must really have invalidated the session, or the PC check proves nothing about a changed config.
		if len(restarted.state.Sessions) != 0 {
			t.Fatalf("%s retained browser sessions", step.name)
		}
	}
}

func TestAuthPCsLegacyStateWithoutPCsLoads(t *testing.T) {
	_, cfg := newTestAuth(t)
	pcWriteAuthJSON(t, cfg, "")
	restarted, err := NewAuth(cfg)
	if err != nil {
		t.Fatalf("version 1 state without pcs rejected: %v", err)
	}
	if restarted.state.PCs == nil || len(restarted.state.PCs) != 0 {
		t.Fatal("missing pcs table did not load as an empty table")
	}
	data, err := os.ReadFile(filepath.Join(cfg.StateDir, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), `"pcs":{}`) {
		t.Fatal("empty PC table not persisted as an object")
	}
}

func TestAuthPCsMalformedDurableDataFailsClosed(t *testing.T) {
	id, record := pcFixture(t)
	// A valid ID for a different key: the mismatch case fails only on key equality.
	_, other := pcFixture(t)
	otherPublic, err := base64.RawURLEncoding.DecodeString(other.Public)
	if err != nil {
		t.Fatal(err)
	}
	otherID := deviceID(otherPublic)

	alphabet := "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
	// Flip the low bit of the final character: it still decodes to the same bytes,
	// but the encoding is no longer canonical.
	nonCanonical := []byte(record.Public)
	nonCanonical[len(nonCanonical)-1] = alphabet[strings.IndexByte(alphabet, nonCanonical[len(nonCanonical)-1])^1]
	lower := strings.ToLower(id)
	if lower == id {
		lower = strings.ToUpper(id)
	}
	short := make([]byte, 31)
	long := make([]byte, 33)
	for _, b := range [][]byte{short, long} {
		if _, err := rand.Read(b); err != nil {
			t.Fatal(err)
		}
	}
	shortID, longID := deviceID(short), deviceID(long)

	cases := []struct {
		name string
		pcs  string
		ok   bool
	}{
		{"valid control", pcJSON(t, map[string]pcRecord{id: record}), true},
		{"padded public key", pcJSON(t, map[string]pcRecord{id: {Public: record.Public + "=", FirstSeen: record.FirstSeen, LastSeen: record.LastSeen}}), false},
		{"standard alphabet public key", pcJSON(t, map[string]pcRecord{id: {Public: "+" + record.Public[1:], FirstSeen: record.FirstSeen, LastSeen: record.LastSeen}}), false},
		{"non-canonical trailing bits", pcJSON(t, map[string]pcRecord{id: {Public: string(nonCanonical), FirstSeen: record.FirstSeen, LastSeen: record.LastSeen}}), false},
		{"31-byte public key", pcJSON(t, map[string]pcRecord{shortID: {Public: base64.RawURLEncoding.EncodeToString(short), FirstSeen: 1, LastSeen: 1}}), false},
		{"33-byte public key", pcJSON(t, map[string]pcRecord{longID: {Public: base64.RawURLEncoding.EncodeToString(long), FirstSeen: 1, LastSeen: 1}}), false},
		{"ID of another key", pcJSON(t, map[string]pcRecord{otherID: record}), false},
		{"non-canonical ID case", pcJSON(t, map[string]pcRecord{lower: record}), false},
		{"zero firstSeen", pcJSON(t, map[string]pcRecord{id: {Public: record.Public, FirstSeen: 0, LastSeen: 1}}), false},
		{"lastSeen before firstSeen", pcJSON(t, map[string]pcRecord{id: {Public: record.Public, FirstSeen: 2, LastSeen: 1}}), false},
		{"unknown record field", fmt.Sprintf(`{%q:{"public":%q,"firstSeen":1,"lastSeen":1,"extra":true}}`, id, record.Public), false},
		{"null record", fmt.Sprintf(`{%q:null}`, id), false},
		{"non-object table", `[]`, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := authConfig(t)
			if _, err := NewAuth(cfg); err != nil {
				t.Fatal(err)
			}
			pcWriteAuthJSON(t, cfg, tc.pcs)
			_, err := NewAuth(cfg)
			if tc.ok && err != nil {
				t.Fatalf("valid PC rejected: %v", err)
			}
			if !tc.ok && err == nil {
				t.Fatal("malformed PC accepted")
			}
		})
	}
}

func TestAuthPCsLimitedToMaxAuthRecords(t *testing.T) {
	pcs := make(map[string]pcRecord, maxAuthRecords+1)
	for i := 0; i <= maxAuthRecords; i++ {
		public := make([]byte, 32)
		binary.BigEndian.PutUint64(public, uint64(i))
		pcs[deviceID(public)] = pcRecord{Public: base64.RawURLEncoding.EncodeToString(public), FirstSeen: 1, LastSeen: 1}
		if i == maxAuthRecords-1 {
			if !validDurablePCs(pcs) {
				t.Fatalf("%d valid PCs rejected", len(pcs))
			}
		}
	}
	if len(pcs) != maxAuthRecords+1 || validDurablePCs(pcs) {
		t.Fatalf("%d PCs accepted", len(pcs))
	}
}
