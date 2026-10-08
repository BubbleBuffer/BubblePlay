package main

import (
	"bytes"
	"encoding/json"
	"log"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

func TestApprovalFixtureValidationLimits(t *testing.T) {
	fixture := browserContract(t)
	decision := func(value map[string]any) bool {
		data, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		var m map[string]json.RawMessage
		if err := json.Unmarshal(data, &m); err != nil {
			t.Fatal(err)
		}
		return validApprovalDecision(m)
	}
	if !decision(fixture.Decision) || !validApprovalAgent(fixture.Request.Agent) || !validApprovalID(fixture.Request.ID) {
		t.Fatal("valid contract example rejected")
	}
	for _, invalid := range fixture.Invalid {
		if invalid["type"] == "approval-decision" && decision(invalid) {
			t.Fatal("invalid fixture decision accepted")
		}
		if invalid["type"] == "approval-request" && validApprovalAgent(invalid["agent"].(string)) {
			t.Fatal("invalid fixture agent accepted")
		}
	}
	for _, length := range []int{fixture.Limits.IDMin - 1, fixture.Limits.IDMin, fixture.Limits.IDMax, fixture.Limits.IDMax + 1} {
		if got := validApprovalID(strings.Repeat("a", length)); got != (length >= fixture.Limits.IDMin && length <= fixture.Limits.IDMax) {
			t.Fatalf("id length %d accepted=%v", length, got)
		}
	}
	for _, length := range []int{fixture.Limits.AgentMax, fixture.Limits.AgentMax + 1} {
		if got := validApprovalAgent(strings.Repeat("a", length)); got != (length <= fixture.Limits.AgentMax) {
			t.Fatalf("agent length %d accepted=%v", length, got)
		}
	}
	for _, agent := range []string{"bad\nagent", "bad\x7fagent", "bad\aagent"} {
		if validApprovalAgent(agent) {
			t.Fatal("nonprintable agent accepted")
		}
	}
}

func TestApprovalUserAgentSummaries(t *testing.T) {
	for _, test := range []struct{ ua, want string }{
		{"Mozilla/5.0 (Windows NT 10.0) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0", "Edge 141 on Windows"},
		{"Mozilla/5.0 (Linux; Android 16) Chrome/141.0.0.0", "Chrome 141 on Android"},
		{"Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Version/18.0 Mobile/15 Safari/604.1", "Safari 18 on iOS"},
		{"Mozilla/5.0 (Macintosh; Intel Mac OS X) Firefox/141.0", "Firefox 141 on macOS"},
		{"Mozilla/5.0 (X11; CrOS x86_64) Chrome/141.0.0.0", "Chrome 141 on ChromeOS"},
		{"Mozilla/5.0 (X11; Linux x86_64) Firefox/141.0", "Firefox 141 on Linux"},
		{"", "Unknown browser on unknown OS"},
		{strings.Repeat("\a", 9000), "Unknown browser on unknown OS"},
	} {
		if got := browserAgent(test.ua); got != test.want || !validApprovalAgent(got) {
			t.Fatalf("summary=%q want=%q", got, test.want)
		}
	}
}

func TestRelayWarnsOnceForUntrustedForwardingHeaders(t *testing.T) {
	for _, header := range []string{"X-Forwarded-For", "Forwarded"} {
		t.Run(header, func(t *testing.T) {
			f := newRelayFixture(t)
			var output bytes.Buffer
			previous := log.Writer()
			log.SetOutput(&output)
			defer log.SetOutput(previous)
			request := httptest.NewRequest("GET", f.cfg.PublicURL+"/healthz", nil)
			f.relay.ServeHTTP(httptest.NewRecorder(), request)
			if output.Len() != 0 {
				t.Fatal("warning logged without forwarding headers")
			}
			request.Header.Set(header, "for=203.0.113.1")
			for i := 0; i < 2; i++ {
				f.relay.ServeHTTP(httptest.NewRecorder(), request)
			}
			if strings.Count(output.String(), "without trustedProxyCIDRs") != 1 || !strings.Contains(output.String(), "--trusted-proxy") {
				t.Fatalf("warning missing or repeated: %q", output.String())
			}
			output.Reset()
			f.relay.cfg.TrustedProxyCIDRs = []string{"172.30.0.0/24"}
			f.relay.proxyWarning = sync.Once{}
			f.relay.ServeHTTP(httptest.NewRecorder(), request)
			if output.Len() != 0 {
				t.Fatal("warning logged with trusted proxy configured")
			}
		})
	}
}

func TestTrustedDockerProxySeparatesClientLockouts(t *testing.T) {
	a, _ := newTestAuth(t)
	a.cfg.TrustedProxyCIDRs = []string{"172.30.0.0/24"}
	request := func(client string) *httptest.ResponseRecorder {
		r := loginRequest(a, "incorrect", "000000", nil)
		r.RemoteAddr = "172.30.0.1:1234"
		r.Header.Set("X-Forwarded-For", client)
		return runAuth(a, r)
	}
	for i := 0; i < 5; i++ {
		request("203.0.113.10")
	}
	if request("203.0.113.10").Code != 429 {
		t.Fatal("first client was not locked out")
	}
	if request("198.51.100.10").Code != 401 {
		t.Fatal("proxy caused unrelated client prefix to share lockout")
	}
	// A forwarded prefix supplied through an untrusted hop cannot evade lockout.
	for i := 0; i < 5; i++ {
		request("198.51.100.10, 203.0.113.11")
	}
	if request("198.51.100.10, 203.0.113.11").Code != 429 {
		t.Fatal("spoofed forwarded prefix escaped the nearest untrusted hop")
	}
}

func TestApprovalMethodChangesInvalidateSessionsAndDisableSavedTOTP(t *testing.T) {
	a, cfg := newTestAuth(t)
	cookies := loginCookies(t, a)
	cfg.ApprovalMethods = []string{"totp", "host"}
	changed, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	changed.now = a.now
	if changed.AuthorizedBrowser(withCookies(authRequest("GET", "/", ""), cookies)) {
		t.Fatal("changing methods retained browser session")
	}
	if changed.state.LastTOTPCounter != a.state.LastTOTPCounter {
		t.Fatal("changing notifier methods reset authenticator replay protection")
	}
	cfg.ApprovalMethods = []string{"host"}
	disabled, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	disabled.now = a.now
	code := totpCode(a.secret, a.now().Unix()/30+1, 6)
	if got := runAuth(disabled, loginRequest(disabled, syntheticPassword, code, nil)).Code; got != 401 {
		t.Fatalf("disabled saved authenticator accepted login: %d", got)
	}
}
