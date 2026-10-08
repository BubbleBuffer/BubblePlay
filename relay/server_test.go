package main

import (
	"bufio"
	"bytes"
	"crypto/tls"
	"encoding/base32"
	"encoding/hex"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

type heldHijacker struct {
	http.ResponseWriter
	entered chan<- struct{}
	release <-chan struct{}
}

func (w heldHijacker) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	close(w.entered)
	<-w.release
	return w.ResponseWriter.(http.Hijacker).Hijack()
}

func TestRelayShutdownDuringUpgrade(t *testing.T) {
	f := newRelayFixture(t)
	entered, release := make(chan struct{}), make(chan struct{})
	server := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Preserve the configured canonical Host while using this isolated test listener.
		r.Host = strings.TrimPrefix(f.cfg.PublicURL, "https://")
		f.relay.ServeHTTP(heldHijacker{w, entered, release}, r)
	}))
	defer server.Close()
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		c, _, err := f.dialer.Dial("wss"+strings.TrimPrefix(server.URL, "https")+"/host-ws", http.Header{"Authorization": {"Bearer " + f.cfg.HostGateToken}})
		if err == nil {
			defer c.Close()
			_ = c.SetReadDeadline(time.Now().Add(time.Second))
			_, _, _ = c.ReadMessage()
		}
	}()
	select {
	case <-entered:
	case <-time.After(3 * time.Second):
		close(release)
		t.Fatal("upgrade did not enter owned hijacker")
	}
	f.relay.mu.Lock()
	pending, attached := f.relay.pendingUpgrades, len(f.relay.all)
	f.relay.mu.Unlock()
	if pending != 1 || attached != 0 {
		close(release)
		t.Fatalf("pending admission = %d, attached = %d", pending, attached)
	}
	f.relay.Close()
	close(release)
	select {
	case <-finished:
	case <-time.After(3 * time.Second):
		t.Fatal("shutdown left upgrade alive")
	}
	waitRelay(t, func() bool {
		f.relay.mu.Lock()
		defer f.relay.mu.Unlock()
		return f.relay.pendingUpgrades == 0 && len(f.relay.all) == 0
	})
}

var integrationHash string
var integrationHashOnce sync.Once

const fixturePassword = "synthetic-test-password-only"

type relayFixture struct {
	server *httptest.Server
	relay  *Relay
	cfg    Config
	cookie string
	dialer websocket.Dialer
}

func newRelayFixture(t *testing.T) *relayFixture {
	t.Helper()
	integrationHashOnce.Do(func() {
		var err error
		integrationHash, err = HashPassword(fixturePassword)
		if err != nil {
			panic(err)
		}
	})
	assets := t.TempDir()
	if err := os.WriteFile(filepath.Join(assets, "index.html"), []byte("synthetic browser asset"), 0600); err != nil {
		t.Fatal(err)
	}
	var app *Relay
	ts := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { app.ServeHTTP(w, r) }))
	cfg := Config{PublicURL: ts.URL, AssetsDir: assets, StateDir: t.TempDir(), Token: strings.Repeat("t", 40), HostGateToken: strings.Repeat("h", 40), AdminToken: strings.Repeat("a", 40), PasswordHash: integrationHash, TOTPSecret: base32.StdEncoding.WithPadding(base32.NoPadding).EncodeToString([]byte("12345678901234567890")), MaxConnections: 32}
	var err error
	app, err = NewRelay(cfg)
	if err != nil {
		ts.Close()
		t.Fatal(err)
	}
	cfg = app.cfg
	f := &relayFixture{server: ts, relay: app, cfg: cfg, dialer: websocket.Dialer{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}, HandshakeTimeout: 3 * time.Second}}
	t.Cleanup(func() { app.Close(); ts.Close() })
	secret, _ := base32.StdEncoding.WithPadding(base32.NoPadding).DecodeString(cfg.TOTPSecret)
	body, _ := json.Marshal(map[string]any{"password": fixturePassword, "totp": totpCode(secret, time.Now().Unix()/30, 6), "remember": true})
	request, _ := http.NewRequest("POST", ts.URL+"/__auth/login", bytes.NewReader(body))
	request.Header.Set("Origin", cfg.PublicURL)
	request.Header.Set("Content-Type", "application/json")
	response, err := ts.Client().Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != 204 {
		t.Fatalf("fixture login status %d", response.StatusCode)
	}
	var cookies []string
	for _, cookie := range response.Cookies() {
		cookies = append(cookies, cookie.Name+"="+cookie.Value)
	}
	f.cookie = strings.Join(cookies, "; ")
	return f
}
func (f *relayFixture) dial(t *testing.T, route, role string) *websocket.Conn {
	t.Helper()
	headers := http.Header{}
	if role == "host" || role == "version-probe" || strings.HasSuffix(role, "-host") {
		headers.Set("Authorization", "Bearer "+f.cfg.HostGateToken)
	} else {
		headers.Set("Cookie", f.cookie)
		headers.Set("Origin", f.cfg.PublicURL)
	}
	conn, response, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+route, headers)
	if err != nil {
		if response != nil {
			t.Fatalf("socket %s rejected %d", route, response.StatusCode)
		}
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	hello := map[string]any{"type": "hello", "role": role}
	if role == "host" || role == "version-probe" {
		hello["token"] = f.cfg.Token
	}
	if err := conn.WriteJSON(hello); err != nil {
		t.Fatal(err)
	}
	return conn
}
func readJSON(t *testing.T, c *websocket.Conn) map[string]any {
	t.Helper()
	_ = c.SetReadDeadline(time.Now().Add(7 * time.Second))
	var message map[string]any
	if err := c.ReadJSON(&message); err != nil {
		t.Fatal(err)
	}
	return message
}
func helloEpoch(t *testing.T, c *websocket.Conn) string {
	m := readJSON(t, c)
	if m["type"] != "hello-ok" {
		t.Fatal("missing hello")
	}
	epoch, _ := m["mediaEpoch"].(string)
	return epoch
}
func waitRelay(t *testing.T, predicate func() bool) {
	t.Helper()
	until := time.Now().Add(3 * time.Second)
	for time.Now().Before(until) {
		if predicate() {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("relay fixture timed out")
}
func packetFixture(t *testing.T) struct {
	Epoch    string         `json:"epoch"`
	MediaHex string         `json:"mediaHex"`
	Ack      map[string]any `json:"ack"`
	Keyframe map[string]any `json:"keyframe"`
	Files    []string       `json:"files"`
	Signals  []struct {
		Role    string                     `json:"role"`
		Message map[string]json.RawMessage `json:"message"`
	} `json:"signals"`
} {
	t.Helper()
	var f struct {
		Epoch    string         `json:"epoch"`
		MediaHex string         `json:"mediaHex"`
		Ack      map[string]any `json:"ack"`
		Keyframe map[string]any `json:"keyframe"`
		Files    []string       `json:"files"`
		Signals  []struct {
			Role    string                     `json:"role"`
			Message map[string]json.RawMessage `json:"message"`
		} `json:"signals"`
	}
	data, err := os.ReadFile("testdata/worker-contract.json")
	if err != nil {
		t.Fatal(err)
	}
	if err = json.Unmarshal(data, &f); err != nil {
		t.Fatal(err)
	}
	return f
}
func TestRelayAdmissionAndSignaling(t *testing.T) {
	f := newRelayFixture(t)
	for _, headers := range []http.Header{{}, {"Cookie": {f.cookie}, "Origin": {"https://foreign.example"}}, {"Authorization": {"Bearer wrong"}}} {
		_, resp, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+"/ws", headers)
		if err == nil || resp == nil || resp.StatusCode != 404 {
			t.Fatal("unauthorized socket accepted")
		}
	}
	host := f.dial(t, "/host-ws", "host")
	helloEpoch(t, host)
	client := f.dial(t, "/ws", "client")
	epoch := helloEpoch(t, client)
	if !epochPattern.MatchString(epoch) {
		t.Fatal("invalid epoch")
	}
	offer := map[string]any{"type": "offer", "sdp": "synthetic-sdp"}
	if err := client.WriteJSON(offer); err != nil {
		t.Fatal(err)
	}
	if readJSON(t, host)["sdp"] != "synthetic-sdp" {
		t.Fatal("SDP changed")
	}
	input := []byte{129, 12}
	if err := host.WriteMessage(websocket.BinaryMessage, input); err != nil {
		t.Fatal(err)
	}
	kind, bytes, err := client.ReadMessage()
	if err != nil || kind != websocket.BinaryMessage || !stringEqualBytes(bytes, input) {
		t.Fatal("cursor bytes changed")
	}
	_ = client.WriteJSON(map[string]any{"type": "media-control", "action": "follow", "protocol": 4, "epoch": strings.Repeat("f", 32), "follow": true})
	_ = client.WriteJSON(map[string]any{"type": "media-control", "action": "follow", "protocol": 4, "epoch": epoch, "follow": false})
	if readJSON(t, host)["epoch"] != epoch {
		t.Fatal("stale epoch forwarded")
	}
	request, _ := http.NewRequest("GET", f.server.URL+"/", nil)
	request.Header.Set("Cookie", f.cookie)
	response, err := f.server.Client().Do(request)
	if err != nil {
		t.Fatal(err)
	}
	content, _ := io.ReadAll(response.Body)
	response.Body.Close()
	if string(content) != "synthetic browser asset" {
		t.Fatal("authenticated assets unavailable")
	}
	response, err = f.server.Client().Get(f.server.URL + "/agent-ws")
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != 404 {
		t.Fatal("private agent route exposed")
	}
}
func stringEqualBytes(a, b []byte) bool { return bytes.Equal(a, b) }
func TestRelayMediaFileEpochAndDeparture(t *testing.T) {
	f := newRelayFixture(t)
	signal := f.dial(t, "/ws", "client")
	epoch := helloEpoch(t, signal)
	mediaClient := f.dial(t, "/media-ws?epoch="+epoch, "media-client")
	helloEpoch(t, mediaClient)
	readJSON(t, mediaClient)
	mediaHost := f.dial(t, "/host-media-ws?epoch="+epoch, "media-host")
	helloEpoch(t, mediaHost)
	readJSON(t, mediaHost)
	readJSON(t, mediaClient)
	packets := packetFixture(t)
	frame, _ := hex.DecodeString(packets.MediaHex)
	_ = mediaHost.WriteMessage(websocket.BinaryMessage, frame)
	kind, got, err := mediaClient.ReadMessage()
	if err != nil || kind != websocket.BinaryMessage || !bytes.Equal(frame, got) {
		t.Fatal("media bytes changed")
	}
	_ = mediaClient.WriteJSON(packets.Ack)
	ack := readJSON(t, mediaHost)
	if ack["recvTsMs"] != 12.3 {
		t.Fatal("fractional browser ACK dropped")
	}
	fileClient := f.dial(t, "/file-ws?epoch="+epoch, "file-client")
	helloEpoch(t, fileClient)
	readJSON(t, fileClient)
	fileHost := f.dial(t, "/host-file-ws?epoch="+epoch, "file-host")
	helloEpoch(t, fileHost)
	readJSON(t, fileHost)
	readJSON(t, fileClient)
	for _, value := range packets.Files[:3] {
		packet, _ := hex.DecodeString(value)
		_ = fileClient.WriteMessage(websocket.BinaryMessage, packet)
		kind, got, err := fileHost.ReadMessage()
		if err != nil || kind != websocket.BinaryMessage || !bytes.Equal(packet, got) {
			t.Fatal("file packet changed")
		}
	}
	fileHost.Close()
	_ = fileClient.SetReadDeadline(time.Now().Add(3 * time.Second))
	_, _, err = fileClient.ReadMessage()
	if !websocket.IsCloseError(err, websocket.CloseGoingAway) {
		t.Fatalf("ordinary file peer departure did not cancel counterpart: %v", err)
	}
	probe := f.dial(t, "/host-version-ws", "version-probe")
	if readJSON(t, probe)["type"] != "hello-ok" {
		t.Fatal("version hello was not flushed")
	}
	signal.Close()
	waitRelay(t, func() bool { f.relay.mu.Lock(); defer f.relay.mu.Unlock(); return f.relay.currentEpoch == "" })
	_, response, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+"/host-file-ws?epoch="+epoch, http.Header{"Authorization": {"Bearer " + f.cfg.HostGateToken}})
	if err == nil || response == nil || response.StatusCode != 404 {
		t.Fatal("retired epoch admitted")
	}
}
func TestRelayRevocationClosesIdleViewer(t *testing.T) {
	f := newRelayFixture(t)
	host := f.dial(t, "/host-ws", "host")
	helloEpoch(t, host)
	client := f.dial(t, "/ws", "client")
	helloEpoch(t, client)
	request, _ := http.NewRequest("DELETE", f.server.URL+"/__gate/manage/sessions", nil)
	request.Header.Set("Authorization", "Bearer "+f.cfg.AdminToken)
	response, err := f.server.Client().Do(request)
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != 204 {
		t.Fatalf("revoke status %d", response.StatusCode)
	}
	_ = client.SetReadDeadline(time.Now().Add(7 * time.Second))
	_, _, err = client.ReadMessage()
	if !websocket.IsCloseError(err, websocket.ClosePolicyViolation) {
		t.Fatalf("idle revoked viewer stayed active: %v", err)
	}
	if readJSON(t, host)["type"] != "peer-left" {
		t.Fatal("host not informed of revoked viewer")
	}
}
func TestBrowserGeneratedProtocolFixtures(t *testing.T) {
	f := packetFixture(t)
	for _, vector := range f.Signals {
		data, _ := json.Marshal(vector.Message)
		if !validSignalMessage(vector.Message, vector.Role, f.Epoch, len(data)) {
			t.Fatalf("real producer shape rejected: %s", str(vector.Message, "type"))
		}
	}
	for index, value := range f.Files {
		packet, _ := hex.DecodeString(value)
		if !validFilePacket(packet, "file-client") {
			t.Fatalf("browser packet%d rejected", index)
		}
		if index == 3 && validFilePacket(packet, "file-host") {
			t.Fatal("host request accepted")
		}
	}
	for _, input := range []string{`null`, `"0"`, `false`, `[]`, `{}`} {
		m := map[string]json.RawMessage{"value": json.RawMessage(input)}
		if integer(m, "value", 0, 100) || isBoolean(m, "value") && input != "false" || boundedString(m, "value", 0, 10) && input != `"0"` {
			t.Fatalf("wrong JSON type accepted %s", input)
		}
	}
	m := map[string]json.RawMessage{"value": json.RawMessage(`null`)}
	if integer(m, "value", 0, 100) || isBoolean(m, "value") || boundedString(m, "value", 0, 100) {
		t.Fatal("null treated as required zero value")
	}
}
