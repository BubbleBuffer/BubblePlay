package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
	"unicode"

	"github.com/gorilla/websocket"
)

type browserApprovalContract struct {
	Capability map[string]any `json:"capability"`
	Request    struct {
		Type, ID, Client, Agent string
		ExpiresAt               int64 `json:"expiresAt"`
	} `json:"request"`
	Decision map[string]any `json:"decision"`
	Cancel   map[string]any `json:"cancel"`
	Limits   struct {
		IDMin, IDMax, AgentMax int
	} `json:"limits"`
	Invalid         []map[string]any `json:"invalid"`
	MethodsResponse struct {
		TOTP       bool     `json:"totp"`
		Approval   []string `json:"approval"`
		HostOnline bool     `json:"hostOnline"`
	} `json:"methodsResponse"`
}

func browserContract(t *testing.T) browserApprovalContract {
	t.Helper()
	data, err := os.ReadFile("../fixtures/browser-approval-contract.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixture browserApprovalContract
	if err := json.Unmarshal(data, &fixture); err != nil {
		t.Fatal(err)
	}
	return fixture
}

func approvalFixture(t *testing.T, methods ...string) *relayFixture {
	t.Helper()
	f := newRelayFixture(t)
	f.relay.auth.mu.Lock()
	f.relay.auth.cfg.ApprovalMethods = methods
	f.relay.auth.mu.Unlock()
	return f
}

func approvalWrite(t *testing.T, c *websocket.Conn, message any) {
	t.Helper()
	if err := c.WriteJSON(message); err != nil {
		t.Fatal(err)
	}
}

// A pong orders prior packets without poisoning the socket with a timeout read.
func approvalBarrier(t *testing.T, c *websocket.Conn) {
	t.Helper()
	approvalWrite(t, c, map[string]any{"type": "ping"})
	for {
		message := readJSON(t, c)
		switch message["type"] {
		case "pong":
			return
		case "host-capabilities", "peer-left":
			// Ordinary signaling events may already be queued on a browser socket.
		default:
			t.Fatalf("unexpected message before barrier: %v", message)
		}
	}
}

func approvalHost(t *testing.T, f *relayFixture, capable bool) *websocket.Conn {
	t.Helper()
	c := f.dial(t, "/host-ws", "host")
	helloEpoch(t, c)
	if capable {
		approvalWrite(t, c, browserContract(t).Capability)
		approvalBarrier(t, c)
	}
	return c
}

func approvalDecision(t *testing.T, c *websocket.Conn, id string, approve bool) {
	t.Helper()
	message := browserContract(t).Decision
	message["id"], message["approve"] = id, approve
	approvalWrite(t, c, message)
}

func approvalRequest(t *testing.T, c *websocket.Conn) map[string]any {
	t.Helper()
	fixture := browserContract(t)
	message := readJSON(t, c)
	if message["type"] != fixture.Request.Type || len(message) != 5 {
		t.Fatalf("invalid approval request shape: %v", message)
	}
	id, ok := message["id"].(string)
	if !ok || len(id) < fixture.Limits.IDMin || len(id) > fixture.Limits.IDMax {
		t.Fatalf("invalid approval id: %q", id)
	}
	if message["client"] != fixture.Request.Client {
		t.Fatalf("client prefix=%v, want %s", message["client"], fixture.Request.Client)
	}
	agent, ok := message["agent"].(string)
	if !ok || len(agent) > fixture.Limits.AgentMax {
		t.Fatalf("invalid browser summary: %q", agent)
	}
	for _, r := range agent {
		if !unicode.IsPrint(r) {
			t.Fatalf("non-printable browser summary: %q", agent)
		}
	}
	expires, ok := message["expiresAt"].(float64)
	if !ok || expires <= float64(time.Now().UnixMilli()) || expires > float64(time.Now().Add(2*time.Minute).UnixMilli()) {
		t.Fatalf("invalid approval expiry: %v", message["expiresAt"])
	}
	return message
}

func approvalCancel(t *testing.T, c *websocket.Conn, id string) {
	t.Helper()
	message := readJSON(t, c)
	want := browserContract(t).Cancel
	want["id"] = id
	if !reflect.DeepEqual(message, want) {
		t.Fatalf("cancel=%v, want %v", message, want)
	}
}

func approvalCounts(a *Auth) (pending, gates int) {
	a.mu.Lock()
	defer a.mu.Unlock()
	return len(a.pending), len(a.state.Gates)
}

func assertApprovalHeld(t *testing.T, a *Auth, held <-chan *httptest.ResponseRecorder, gates int) {
	t.Helper()
	select {
	case w := <-held:
		t.Fatalf("invalid decision released browser: status=%d", w.Code)
	default:
	}
	if pending, got := approvalCounts(a); pending != 1 || got != gates {
		t.Fatalf("invalid decision changed approval state: pending=%d gates=%d, baseline=%d", pending, got, gates)
	}
}

func assertApprovalResult(t *testing.T, w *httptest.ResponseRecorder, approved bool) {
	t.Helper()
	if !approved {
		if w.Code != http.StatusNotFound || len(w.Result().Cookies()) != 0 {
			t.Fatalf("denied navigation status=%d cookies=%v", w.Code, w.Result().Cookies())
		}
		return
	}
	cookies := w.Result().Cookies()
	if w.Code != http.StatusOK || len(cookies) != 1 || cookies[0].Name != gateCookieName || strings.Contains(w.Body.String(), `id="totp"`) {
		t.Fatalf("approved navigation did not receive gate/password form: status=%d cookies=%v", w.Code, cookies)
	}
}

func TestBrowserApprovalHostDecisionsAndDuplicate(t *testing.T) {
	for _, approve := range []bool{true, false} {
		name := "deny"
		if approve {
			name = "approve"
		}
		t.Run(name, func(t *testing.T) {
			f := approvalFixture(t, "host")
			host := approvalHost(t, f, true)
			_, before := approvalCounts(f.relay.auth)
			held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
			request := approvalRequest(t, host)
			id := request["id"].(string)
			approvalDecision(t, host, id, approve)
			result := waitHeld(t, held)
			assertApprovalResult(t, result, approve)
			// The opposite replay must not create or revoke any gate.
			approvalDecision(t, host, id, !approve)
			approvalBarrier(t, host)
			want := before
			if approve {
				want++
			}
			if pending, gates := approvalCounts(f.relay.auth); pending != 0 || gates != want {
				t.Fatalf("duplicate decision changed state: pending=%d gates=%d, want=%d", pending, gates, want)
			}
			if approve {
				cookies := result.Result().Cookies()
				if f.relay.auth.AuthorizedBrowser(withCookies(authRequest(http.MethodGet, "/", ""), cookies)) {
					t.Fatal("host approval alone granted an application session")
				}
				login := loginRequest(f.relay.auth, fixturePassword, "", cookies)
				login.Header.Set("Origin", f.cfg.PublicURL)
				if got := runAuth(f.relay.auth, login).Code; got != http.StatusNoContent {
					t.Fatalf("approved browser password login status=%d", got)
				}
			}
		})
	}
}

func TestBrowserApprovalHostExpiryAndBrowserDisconnect(t *testing.T) {
	for _, disconnect := range []bool{false, true} {
		name := "expiry"
		if disconnect {
			name = "browser-disconnect"
		}
		t.Run(name, func(t *testing.T) {
			f := approvalFixture(t, "host")
			f.relay.auth.approvalTTL = 200 * time.Millisecond
			if disconnect {
				f.relay.auth.approvalTTL = 2 * time.Second
			}
			host := approvalHost(t, f, true)
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			_, before := approvalCounts(f.relay.auth)
			held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", ctx)
			id := approvalRequest(t, host)["id"].(string)
			if disconnect {
				cancel()
			}
			assertApprovalResult(t, waitHeld(t, held), false)
			approvalCancel(t, host, id)
			approvalDecision(t, host, id, true)
			approvalBarrier(t, host)
			if pending, gates := approvalCounts(f.relay.auth); pending != 0 || gates != before {
				t.Fatalf("late approval accepted: pending=%d gates=%d", pending, gates)
			}
		})
	}
}

func TestBrowserApprovalWrongConnectionsAndInvalidDecisions(t *testing.T) {
	f := approvalFixture(t, "host")
	host := approvalHost(t, f, true)
	client := f.dial(t, "/ws", "client")
	helloEpoch(t, client)
	_, before := approvalCounts(f.relay.auth)
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
	id := approvalRequest(t, host)["id"].(string)
	approvalDecision(t, client, id, true)
	approvalBarrier(t, client)
	assertApprovalHeld(t, f.relay.auth, held, before)
	for _, invalid := range browserContract(t).Invalid {
		if invalid["type"] != "approval-decision" {
			continue
		}
		if invalid["approve"] == "yes" {
			invalid["id"] = id
		}
		approvalWrite(t, host, invalid)
		approvalBarrier(t, host)
		assertApprovalHeld(t, f.relay.auth, held, before)
	}
	// Transport authentication alone is insufficient: hello registration is required.
	unregistered, _, err := f.dialer.Dial("wss"+strings.TrimPrefix(f.server.URL, "https")+"/host-ws", http.Header{"Authorization": {"Bearer " + f.cfg.HostGateToken}})
	if err != nil {
		t.Fatal(err)
	}
	defer unregistered.Close()
	approvalDecision(t, unregistered, id, true)
	_ = unregistered.SetReadDeadline(time.Now().Add(3 * time.Second))
	if _, _, err := unregistered.ReadMessage(); err == nil {
		t.Fatal("unregistered host decision was not rejected")
	}
	assertApprovalHeld(t, f.relay.auth, held, before)
	approvalDecision(t, host, id, true)
	assertApprovalResult(t, waitHeld(t, held), true)
}

func TestBrowserApprovalReplacementHostCannotDecideOldRequest(t *testing.T) {
	f := approvalFixture(t, "host")
	host := approvalHost(t, f, true)
	_, before := approvalCounts(f.relay.auth)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", ctx)
	id := approvalRequest(t, host)["id"].(string)
	replacement := approvalHost(t, f, true)
	approvalDecision(t, replacement, id, true)
	approvalBarrier(t, replacement)
	// Implementations may promptly cancel when the assigned host leaves, or retain
	// the held request until browser/TTL cancellation. Neither can grant a gate.
	if _, gates := approvalCounts(f.relay.auth); gates != before {
		t.Fatal("replacement host granted an old request")
	}
	_ = host.WriteJSON(map[string]any{"type": "approval-decision", "id": id, "approve": true})
	_ = host.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		if _, _, err := host.ReadMessage(); err != nil {
			break
		}
	}
	cancel()
	assertApprovalResult(t, waitHeld(t, held), false)
	if _, gates := approvalCounts(f.relay.auth); gates != before {
		t.Fatal("replaced host granted an old request")
	}
}

func approvalNtfy(t *testing.T, f *relayFixture) <-chan syntheticNotice {
	t.Helper()
	notices := make(chan syntheticNotice, 4)
	approvalNtfyServer(t, f, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var notice syntheticNotice
		if err := json.NewDecoder(r.Body).Decode(&notice); err != nil {
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		notices <- notice
		w.WriteHeader(http.StatusNoContent)
	}))
	return notices
}

func approvalNtfyServer(t *testing.T, f *relayFixture, handler http.Handler) {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	f.relay.auth.mu.Lock()
	f.relay.auth.cfg.NtfyURL = server.URL
	f.relay.auth.cfg.NtfyTopic = "synthetic-private"
	f.relay.auth.cfg.NtfyToken = strings.Repeat("n", 32)
	f.relay.auth.cfg.AllowInsecureLoopback = true
	f.relay.auth.mu.Unlock()
}

func approvalWaitSignal(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(3 * time.Second):
		t.Fatal(description)
	}
}

func TestBrowserApprovalHostSurvivesNtfyPublicationFailure(t *testing.T) {
	f := approvalFixture(t, "ntfy", "host")
	failed := make(chan struct{})
	approvalNtfyServer(t, f, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, r.Body)
		w.WriteHeader(http.StatusServiceUnavailable)
		w.(http.Flusher).Flush()
		close(failed)
	}))
	host := approvalHost(t, f, true)
	_, before := approvalCounts(f.relay.auth)
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
	id := approvalRequest(t, host)["id"].(string)
	approvalWaitSignal(t, failed, "ntfy publisher did not return its failure")
	approvalBarrier(t, host)
	assertApprovalHeld(t, f.relay.auth, held, before)
	approvalDecision(t, host, id, true)
	assertApprovalResult(t, waitHeld(t, held), true)
	if pending, gates := approvalCounts(f.relay.auth); pending != 0 || gates != before+1 {
		t.Fatalf("host approval after publication failure: pending=%d gates=%d", pending, gates)
	}
}

func TestBrowserApprovalHostCancelsInFlightNtfyPublication(t *testing.T) {
	f := approvalFixture(t, "ntfy", "host")
	entered, cancelled, release := make(chan struct{}), make(chan struct{}), make(chan struct{})
	approvalNtfyServer(t, f, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Consume to EOF so the HTTP server can observe the client going away.
		_, _ = io.Copy(io.Discard, r.Body)
		close(entered)
		select {
		case <-r.Context().Done():
			close(cancelled)
		case <-release:
			w.WriteHeader(http.StatusServiceUnavailable)
		}
	}))
	// Registered after server.Close so a failed test cannot leave Close blocked.
	t.Cleanup(func() { close(release) })
	host := approvalHost(t, f, true)
	_, before := approvalCounts(f.relay.auth)
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
	id := approvalRequest(t, host)["id"].(string)
	approvalWaitSignal(t, entered, "ntfy publication did not enter the stalled handler")
	approvalDecision(t, host, id, true)
	assertApprovalResult(t, waitHeld(t, held), true)
	approvalWaitSignal(t, cancelled, "host decision did not cancel the in-flight ntfy request")
	approvalBarrier(t, host)
	if pending, gates := approvalCounts(f.relay.auth); pending != 0 || gates != before+1 {
		t.Fatalf("in-flight publication left approval state: pending=%d gates=%d", pending, gates)
	}
}

func TestBrowserApprovalNtfyOnlyPublicationFailureReleasesBrowser(t *testing.T) {
	f := approvalFixture(t, "ntfy")
	notices := make(chan syntheticNotice, 1)
	approvalNtfyServer(t, f, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var notice syntheticNotice
		if err := json.NewDecoder(r.Body).Decode(&notice); err != nil {
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		notices <- notice
		w.WriteHeader(http.StatusServiceUnavailable)
	}))
	_, before := approvalCounts(f.relay.auth)
	started := time.Now()
	held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
	notice := waitNotice(t, notices)
	assertApprovalResult(t, waitHeld(t, held), false)
	if elapsed := time.Since(started); elapsed > time.Second {
		t.Fatalf("publication failure took %s to release browser", elapsed)
	}
	f.relay.auth.mu.Lock()
	pending, capabilities, gates := len(f.relay.auth.pending), len(f.relay.auth.capabilities), len(f.relay.auth.state.Gates)
	f.relay.auth.mu.Unlock()
	if pending != 0 || capabilities != 0 || gates != before {
		t.Fatalf("publication failure leaked approval state: pending=%d capabilities=%d gates=%d", pending, capabilities, gates)
	}
	decideNotice(f.relay.auth, notice, 0, "approve")
	decideNotice(f.relay.auth, notice, 1, "deny")
	if _, gates := approvalCounts(f.relay.auth); gates != before {
		t.Fatal("failed publication retained usable decision capabilities")
	}
}

func TestBrowserApprovalFirstNotifierDecisionWins(t *testing.T) {
	for _, winner := range []string{"host", "ntfy"} {
		for _, approve := range []bool{true, false} {
			decision := "deny"
			if approve {
				decision = "approve"
			}
			t.Run(winner+"-"+decision, func(t *testing.T) {
				f := approvalFixture(t, "ntfy", "host")
				notices := approvalNtfy(t, f)
				host := approvalHost(t, f, true)
				_, before := approvalCounts(f.relay.auth)
				held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
				id := approvalRequest(t, host)["id"].(string)
				notice := waitNotice(t, notices)
				index := 1
				if approve {
					index = 0
				}
				if winner == "ntfy" {
					decideNotice(f.relay.auth, notice, index, decision)
					approvalCancel(t, host, id)
				} else {
					approvalDecision(t, host, id, approve)
				}
				assertApprovalResult(t, waitHeld(t, held), approve)
				if winner == "ntfy" {
					approvalDecision(t, host, id, !approve)
					approvalBarrier(t, host)
				} else {
					otherDecision := "approve"
					if approve {
						otherDecision = "deny"
					}
					decideNotice(f.relay.auth, notice, 1-index, otherDecision)
				}
				want := before
				if approve {
					want++
				}
				if pending, gates := approvalCounts(f.relay.auth); pending != 0 || gates != want {
					t.Fatalf("losing decision changed state: pending=%d gates=%d want=%d", pending, gates, want)
				}
			})
		}
	}
}

func TestBrowserApprovalDoesNotNotifyIneligibleHost(t *testing.T) {
	for _, capability := range []bool{false, true} {
		name := "capability-absent"
		methods := []string{"totp", "ntfy", "host"}
		if capability {
			name = "host-method-disabled"
			methods = []string{"totp", "ntfy"}
		}
		t.Run(name, func(t *testing.T) {
			f := approvalFixture(t, methods...)
			notices := approvalNtfy(t, f)
			host := approvalHost(t, f, capability)
			held := heldRequest(f.relay.auth, "/?approval=request", "203.0.113.10", context.Background())
			notice := waitNotice(t, notices)
			approvalBarrier(t, host)
			decideNotice(f.relay.auth, notice, 0, "approve")
			assertApprovalResult(t, waitHeld(t, held), true)
			approvalBarrier(t, host)
		})
	}
}

func approvalMethods(t *testing.T, f *relayFixture) map[string]any {
	t.Helper()
	response, err := f.server.Client().Get(f.server.URL + "/__auth/methods")
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK || response.Header.Get("Cache-Control") != "no-store" {
		t.Fatalf("methods response status=%d cache=%q", response.StatusCode, response.Header.Get("Cache-Control"))
	}
	var methods map[string]any
	if err := json.NewDecoder(response.Body).Decode(&methods); err != nil {
		t.Fatal(err)
	}
	return methods
}

func TestBrowserApprovalMethodsAndCapabilityPresence(t *testing.T) {
	f := approvalFixture(t, "totp", "ntfy", "host")
	approvalNtfy(t, f)
	fixture := browserContract(t)
	notifiers := make([]any, len(fixture.MethodsResponse.Approval))
	for i, method := range fixture.MethodsResponse.Approval {
		notifiers[i] = method
	}
	want := map[string]any{"totp": fixture.MethodsResponse.TOTP, "approval": notifiers, "hostOnline": fixture.MethodsResponse.HostOnline, "sourceURL": defaultSourceURL}
	if got := approvalMethods(t, f); !reflect.DeepEqual(got, want) {
		t.Fatalf("methods=%v want=%v", got, want)
	}
	host := approvalHost(t, f, false)
	if got := approvalMethods(t, f); !reflect.DeepEqual(got, want) {
		t.Fatalf("host without capability reported online: %v", got)
	}
	// The existing full capability announcement also supports the extension.
	approvalWrite(t, host, map[string]any{"type": "host-capabilities", "serviceMode": false, "coldBoot": false, "secureDesktop": false, "secureText": false, "browserApproval": true})
	approvalBarrier(t, host)
	want["hostOnline"] = true
	if got := approvalMethods(t, f); !reflect.DeepEqual(got, want) {
		t.Fatalf("capable host not reported online: %v", got)
	}
	host.Close()
	waitRelay(t, func() bool {
		f.relay.mu.Lock()
		defer f.relay.mu.Unlock()
		return f.relay.peers["signal:host"] == nil
	})
	want["hostOnline"] = false
	if got := approvalMethods(t, f); !reflect.DeepEqual(got, want) {
		t.Fatalf("disconnected host reported online: %v", got)
	}
}

func TestBrowserApprovalOfflineHostAndUnadvertisedCapability(t *testing.T) {
	for _, connected := range []bool{false, true} {
		name := "offline"
		if connected {
			name = "legacy-host"
		}
		t.Run(name, func(t *testing.T) {
			f := approvalFixture(t, "host")
			var host *websocket.Conn
			if connected {
				host = approvalHost(t, f, false)
				approvalWrite(t, host, map[string]any{"type": "host-capabilities", "serviceMode": false, "coldBoot": false, "secureDesktop": false, "secureText": false})
				approvalBarrier(t, host)
			}
			started := time.Now()
			// Host-only configurations automatically request approval on navigation.
			w := waitHeld(t, heldRequest(f.relay.auth, "/", "203.0.113.10", context.Background()))
			body := strings.ToLower(w.Body.String())
			if w.Code != http.StatusServiceUnavailable || !strings.Contains(body, "pc") || !strings.Contains(body, "offline") || time.Since(started) > time.Second {
				t.Fatalf("offline host status=%d duration=%s body=%q", w.Code, time.Since(started), body)
			}
			if pending, _ := approvalCounts(f.relay.auth); pending != 0 {
				t.Fatal("offline host retained pending request")
			}
			if connected {
				approvalBarrier(t, host)
			}
		})
	}
}

func TestBrowserApprovalAdaptiveLoginLink(t *testing.T) {
	for _, methods := range [][]string{{"totp", "host"}, {"totp", "ntfy"}, {"totp", "ntfy", "host"}} {
		t.Run(strings.Join(methods, "+"), func(t *testing.T) {
			f := approvalFixture(t, methods...)
			if len(methods) > 2 || methods[1] == "ntfy" {
				approvalNtfy(t, f)
			}
			request, err := http.NewRequest(http.MethodGet, f.server.URL+"/", nil)
			if err != nil {
				t.Fatal(err)
			}
			request.Header.Set("Accept", "text/html")
			response, err := f.server.Client().Do(request)
			if err != nil {
				t.Fatal(err)
			}
			defer response.Body.Close()
			body, err := io.ReadAll(response.Body)
			if err != nil {
				t.Fatal(err)
			}
			if response.StatusCode != http.StatusOK || !strings.Contains(string(body), `id="totp"`) || !strings.Contains(string(body), `href="/?approval=request"`) || !strings.Contains(string(body), "Request approval instead") {
				t.Fatalf("adaptive login form/link missing: status=%d body=%s", response.StatusCode, body)
			}
			for _, destination := range []struct{ method, label string }{{"host", "on your PC"}, {"ntfy", "by notification"}} {
				enabled := false
				for _, method := range methods {
					enabled = enabled || method == destination.method
				}
				if strings.Contains(string(body), destination.label) != enabled {
					t.Fatalf("login destination label %q does not match methods %v", destination.label, methods)
				}
			}
		})
	}
}

func TestBrowserApprovalServerBuildsUserAgentSummary(t *testing.T) {
	f := approvalFixture(t, "host")
	host := approvalHost(t, f, true)
	held := make(chan *httptest.ResponseRecorder, 1)
	go func() {
		request := authRequest(http.MethodGet, "/?approval=request&agent=attacker-controlled", "")
		request.RemoteAddr = "203.0.113.10:1234"
		request.Header.Set("Accept", "text/html")
		request.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"+strings.Repeat("x", 200)+"\a")
		held <- runAuth(f.relay.auth, request)
	}()
	message := approvalRequest(t, host)
	if message["agent"] != browserContract(t).Request.Agent {
		t.Fatalf("server browser summary=%q want=%q", message["agent"], browserContract(t).Request.Agent)
	}
	approvalDecision(t, host, message["id"].(string), false)
	assertApprovalResult(t, waitHeld(t, held), false)
}
