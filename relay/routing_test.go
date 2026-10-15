package main

import (
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

// Black-box checks of the phase 1 routing contract: host registration by ID,
// per-PC rooms, tickets for data sockets, default target order and the admin
// PC list. Relay proofs are encoded here with dkcLengthPrefixed and sha256, not
// with the production helpers.

const routingOtherOrigin = "https://other-relay.example.test"

var routingBase = time.Date(2026, time.October, 15, 9, 0, 0, 0, time.UTC)

type routingPC struct {
	id     string
	pub    []byte
	public string
	priv   ed25519.PrivateKey
}

func routingNewPC(t *testing.T) routingPC {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return routingPC{id: dkcDeviceID(pub), pub: pub, public: dkcEnc(pub), priv: priv}
}

// routingSummary names the only fields that may appear in a failure message.
// It never includes the ticket or the whole hello-ok map.
func routingSummary(m map[string]any) string {
	var parts []string
	for _, key := range []string{"type", "pc", "mediaEpoch", "present"} {
		if v, ok := m[key]; ok {
			parts = append(parts, fmt.Sprintf("%s=%v", key, v))
		}
	}
	return "{" + strings.Join(parts, " ") + "}"
}

func routingURL(f *relayFixture, route string) string {
	return "wss" + strings.TrimPrefix(f.server.URL, "https") + route
}

func routingBearer(secret string) http.Header {
	return http.Header{"Authorization": {"Bearer " + secret}}
}

func routingViewerHeaders(f *relayFixture) http.Header {
	return http.Header{"Cookie": {f.cookie}, "Origin": {f.cfg.PublicURL}}
}

func routingOpen(t *testing.T, f *relayFixture, route string, headers http.Header) *websocket.Conn {
	t.Helper()
	c, resp, err := f.dialer.Dial(routingURL(f, route), headers)
	if err != nil {
		if resp != nil {
			t.Fatalf("socket %s rejected %d", route, resp.StatusCode)
		}
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

// routingRefusedUpgrade expects the HTTP upgrade itself to be refused with 404.
func routingRefusedUpgrade(t *testing.T, f *relayFixture, route string, headers http.Header) {
	t.Helper()
	c, resp, err := f.dialer.Dial(routingURL(f, route), headers)
	if c != nil {
		c.Close()
	}
	if err == nil || resp == nil || resp.StatusCode != http.StatusNotFound {
		t.Fatalf("upgrade admitted on %s", route)
	}
}

// routingReadUntilClose fails if the socket is admitted or closes with another code.
func routingReadUntilClose(t *testing.T, c *websocket.Conn, code int) {
	t.Helper()
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		_, data, err := c.ReadMessage()
		if err != nil {
			if !websocket.IsCloseError(err, code) {
				t.Fatalf("expected close %d, got %v", code, err)
			}
			return
		}
		var m map[string]any
		if json.Unmarshal(data, &m) == nil && m["type"] == "hello-ok" {
			t.Fatal("refused socket was admitted")
		}
	}
}

func routingSend(t *testing.T, c *websocket.Conn, value any) {
	t.Helper()
	if err := c.WriteJSON(value); err != nil {
		t.Fatal(err)
	}
}

func routingOpenHello(t *testing.T, f *relayFixture, route string, headers http.Header, hello map[string]any) (*websocket.Conn, map[string]any) {
	t.Helper()
	c := routingOpen(t, f, route, headers)
	routingSend(t, c, hello)
	return c, readJSON(t, c)
}

func routingExpectHelloRefused(t *testing.T, f *relayFixture, route string, headers http.Header, hello map[string]any) {
	t.Helper()
	c := routingOpen(t, f, route, headers)
	routingSend(t, c, hello)
	routingReadUntilClose(t, c, websocket.ClosePolicyViolation)
}

func routingDataHello(role, ticket string) map[string]any {
	return map[string]any{"type": "hello", "role": role, "ticket": ticket}
}

func routingHostHello(pc routingPC, token string) map[string]any {
	hello := map[string]any{"type": "hello", "role": "host", "id": pc.id, "public": pc.public}
	if token != "" {
		hello["token"] = token
	}
	return hello
}

func routingChallenge(t *testing.T, c *websocket.Conn) []byte {
	t.Helper()
	m := readJSON(t, c)
	nonce, _ := m["nonce"].(string)
	if m["type"] != "register-challenge" || nonce == "" {
		t.Fatalf("expected register-challenge, got %s", routingSummary(m))
	}
	return dkcB64(t, nonce)
}

// routingSign encodes the relay proof independently of the production helper.
func routingSign(origin string, nonce []byte, pc routingPC, priv ed25519.PrivateKey) string {
	encoded := dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte("host"), []byte(origin), nonce, []byte(pc.id), pc.pub)
	sum := sha256.Sum256(encoded)
	return dkcEnc(ed25519.Sign(priv, sum[:]))
}

// routingProveWith opens a host socket for pc, sends hello and answers the challenge with sign.
// The caller reads the outcome.
func routingProveWith(t *testing.T, f *relayFixture, pc routingPC, token string, sign func(nonce []byte) string) *websocket.Conn {
	t.Helper()
	c := routingOpen(t, f, "/host-ws?pc="+pc.id, routingBearer(f.cfg.HostGateToken))
	routingSend(t, c, routingHostHello(pc, token))
	nonce := routingChallenge(t, c)
	routingSend(t, c, map[string]any{"type": "register-proof", "signature": sign(nonce)})
	return c
}

// routingRegister completes a valid registration and returns the admitted host socket.
func routingRegister(t *testing.T, f *relayFixture, pc routingPC, token string) (*websocket.Conn, map[string]any) {
	t.Helper()
	c := routingProveWith(t, f, pc, token, func(nonce []byte) string {
		return routingSign(f.cfg.PublicURL, nonce, pc, pc.priv)
	})
	ok := readJSON(t, c)
	if ok["type"] != "hello-ok" || ok["pc"] != pc.id {
		t.Fatalf("registration not admitted: %s", routingSummary(ok))
	}
	return c, ok
}

func routingLegacyHost(t *testing.T, f *relayFixture) *websocket.Conn {
	t.Helper()
	c, ok := routingOpenHello(t, f, "/host-ws", routingBearer(f.cfg.HostGateToken), map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token})
	if ok["type"] != "hello-ok" {
		t.Fatalf("legacy host refused: %s", routingSummary(ok))
	}
	if _, present := ok["pc"]; present {
		t.Fatal("legacy host hello-ok names a pc")
	}
	return c
}

func routingViewer(t *testing.T, f *relayFixture, route string) (*websocket.Conn, map[string]any) {
	t.Helper()
	c, ok := routingOpenHello(t, f, route, routingViewerHeaders(f), map[string]any{"type": "hello", "role": "client"})
	if ok["type"] != "hello-ok" {
		t.Fatalf("viewer refused on %s: %s", route, routingSummary(ok))
	}
	return c, ok
}

func routingEpoch(t *testing.T, m map[string]any) string {
	t.Helper()
	epoch, _ := m["mediaEpoch"].(string)
	if !epochPattern.MatchString(epoch) {
		t.Fatalf("no media epoch in %s", routingSummary(m))
	}
	return epoch
}

func routingTicket(t *testing.T, m map[string]any) string {
	t.Helper()
	ticket, _ := m["ticket"].(string)
	// Not dkcB64: its failure message would print the ticket value.
	raw, err := base64.RawURLEncoding.DecodeString(ticket)
	if err != nil || len(raw) != 32 || dkcEnc(raw) != ticket {
		t.Fatalf("hello-ok ticket is not a canonical 32-byte value: %s", routingSummary(m))
	}
	return ticket
}

func routingRandomTicket(t *testing.T, size int) string {
	t.Helper()
	b := make([]byte, size)
	if _, err := rand.Read(b); err != nil {
		t.Fatal(err)
	}
	return dkcEnc(b)
}

// routingAdmitted checks a data socket's hello-ok: admitted, and no ticket in it.
func routingAdmitted(t *testing.T, m map[string]any) {
	t.Helper()
	if m["type"] != "hello-ok" {
		t.Fatalf("data socket not admitted: %s", routingSummary(m))
	}
	if _, present := m["ticket"]; present {
		t.Fatal("data socket hello-ok carries a ticket")
	}
}

func routingExpectPresence(t *testing.T, c *websocket.Conn, kind string, present bool) {
	t.Helper()
	m := readJSON(t, c)
	if m["type"] != kind+"-peer" || m["present"] != present {
		t.Fatalf("expected %s present=%v, got %s", kind, present, routingSummary(m))
	}
}

// routingJoin admits a viewer data client and a host data socket on one epoch
// and drains the presence events. hostTicket is sent in the host hello when set.
func routingJoin(t *testing.T, f *relayFixture, kind, epoch, pcID string, hostHeaders http.Header, hostTicket string) (*websocket.Conn, *websocket.Conn) {
	t.Helper()
	query := "?epoch=" + epoch + "&pc=" + pcID
	client, ok := routingOpenHello(t, f, "/"+kind+"-ws"+query, routingViewerHeaders(f), map[string]any{"type": "hello", "role": kind + "-client"})
	routingAdmitted(t, ok)
	routingExpectPresence(t, client, kind, false)
	hello := map[string]any{"type": "hello", "role": kind + "-host"}
	if hostTicket != "" {
		hello["ticket"] = hostTicket
	}
	host, ok := routingOpenHello(t, f, "/host-"+kind+"-ws"+query, hostHeaders, hello)
	routingAdmitted(t, ok)
	routingExpectPresence(t, host, kind, true)
	routingExpectPresence(t, client, kind, true)
	return client, host
}

func routingWriteBinary(t *testing.T, c *websocket.Conn, data []byte) {
	t.Helper()
	if err := c.WriteMessage(websocket.BinaryMessage, data); err != nil {
		t.Fatal(err)
	}
}

func routingExpectBinary(t *testing.T, c *websocket.Conn, want []byte) {
	t.Helper()
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	kind, got, err := c.ReadMessage()
	if err != nil || kind != websocket.BinaryMessage || !bytes.Equal(got, want) {
		t.Fatalf("binary mismatch (kind %d, err %v)", kind, err)
	}
}

func routingExpectSDP(t *testing.T, c *websocket.Conn, want string) {
	t.Helper()
	if m := readJSON(t, c); m["sdp"] != want {
		t.Fatalf("expected sdp %q, got %v", want, m["sdp"])
	}
}

// routingMediaFrame reuses the browser-generated media frame and marks its last byte.
func routingMediaFrame(t *testing.T, marker byte) []byte {
	t.Helper()
	frame, err := hex.DecodeString(packetFixture(t).MediaHex)
	if err != nil {
		t.Fatal(err)
	}
	frame[len(frame)-1] = marker
	return frame
}

// routingFilePacket returns one browser-generated file-client packet from the shared fixture.
func routingFilePacket(t *testing.T, index int) []byte {
	t.Helper()
	packet, err := hex.DecodeString(packetFixture(t).Files[index])
	if err != nil {
		t.Fatal(err)
	}
	return packet
}

func routingClock(f *relayFixture, at time.Time) {
	f.relay.mu.Lock()
	defer f.relay.mu.Unlock()
	f.relay.now = func() time.Time { return at }
}

func routingAdminStatus(t *testing.T, f *relayFixture, method, path, token string) (int, []byte) {
	t.Helper()
	req, err := http.NewRequest(method, f.server.URL+path, nil)
	if err != nil {
		t.Fatal(err)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	resp, err := f.server.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, body
}

// routingFindPC returns the admin-listed row for id, or nil.
func routingFindPC(t *testing.T, f *relayFixture, id string) map[string]any {
	t.Helper()
	status, body := routingAdminStatus(t, f, "GET", "/__gate/manage/pcs", f.cfg.AdminToken)
	if status != http.StatusOK {
		t.Fatalf("list status %d", status)
	}
	var list []map[string]any
	if err := json.Unmarshal(body, &list); err != nil {
		t.Fatal(err)
	}
	for _, row := range list {
		if row["id"] == id {
			return row
		}
	}
	return nil
}

func routingWaitOffline(t *testing.T, f *relayFixture, id string) {
	t.Helper()
	waitRelay(t, func() bool {
		row := routingFindPC(t, f, id)
		return row != nil && row["online"] == false
	})
}

func TestRoutingRegistrationNeedsTokenOnlyForNewIDs(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	route := "/host-ws?pc=" + pc.id
	bearer := routingBearer(f.cfg.HostGateToken)
	// An unknown ID is refused before any challenge without TOKEN, or with a wrong one.
	routingExpectHelloRefused(t, f, route, bearer, routingHostHello(pc, ""))
	routingExpectHelloRefused(t, f, route, bearer, routingHostHello(pc, routingRandomTicket(t, 32)))
	first, _ := routingRegister(t, f, pc, f.cfg.Token)
	// A remembered ID needs only the signature.
	first.Close()
	routingRegister(t, f, pc, "")
	// A token that is present must still match, even for a remembered ID.
	routingExpectHelloRefused(t, f, route, bearer, routingHostHello(pc, routingRandomTicket(t, 32)))
	routingRegister(t, f, pc, f.cfg.Token)
}

func TestRoutingRegistrationProofBindsOriginNonceAndKey(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	other := routingNewPC(t)
	var captured string
	first := routingProveWith(t, f, pc, f.cfg.Token, func(nonce []byte) string {
		captured = routingSign(f.cfg.PublicURL, nonce, pc, pc.priv)
		return captured
	})
	if ok := readJSON(t, first); ok["type"] != "hello-ok" {
		t.Fatalf("valid registration refused: %s", routingSummary(ok))
	}
	// Replaying a captured proof against a fresh challenge fails.
	replay := routingProveWith(t, f, pc, "", func([]byte) string { return captured })
	routingReadUntilClose(t, replay, websocket.ClosePolicyViolation)
	// A proof bound to another relay origin fails.
	wrongOrigin := routingProveWith(t, f, pc, "", func(nonce []byte) string {
		return routingSign(routingOtherOrigin, nonce, pc, pc.priv)
	})
	routingReadUntilClose(t, wrongOrigin, websocket.ClosePolicyViolation)
	// The right ID and key, signed by a different key, fails.
	wrongKey := routingProveWith(t, f, pc, "", func(nonce []byte) string {
		return routingSign(f.cfg.PublicURL, nonce, pc, other.priv)
	})
	routingReadUntilClose(t, wrongKey, websocket.ClosePolicyViolation)
	// Control: the correct proof on a fresh connection is admitted.
	routingRegister(t, f, pc, "")
}

func TestRoutingHelloPublicMustDeriveID(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	other := routingNewPC(t)
	route := "/host-ws?pc=" + pc.id
	bearer := routingBearer(f.cfg.HostGateToken)
	// URL and hello ID agree, but the key does not hash to that ID.
	mismatch := routingHostHello(pc, f.cfg.Token)
	mismatch["public"] = other.public
	routingExpectHelloRefused(t, f, route, bearer, mismatch)
	// A key that is not 32 bytes never reaches the challenge.
	short := routingHostHello(pc, f.cfg.Token)
	short["public"] = dkcEnc(pc.pub[:16])
	routingExpectHelloRefused(t, f, route, bearer, short)
}

func TestRoutingHostSignalNeedsBearerAndViewerCookie(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	routingRefusedUpgrade(t, f, "/host-ws?pc="+pc.id, http.Header{})
	routingRefusedUpgrade(t, f, "/host-ws?pc="+pc.id, routingViewerHeaders(f))
	routingRefusedUpgrade(t, f, "/host-ws?pc="+pc.id, routingBearer(strings.Repeat("w", 40)))
	routingRefusedUpgrade(t, f, "/ws?pc="+pc.id, http.Header{"Origin": {f.cfg.PublicURL}})
}

func TestRoutingTicketsAdmitDataHostsByBearerOrTicket(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	_, hello := routingRegister(t, f, pc, f.cfg.Token)
	ticket := routingTicket(t, hello)
	_, viewer := routingViewer(t, f, "/ws?pc="+pc.id)
	query := "?epoch=" + routingEpoch(t, viewer) + "&pc=" + pc.id
	for _, kind := range []string{"media", "file"} {
		route := "/host-" + kind + "-ws" + query
		_, bearerHost := routingOpenHello(t, f, route, routingBearer(f.cfg.HostGateToken), map[string]any{"type": "hello", "role": kind + "-host"})
		routingAdmitted(t, bearerHost)
		_, ticketHost := routingOpenHello(t, f, route, http.Header{}, routingDataHello(kind+"-host", ticket))
		routingAdmitted(t, ticketHost)
		routingExpectHelloRefused(t, f, route, http.Header{}, map[string]any{"type": "hello", "role": kind + "-host"})
	}
}

func TestRoutingSignalingTicketRejectsWrongAndForeignTickets(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	_, helloA := routingRegister(t, f, a, f.cfg.Token)
	_, helloB := routingRegister(t, f, b, f.cfg.Token)
	ticketA, ticketB := routingTicket(t, helloA), routingTicket(t, helloB)
	_, viewerA := routingViewer(t, f, "/ws?pc="+a.id)
	_, viewerB := routingViewer(t, f, "/ws?pc="+b.id)
	mediaA := "/host-media-ws?epoch=" + routingEpoch(t, viewerA) + "&pc=" + a.id
	mediaB := "/host-media-ws?epoch=" + routingEpoch(t, viewerB) + "&pc=" + b.id
	routingExpectHelloRefused(t, f, mediaA, http.Header{}, routingDataHello("media-host", routingRandomTicket(t, 32)))
	routingExpectHelloRefused(t, f, mediaA, http.Header{}, routingDataHello("media-host", routingRandomTicket(t, 16)))
	routingExpectHelloRefused(t, f, mediaA, http.Header{}, routingDataHello("media-host", ticketB))
	routingExpectHelloRefused(t, f, mediaB, http.Header{}, routingDataHello("media-host", ticketA))
	_, admitted := routingOpenHello(t, f, mediaA, http.Header{}, routingDataHello("media-host", ticketA))
	routingAdmitted(t, admitted)
}

func TestRoutingSignalingTicketExpiresAtTwelveHours(t *testing.T) {
	f := newRelayFixture(t)
	routingClock(f, routingBase)
	pc := routingNewPC(t)
	_, hello := routingRegister(t, f, pc, f.cfg.Token)
	ticket := routingTicket(t, hello)
	_, viewer := routingViewer(t, f, "/ws?pc="+pc.id)
	query := "?epoch=" + routingEpoch(t, viewer) + "&pc=" + pc.id
	routingClock(f, routingBase.Add(12*time.Hour-time.Second))
	_, admitted := routingOpenHello(t, f, "/host-file-ws"+query, http.Header{}, routingDataHello("file-host", ticket))
	routingAdmitted(t, admitted)
	routingClock(f, routingBase.Add(12*time.Hour))
	routingExpectHelloRefused(t, f, "/host-media-ws"+query, http.Header{}, routingDataHello("media-host", ticket))
}

func TestRoutingSignalingTicketEndsWithItsHostSocket(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	host, hello := routingRegister(t, f, pc, f.cfg.Token)
	ticket := routingTicket(t, hello)
	_, viewer := routingViewer(t, f, "/ws?pc="+pc.id)
	query := "?epoch=" + routingEpoch(t, viewer) + "&pc=" + pc.id
	_, admitted := routingOpenHello(t, f, "/host-media-ws"+query, http.Header{}, routingDataHello("media-host", ticket))
	routingAdmitted(t, admitted)
	host.Close()
	routingWaitOffline(t, f, pc.id)
	routingExpectHelloRefused(t, f, "/host-file-ws"+query, http.Header{}, routingDataHello("file-host", ticket))
}

func TestRoutingViewerTicketBoundToEpochAndPCAndCookie(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	_, first := routingViewer(t, f, "/ws?pc="+a.id)
	ticket1, epoch1 := routingTicket(t, first), routingEpoch(t, first)
	// A new viewer on the same PC takes the slot, so the first viewer's ticket and epoch stop working.
	_, second := routingViewer(t, f, "/ws?pc="+a.id)
	ticket2, epoch2 := routingTicket(t, second), routingEpoch(t, second)
	if epoch1 == epoch2 {
		t.Fatal("viewer epoch reused")
	}
	_, otherViewer := routingViewer(t, f, "/ws?pc="+b.id)
	ticketB := routingTicket(t, otherViewer)
	mediaRoute := "/media-ws?epoch=" + epoch2 + "&pc=" + a.id
	routingExpectHelloRefused(t, f, mediaRoute, routingViewerHeaders(f), routingDataHello("media-client", ticket1))
	routingRefusedUpgrade(t, f, "/media-ws?epoch="+epoch1+"&pc="+a.id, routingViewerHeaders(f))
	routingExpectHelloRefused(t, f, mediaRoute, routingViewerHeaders(f), routingDataHello("media-client", ticketB))
	routingExpectHelloRefused(t, f, mediaRoute, routingViewerHeaders(f), routingDataHello("media-client", routingRandomTicket(t, 32)))
	routingRefusedUpgrade(t, f, mediaRoute, http.Header{"Origin": {f.cfg.PublicURL}})
	_, withTicket := routingOpenHello(t, f, mediaRoute, routingViewerHeaders(f), routingDataHello("media-client", ticket2))
	routingAdmitted(t, withTicket)
	_, withoutTicket := routingOpenHello(t, f, mediaRoute, routingViewerHeaders(f), map[string]any{"type": "hello", "role": "media-client"})
	routingAdmitted(t, withoutTicket)
}

func TestRoutingTwoPCsKeepEpochsMediaAndFileApart(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	routingRegister(t, f, a, f.cfg.Token)
	routingRegister(t, f, b, f.cfg.Token)
	_, okA := routingViewer(t, f, "/ws?pc="+a.id)
	_, okB := routingViewer(t, f, "/ws?pc="+b.id)
	epochA, epochB := routingEpoch(t, okA), routingEpoch(t, okB)
	if epochA == epochB {
		t.Fatal("two PCs share an epoch")
	}
	bearer := routingBearer(f.cfg.HostGateToken)
	// Each PC's epoch opens only that PC's data sockets.
	routingRefusedUpgrade(t, f, "/host-media-ws?epoch="+epochA+"&pc="+b.id, bearer)
	routingRefusedUpgrade(t, f, "/host-file-ws?epoch="+epochB+"&pc="+a.id, bearer)
	mediaClientA, mediaHostA := routingJoin(t, f, "media", epochA, a.id, bearer, "")
	mediaClientB, mediaHostB := routingJoin(t, f, "media", epochB, b.id, bearer, "")
	routingWriteBinary(t, mediaHostA, routingMediaFrame(t, 0xA1))
	routingExpectBinary(t, mediaClientA, routingMediaFrame(t, 0xA1))
	routingWriteBinary(t, mediaHostB, routingMediaFrame(t, 0xB1))
	routingExpectBinary(t, mediaClientB, routingMediaFrame(t, 0xB1))
	// B's frame was sent before this one, so a leak would be read first here.
	routingWriteBinary(t, mediaHostA, routingMediaFrame(t, 0xA2))
	routingExpectBinary(t, mediaClientA, routingMediaFrame(t, 0xA2))
	fileClientA, fileHostA := routingJoin(t, f, "file", epochA, a.id, bearer, "")
	fileClientB, fileHostB := routingJoin(t, f, "file", epochB, b.id, bearer, "")
	routingWriteBinary(t, fileClientA, routingFilePacket(t, 0))
	routingExpectBinary(t, fileHostA, routingFilePacket(t, 0))
	routingWriteBinary(t, fileClientB, routingFilePacket(t, 1))
	routingExpectBinary(t, fileHostB, routingFilePacket(t, 1))
	routingWriteBinary(t, fileClientA, routingFilePacket(t, 2))
	routingExpectBinary(t, fileHostA, routingFilePacket(t, 2))
}

func TestRoutingLegacyAndIDRoomsStayApart(t *testing.T) {
	f := newRelayFixture(t)
	legacy := routingLegacyHost(t, f)
	a, b := routingNewPC(t), routingNewPC(t)
	hostA, _ := routingRegister(t, f, a, f.cfg.Token)
	hostB, _ := routingRegister(t, f, b, f.cfg.Token)
	viewerA, _ := routingViewer(t, f, "/ws?pc="+a.id)
	viewerB, _ := routingViewer(t, f, "/ws?pc="+b.id)
	routingSend(t, viewerA, map[string]any{"type": "offer", "sdp": "a-offer"})
	routingExpectSDP(t, hostA, "a-offer")
	routingSend(t, hostB, map[string]any{"type": "answer", "sdp": "b-answer"})
	routingExpectSDP(t, viewerB, "b-answer")
	routingSend(t, legacy, map[string]any{"type": "answer", "sdp": "legacy-answer"})
	routingSend(t, hostA, map[string]any{"type": "answer", "sdp": "a-answer"})
	// The legacy answer was sent before this one; it must not reach A's viewer.
	routingExpectSDP(t, viewerA, "a-answer")
	routingSend(t, viewerB, map[string]any{"type": "offer", "sdp": "b-offer"})
	routingExpectSDP(t, hostB, "b-offer")
	// With no PC online, the default target is the legacy room, which still works.
	hostA.Close()
	hostB.Close()
	routingWaitOffline(t, f, a.id)
	routingWaitOffline(t, f, b.id)
	legacyViewer, okLegacy := routingViewer(t, f, "/ws")
	if _, present := okLegacy["pc"]; present {
		t.Fatal("legacy default names a pc")
	}
	routingSend(t, legacyViewer, map[string]any{"type": "offer", "sdp": "legacy-offer"})
	routingExpectSDP(t, legacy, "legacy-offer")
}

func TestRoutingDefaultTargetOrder(t *testing.T) {
	t.Run("empty relay uses the legacy room", func(t *testing.T) {
		f := newRelayFixture(t)
		_, ok := routingViewer(t, f, "/ws")
		if _, present := ok["pc"]; present {
			t.Fatalf("empty relay names a pc: %s", routingSummary(ok))
		}
	})
	t.Run("most recently registered online PC wins", func(t *testing.T) {
		f := newRelayFixture(t)
		a, b := routingNewPC(t), routingNewPC(t)
		routingClock(f, routingBase.Add(time.Minute))
		routingRegister(t, f, a, f.cfg.Token)
		routingClock(f, routingBase.Add(2*time.Minute))
		routingRegister(t, f, b, f.cfg.Token)
		_, ok := routingViewer(t, f, "/ws")
		if ok["pc"] != b.id {
			t.Fatalf("default picked %v, want %s", ok["pc"], b.id)
		}
		// A signature-only registration refreshes A's last seen time and moves it first.
		routingClock(f, routingBase.Add(3*time.Minute))
		routingRegister(t, f, a, "")
		_, ok = routingViewer(t, f, "/ws")
		if ok["pc"] != a.id {
			t.Fatalf("default picked %v after refresh, want %s", ok["pc"], a.id)
		}
	})
	t.Run("online PC beats online legacy host, which serves once none is online", func(t *testing.T) {
		f := newRelayFixture(t)
		legacy := routingLegacyHost(t, f)
		a := routingNewPC(t)
		hostA, _ := routingRegister(t, f, a, f.cfg.Token)
		_, ok := routingViewer(t, f, "/ws")
		if ok["pc"] != a.id {
			t.Fatalf("online legacy host beat online PC: %s", routingSummary(ok))
		}
		hostA.Close()
		routingWaitOffline(t, f, a.id)
		fallback, okFallback := routingViewer(t, f, "/ws")
		if _, present := okFallback["pc"]; present {
			t.Fatal("fallback names a pc while only the legacy host is online")
		}
		routingSend(t, fallback, map[string]any{"type": "offer", "sdp": "fallback-offer"})
		routingExpectSDP(t, legacy, "fallback-offer")
	})
	t.Run("exactly one listed offline PC is the default", func(t *testing.T) {
		f := newRelayFixture(t)
		a := routingNewPC(t)
		host, _ := routingRegister(t, f, a, f.cfg.Token)
		host.Close()
		routingWaitOffline(t, f, a.id)
		_, ok := routingViewer(t, f, "/ws")
		if ok["pc"] != a.id {
			t.Fatalf("default picked %v, want the only listed PC %s", ok["pc"], a.id)
		}
	})
	t.Run("latest listed PC when none is online", func(t *testing.T) {
		f := newRelayFixture(t)
		a, b := routingNewPC(t), routingNewPC(t)
		routingClock(f, routingBase.Add(time.Minute))
		hostA, _ := routingRegister(t, f, a, f.cfg.Token)
		routingClock(f, routingBase.Add(2*time.Minute))
		hostB, _ := routingRegister(t, f, b, f.cfg.Token)
		hostA.Close()
		hostB.Close()
		routingWaitOffline(t, f, a.id)
		routingWaitOffline(t, f, b.id)
		_, ok := routingViewer(t, f, "/ws")
		if ok["pc"] != b.id {
			t.Fatalf("default picked %v, want latest %s", ok["pc"], b.id)
		}
	})
}

func TestRoutingAdminListsClosesAndForcesTokenReRegistration(t *testing.T) {
	f := newRelayFixture(t)
	routingClock(f, routingBase)
	pc := routingNewPC(t)
	host, _ := routingRegister(t, f, pc, f.cfg.Token)
	_, viewer := routingViewer(t, f, "/ws?pc="+pc.id)
	_, mediaHost := routingJoin(t, f, "media", routingEpoch(t, viewer), pc.id, routingBearer(f.cfg.HostGateToken), "")
	stamp := float64(routingBase.UnixMilli())
	row := routingFindPC(t, f, pc.id)
	if row == nil || row["public"] != pc.public || row["online"] != true || row["firstSeen"] != stamp || row["lastSeen"] != stamp {
		t.Fatalf("listed PC = %v", row)
	}
	for _, token := range []string{"", f.cfg.HostGateToken, strings.Repeat("w", 40)} {
		status, body := routingAdminStatus(t, f, "GET", "/__gate/manage/pcs", token)
		if status == http.StatusOK || strings.Contains(string(body), pc.id) {
			t.Fatalf("list served to a non-admin caller: %d", status)
		}
	}
	// The DELETE path accepts the ID in any accepted spelling.
	compact := strings.ToLower(strings.ReplaceAll(pc.id, "-", ""))
	if status, _ := routingAdminStatus(t, f, "DELETE", "/__gate/manage/pcs/"+compact, f.cfg.AdminToken); status != http.StatusNoContent {
		t.Fatalf("delete status %d", status)
	}
	routingReadUntilClose(t, host, websocket.ClosePolicyViolation)
	routingReadUntilClose(t, mediaHost, websocket.ClosePolicyViolation)
	if routingFindPC(t, f, pc.id) != nil {
		t.Fatal("deleted PC still listed")
	}
	if status, _ := routingAdminStatus(t, f, "DELETE", "/__gate/manage/pcs/not-a-device", f.cfg.AdminToken); status != http.StatusNotFound {
		t.Fatalf("bad ID delete status %d", status)
	}
	// Signature alone no longer admits the ID, and TOKEN registers it again.
	routingExpectHelloRefused(t, f, "/host-ws?pc="+pc.id, routingBearer(f.cfg.HostGateToken), routingHostHello(pc, ""))
	routingRegister(t, f, pc, f.cfg.Token)
	if row := routingFindPC(t, f, pc.id); row == nil || row["online"] != true {
		t.Fatalf("re-registered PC = %v", row)
	}
}

func TestRoutingViewerReplaceAndDepartCloseOnlyTheirRoomMedia(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	routingRegister(t, f, a, f.cfg.Token)
	hostB, _ := routingRegister(t, f, b, f.cfg.Token)
	viewerA1, okA1 := routingViewer(t, f, "/ws?pc="+a.id)
	viewerB, okB := routingViewer(t, f, "/ws?pc="+b.id)
	epochA1, epochB := routingEpoch(t, okA1), routingEpoch(t, okB)
	bearer := routingBearer(f.cfg.HostGateToken)
	mediaClientA1, mediaHostA1 := routingJoin(t, f, "media", epochA1, a.id, bearer, "")
	fileClientA1, fileHostA1 := routingJoin(t, f, "file", epochA1, a.id, bearer, "")
	mediaClientB, mediaHostB := routingJoin(t, f, "media", epochB, b.id, bearer, "")
	fileClientB, fileHostB := routingJoin(t, f, "file", epochB, b.id, bearer, "")
	// Replacing the viewer of room A closes its old viewer and every media and file peer of room A.
	viewerA2, okA2 := routingViewer(t, f, "/ws?pc="+a.id)
	epochA2 := routingEpoch(t, okA2)
	if epochA2 == epochA1 {
		t.Fatal("replacement viewer reused the epoch")
	}
	routingReadUntilClose(t, viewerA1, websocket.ClosePolicyViolation)
	for _, c := range []*websocket.Conn{mediaClientA1, mediaHostA1, fileClientA1, fileHostA1} {
		routingReadUntilClose(t, c, websocket.CloseGoingAway)
	}
	// Room B keeps forwarding on media, file and signaling.
	routingWriteBinary(t, mediaHostB, routingMediaFrame(t, 0xB2))
	routingExpectBinary(t, mediaClientB, routingMediaFrame(t, 0xB2))
	routingWriteBinary(t, fileClientB, routingFilePacket(t, 1))
	routingExpectBinary(t, fileHostB, routingFilePacket(t, 1))
	routingSend(t, viewerB, map[string]any{"type": "offer", "sdp": "b-offer-2"})
	routingExpectSDP(t, hostB, "b-offer-2")
	// Peers joined on the replacement epoch close when that viewer leaves; room B still forwards.
	mediaClientA2, mediaHostA2 := routingJoin(t, f, "media", epochA2, a.id, bearer, "")
	fileClientA2, fileHostA2 := routingJoin(t, f, "file", epochA2, a.id, bearer, "")
	routingWriteBinary(t, mediaHostA2, routingMediaFrame(t, 0xA3))
	routingExpectBinary(t, mediaClientA2, routingMediaFrame(t, 0xA3))
	viewerA2.Close()
	for _, c := range []*websocket.Conn{mediaClientA2, mediaHostA2, fileClientA2, fileHostA2} {
		routingReadUntilClose(t, c, websocket.CloseGoingAway)
	}
	routingWriteBinary(t, mediaHostB, routingMediaFrame(t, 0xB3))
	routingExpectBinary(t, mediaClientB, routingMediaFrame(t, 0xB3))
	routingSend(t, viewerB, map[string]any{"type": "offer", "sdp": "b-offer-3"})
	routingExpectSDP(t, hostB, "b-offer-3")
}

func TestRoutingDeletingOnePCClosesOnlyItsHostSockets(t *testing.T) {
	f := newRelayFixture(t)
	a, b := routingNewPC(t), routingNewPC(t)
	hostA, _ := routingRegister(t, f, a, f.cfg.Token)
	hostB, _ := routingRegister(t, f, b, f.cfg.Token)
	_, okA := routingViewer(t, f, "/ws?pc="+a.id)
	viewerB, okB := routingViewer(t, f, "/ws?pc="+b.id)
	epochA, epochB := routingEpoch(t, okA), routingEpoch(t, okB)
	bearer := routingBearer(f.cfg.HostGateToken)
	_, mediaHostA := routingJoin(t, f, "media", epochA, a.id, bearer, "")
	_, fileHostA := routingJoin(t, f, "file", epochA, a.id, bearer, "")
	mediaClientB, mediaHostB := routingJoin(t, f, "media", epochB, b.id, bearer, "")
	fileClientB, fileHostB := routingJoin(t, f, "file", epochB, b.id, bearer, "")
	if status, _ := routingAdminStatus(t, f, "DELETE", "/__gate/manage/pcs/"+a.id, f.cfg.AdminToken); status != http.StatusNoContent {
		t.Fatalf("delete status %d", status)
	}
	for _, c := range []*websocket.Conn{hostA, mediaHostA, fileHostA} {
		routingReadUntilClose(t, c, websocket.ClosePolicyViolation)
	}
	// Room B keeps forwarding on signaling, media and file.
	routingSend(t, viewerB, map[string]any{"type": "offer", "sdp": "b-offer"})
	routingExpectSDP(t, hostB, "b-offer")
	routingSend(t, hostB, map[string]any{"type": "answer", "sdp": "b-answer"})
	routingExpectSDP(t, viewerB, "b-answer")
	routingWriteBinary(t, mediaHostB, routingMediaFrame(t, 0xB1))
	routingExpectBinary(t, mediaClientB, routingMediaFrame(t, 0xB1))
	routingWriteBinary(t, fileClientB, routingFilePacket(t, 1))
	routingExpectBinary(t, fileHostB, routingFilePacket(t, 1))
}

func TestRoutingKnownIDRejectsForeignPublic(t *testing.T) {
	f := newRelayFixture(t)
	pc := routingNewPC(t)
	other := routingNewPC(t)
	host, _ := routingRegister(t, f, pc, f.cfg.Token)
	host.Close()
	route := "/host-ws?pc=" + pc.id
	bearer := routingBearer(f.cfg.HostGateToken)
	// The ID is remembered with its own key, so another key is refused with or without TOKEN.
	mismatch := routingHostHello(pc, "")
	mismatch["public"] = other.public
	routingExpectHelloRefused(t, f, route, bearer, mismatch)
	mismatch["token"] = f.cfg.Token
	routingExpectHelloRefused(t, f, route, bearer, mismatch)
	// Control: the remembered ID still registers with its own key and signature.
	routingRegister(t, f, pc, "")
}
