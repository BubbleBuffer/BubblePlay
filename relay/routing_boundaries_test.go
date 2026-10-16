package main

import (
	"context"
	"crypto/ed25519"
	"crypto/sha256"
	"net/http"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

func boundarySocket(t *testing.T, f *relayFixture, route string) *websocket.Conn {
	t.Helper()
	c, _, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+route, http.Header{"Authorization": {"Bearer " + f.cfg.HostGateToken}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

func boundaryClosed(t *testing.T, c *websocket.Conn) {
	t.Helper()
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	_, _, err := c.ReadMessage()
	if !websocket.IsCloseError(err, websocket.ClosePolicyViolation) {
		t.Fatalf("expected generic auth close, got %v", err)
	}
}

func TestHostURLMustMatchHello(t *testing.T) {
	f := newRelayFixture(t)
	ident := dkcIdentityNamed(t, dkcLoad(t), "pc")
	other := dkcIdentityNamed(t, dkcLoad(t), "viewer")
	for _, tc := range []struct {
		name, route string
		hello       map[string]any
	}{
		{"wrong-url-id", "/host-ws?pc=" + other.ID, map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token, "id": ident.ID, "public": ident.Public}},
		{"legacy-on-pc-socket", "/host-ws?pc=" + ident.ID, map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token}},
		{"id-on-legacy-socket", "/host-ws", map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token, "id": ident.ID, "public": ident.Public}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := boundarySocket(t, f, tc.route)
			if err := c.WriteJSON(tc.hello); err != nil {
				t.Fatal(err)
			}
			boundaryClosed(t, c)
		})
	}
	// Different accepted spellings of the same ID must agree before signing.
	c := boundarySocket(t, f, "/host-ws?pc="+strings.ToLower(strings.ReplaceAll(ident.ID, "-", "")))
	if err := c.WriteJSON(map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token, "id": strings.ReplaceAll(ident.ID, "1", "l"), "public": ident.Public}); err != nil {
		t.Fatal(err)
	}
	boundaryProof(t, f, c, ident)
	hello := readJSON(t, c)
	if hello["type"] != "hello-ok" || hello["pc"] != ident.ID {
		t.Fatal("canonical host registration failed")
	}
}

func TestInvalidPCQueryRefusesUpgrade(t *testing.T) {
	f := newRelayFixture(t)
	for _, route := range []string{"/ws?pc=", "/ws?pc=invalid", "/ws?pc=77VH-7M56-8GJK-Y12J;extra=value", "/ws?pc=77VH-7M56-8GJK-Y12J&pc=47Z3-3QX1-AJH6-2RKB", "/host-ws?pc=invalid"} {
		headers := http.Header{"Cookie": {f.cookie}, "Origin": {f.cfg.PublicURL}, "Authorization": {"Bearer " + f.cfg.HostGateToken}}
		c, response, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+route, headers)
		if c != nil {
			c.Close()
		}
		if err == nil || response == nil || response.StatusCode != 404 {
			t.Fatalf("invalid PC query admitted on %s", route)
		}
	}
}

func boundaryProof(t *testing.T, f *relayFixture, c *websocket.Conn, ident dkcIdentity) {
	t.Helper()
	challenge := readJSON(t, c)
	if challenge["type"] != "register-challenge" {
		t.Fatal("no challenge")
	}
	nonce := dkcB64(t, challenge["nonce"].(string))
	hash := sha256.Sum256(dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte("host"), []byte(f.cfg.PublicURL), nonce, []byte(ident.ID), dkcB64(t, ident.Public)))
	signature := ed25519.Sign(dkcPrivate(t, ident.SeedHex), hash[:])
	if err := c.WriteJSON(map[string]any{"type": "register-proof", "signature": dkcEnc(signature)}); err != nil {
		t.Fatal(err)
	}
}

func TestIDHostStillApprovesBrowserLogin(t *testing.T) {
	f := approvalFixture(t, "host")
	ident := dkcIdentityNamed(t, dkcLoad(t), "pc")
	host := boundarySocket(t, f, "/host-ws?pc="+ident.ID)
	approvalWrite(t, host, map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token, "id": ident.ID, "public": ident.Public})
	boundaryProof(t, f, host, ident)
	helloEpoch(t, host)
	approvalWrite(t, host, browserContract(t).Capability)
	approvalBarrier(t, host)
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
	id := approvalRequest(t, host)["id"].(string)
	approvalDecision(t, host, id, true)
	result := waitHeld(t, held)
	assertApprovalResult(t, result, true)
	login := loginRequest(f.relay.auth, fixturePassword, "", result.Result().Cookies())
	login.Header.Set("Origin", f.cfg.PublicURL)
	if got := runAuth(f.relay.auth, login).Code; got != http.StatusNoContent {
		t.Fatalf("approved password login = %d", got)
	}
}

func boundaryRegistered(t *testing.T, f *relayFixture, ident dkcIdentity) *websocket.Conn {
	t.Helper()
	host := boundarySocket(t, f, "/host-ws?pc="+ident.ID)
	approvalWrite(t, host, map[string]any{"type": "hello", "role": "host", "token": f.cfg.Token, "id": ident.ID, "public": ident.Public})
	boundaryProof(t, f, host, ident)
	helloEpoch(t, host)
	return host
}

func TestRoomPendingAndCapabilitiesStayLocal(t *testing.T) {
	f := newRelayFixture(t)
	fixture := dkcLoad(t)
	a, b := dkcIdentityNamed(t, fixture, "pc"), dkcIdentityNamed(t, fixture, "viewer")
	viewer := f.dial(t, "/ws?pc="+a.ID, "client")
	helloEpoch(t, viewer)
	approvalWrite(t, viewer, map[string]any{"type": "offer", "sdp": "room-a-only"})
	approvalBarrier(t, viewer)
	hostB := boundaryRegistered(t, f, b)
	// A pending offer must never be delivered when the other PC registers.
	approvalBarrier(t, hostB)
	hostA := boundaryRegistered(t, f, a)
	if offer := readJSON(t, hostA); offer["type"] != "offer" || offer["sdp"] != "room-a-only" {
		t.Fatal("pending offer lost or routed to wrong PC")
	}
	viewer.Close()
	waitRelay(t, func() bool {
		f.relay.mu.Lock()
		defer f.relay.mu.Unlock()
		return f.relay.peers["signal:client:"+a.ID] == nil
	})
	approvalWrite(t, hostA, map[string]any{"type": "host-capabilities", "browserApproval": true})
	approvalWrite(t, hostB, map[string]any{"type": "host-capabilities", "browserApproval": false})
	approvalBarrier(t, hostA)
	approvalBarrier(t, hostB)
	viewerA := f.dial(t, "/ws?pc="+a.ID, "client")
	helloEpoch(t, viewerA)
	if cap := readJSON(t, viewerA); cap["type"] != "host-capabilities" || cap["browserApproval"] != true {
		t.Fatal("room A capability cache changed")
	}
	viewerB := f.dial(t, "/ws?pc="+b.ID, "client")
	helloEpoch(t, viewerB)
	if cap := readJSON(t, viewerB); cap["type"] != "host-capabilities" || cap["browserApproval"] != false {
		t.Fatal("room B capability cache changed")
	}
	hostB.Close()
	waitRelay(t, func() bool {
		f.relay.mu.Lock()
		defer f.relay.mu.Unlock()
		return f.relay.peers["signal:host:"+b.ID] == nil
	})
	replacement := f.dial(t, "/ws?pc="+a.ID, "client")
	helloEpoch(t, replacement)
	if cap := readJSON(t, replacement); cap["type"] != "host-capabilities" || cap["browserApproval"] != true {
		t.Fatal("other host departure cleared room A cache")
	}
}

func TestLegacyTicketsCannotSwapSides(t *testing.T) {
	f := newRelayFixture(t)
	host := f.dial(t, "/host-ws", "host")
	hostHello := readJSON(t, host)
	viewer := f.dial(t, "/ws", "client")
	viewerHello := readJSON(t, viewer)
	if _, present := hostHello["pc"]; present {
		t.Fatal("legacy host names a PC")
	}
	if _, present := viewerHello["pc"]; present {
		t.Fatal("legacy viewer names a PC")
	}
	hostTicket, viewerTicket := routingTicket(t, hostHello), routingTicket(t, viewerHello)
	query := "?epoch=" + routingEpoch(t, viewerHello)
	for _, kind := range []string{"media", "file"} {
		hostRoute, viewerRoute := "/host-"+kind+"-ws"+query, "/"+kind+"-ws"+query
		routingExpectHelloRefused(t, f, hostRoute, http.Header{}, routingDataHello(kind+"-host", viewerTicket))
		routingExpectHelloRefused(t, f, viewerRoute, routingViewerHeaders(f), routingDataHello(kind+"-client", hostTicket))
		_, admitted := routingOpenHello(t, f, hostRoute, http.Header{}, routingDataHello(kind+"-host", hostTicket))
		routingAdmitted(t, admitted)
		_, admitted = routingOpenHello(t, f, viewerRoute, routingViewerHeaders(f), routingDataHello(kind+"-client", viewerTicket))
		routingAdmitted(t, admitted)
	}
}
