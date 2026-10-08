package main

import (
	"bytes"
	"context"
	"encoding/base32"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

const syntheticPassword = "Synthetic password 123"

var syntheticHashOnce sync.Once
var syntheticHash string

func authConfig(t *testing.T) Config {
	t.Helper()
	syntheticHashOnce.Do(func() {
		var err error
		syntheticHash, err = HashPassword(syntheticPassword)
		if err != nil {
			panic(err)
		}
	})
	return Config{PublicURL: "https://relay.example.invalid", StateDir: t.TempDir(), AssetsDir: t.TempDir(), Token: strings.Repeat("t", 32), HostGateToken: strings.Repeat("h", 32), AdminToken: strings.Repeat("a", 32), PasswordHash: syntheticHash, TOTPSecret: base32.StdEncoding.WithPadding(base32.NoPadding).EncodeToString([]byte("12345678901234567890")), ApprovalTTLSeconds: 90}
}
func newTestAuth(t *testing.T) (*Auth, Config) {
	t.Helper()
	cfg := authConfig(t)
	a, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	a.now = func() time.Time { return time.Unix(1700000000, 0) }
	return a, cfg
}
func authRequest(method, path, body string) *http.Request {
	r := httptest.NewRequest(method, "https://relay.example.invalid"+path, strings.NewReader(body))
	r.RemoteAddr = "192.0.2.10:1234"
	if method == "POST" {
		r.Header.Set("Origin", "https://relay.example.invalid")
		r.Header.Set("Content-Type", "application/json")
	}
	return r
}
func loginRequest(a *Auth, password, code string, cookies []*http.Cookie) *http.Request {
	data, _ := json.Marshal(map[string]any{"password": password, "totp": code, "remember": true})
	r := authRequest("POST", "/__auth/login", string(data))
	for _, cookie := range cookies {
		r.AddCookie(cookie)
	}
	return r
}
func runAuth(a *Auth, r *http.Request) *httptest.ResponseRecorder {
	w := httptest.NewRecorder()
	if !a.Handle(w, r) {
		w.WriteHeader(200)
	}
	return w
}
func loginCookies(t *testing.T, a *Auth) []*http.Cookie {
	t.Helper()
	code := totpCode(a.secret, a.now().Unix()/30, 6)
	w := runAuth(a, loginRequest(a, syntheticPassword, code, nil))
	if w.Code != 204 {
		t.Fatalf("login status=%d", w.Code)
	}
	return w.Result().Cookies()
}
func withCookies(r *http.Request, cookies []*http.Cookie) *http.Request {
	for _, c := range cookies {
		r.AddCookie(c)
	}
	return r
}

func TestAuthRFC6238VectorsSkewAndReplay(t *testing.T) {
	secret := []byte("12345678901234567890")
	for _, vector := range []struct {
		seconds int64
		want    string
	}{{59, "94287082"}, {1111111109, "07081804"}, {1111111111, "14050471"}, {1234567890, "89005924"}, {2000000000, "69279037"}, {20000000000, "65353130"}} {
		if got := totpCode(secret, vector.seconds/30, 8); got != vector.want {
			t.Fatalf("RFC vector %d failed", vector.seconds)
		}
	}
	now := time.Unix(1700000000, 0)
	step := now.Unix() / 30
	for _, offset := range []int64{-1, 0, 1} {
		counter, ok := matchingTOTP(secret, totpCode(secret, step+offset, 6), now, -1)
		if !ok || counter != step+offset {
			t.Fatal("skew rejected")
		}
		if _, ok := matchingTOTP(secret, totpCode(secret, counter, 6), now, counter); ok {
			t.Fatal("replay accepted")
		}
	}
	for _, code := range []string{"", "12345x", totpCode(secret, step-2, 6), totpCode(secret, step+2, 6)} {
		if _, ok := matchingTOTP(secret, code, now, -1); ok {
			t.Fatal("invalid/outside-skew code accepted")
		}
	}
}
func TestAuthCookiesPersistenceRotationAndRevocation(t *testing.T) {
	a, cfg := newTestAuth(t)
	cookies := loginCookies(t, a)
	if len(cookies) != 2 {
		t.Fatal("need both cookies")
	}
	for _, c := range cookies {
		if !c.Secure || !c.HttpOnly || c.SameSite != http.SameSiteLaxMode || c.Path != "/" || c.Domain != "" || c.MaxAge != 2592000 {
			t.Fatal("unsafe remembered cookie")
		}
	}
	r := withCookies(authRequest("GET", "/__auth/check", ""), cookies)
	if !a.AuthorizedBrowser(r) {
		t.Fatal("valid session rejected")
	}
	restarted, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	restarted.now = a.now
	if !restarted.AuthorizedBrowser(r) {
		t.Fatal("session not durable")
	}
	if w := runAuth(restarted, loginRequest(restarted, syntheticPassword, totpCode(a.secret, a.now().Unix()/30, 6), nil)); w.Code != 401 {
		t.Fatal("restart permitted TOTP replay")
	}
	// Reformatting the same seed is not key rotation and cannot reset replay.
	canonicalCfg := cfg
	canonicalCfg.TOTPSecret = strings.ToLower(cfg.TOTPSecret)
	canonical, err := NewAuth(canonicalCfg)
	if err != nil {
		t.Fatal(err)
	}
	canonical.now = a.now
	if !canonical.AuthorizedBrowser(r) || canonical.state.LastTOTPCounter != a.state.LastTOTPCounter {
		t.Fatal("seed formatting changed factor identity/replay counter")
	}
	data, err := os.ReadFile(filepath.Join(cfg.StateDir, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range cookies {
		if bytes.Contains(data, []byte(c.Value)) {
			t.Fatal("raw cookie persisted")
		}
	}
	forget := withCookies(authRequest("POST", "/__auth/forget-browser", ""), cookies)
	if runAuth(a, forget).Code != 204 || a.AuthorizedBrowser(r) {
		t.Fatal("forget did not revoke")
	}
	rotated, err := HashPassword("Different synthetic password 456")
	if err != nil {
		t.Fatal(err)
	}
	cfg.PasswordHash = rotated
	changed, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	changed.now = a.now
	if changed.AuthorizedBrowser(r) {
		t.Fatal("rotation retained browser")
	}
	if _, ok := matchingTOTP(changed.secret, totpCode(a.secret, a.now().Unix()/30, 6), a.now(), changed.state.LastTOTPCounter); ok {
		t.Fatal("password rotation reset replay protection")
	}
}
func TestAuthConcurrentTOTPIsSingleUse(t *testing.T) {
	a, _ := newTestAuth(t)
	var accepted atomic.Int32
	var wg sync.WaitGroup
	code := totpCode(a.secret, a.now().Unix()/30, 6)
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			w := runAuth(a, loginRequest(a, syntheticPassword, code, nil))
			if w.Code == 204 {
				accepted.Add(1)
			} else if w.Code != 401 && w.Code != 429 {
				t.Errorf("unexpected status %d", w.Code)
			}
		}()
	}
	wg.Wait()
	if accepted.Load() != 1 {
		t.Fatalf("accepted %d replays", accepted.Load())
	}
}
func TestAuthPersistentLockoutAndCSRF(t *testing.T) {
	a, cfg := newTestAuth(t)
	r := loginRequest(a, syntheticPassword, totpCode(a.secret, a.now().Unix()/30, 6), nil)
	r.Header.Del("Origin")
	if runAuth(a, r).Code != 404 {
		t.Fatal("missing Origin accepted")
	}
	for i := 0; i < 5; i++ {
		w := runAuth(a, loginRequest(a, "incorrect", "000000", nil))
		want := 401
		if i == 4 {
			want = 429
		}
		if w.Code != want {
			t.Fatalf("attempt %d status %d", i, w.Code)
		}
	}
	restarted, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	restarted.now = a.now
	if runAuth(restarted, loginRequest(a, syntheticPassword, totpCode(a.secret, a.now().Unix()/30, 6), nil)).Code != 429 {
		t.Fatal("lockout not durable")
	}
	restarted.now = func() time.Time { return time.Unix(1700000000, 0).Add(16 * time.Minute) }
	if runAuth(restarted, loginRequest(restarted, syntheticPassword, totpCode(a.secret, restarted.now().Unix()/30, 6), nil)).Code != 204 {
		t.Fatal("cooldown did not expire")
	}
}
func TestAuthRuntimePasswordRotationSurvivesRestart(t *testing.T) {
	a, cfg := newTestAuth(t)
	cookies := loginCookies(t, a)
	request := authRequest("POST", "/__gate/manage/password", `{"password":"Rotated synthetic password 789"}`)
	request.Header.Set("Authorization", "Bearer "+cfg.AdminToken)
	if runAuth(a, request).Code != 204 {
		t.Fatal("rotation failed")
	}
	if a.AuthorizedBrowser(withCookies(authRequest("GET", "/", ""), cookies)) {
		t.Fatal("old session retained")
	}
	restarted, err := NewAuth(cfg)
	if err != nil {
		t.Fatal(err)
	}
	restarted.now = func() time.Time { return a.now().Add(time.Minute) }
	if runAuth(restarted, loginRequest(restarted, "Rotated synthetic password 789", totpCode(a.secret, restarted.now().Unix()/30, 6), nil)).Code != 204 {
		t.Fatal("effective rotation not durable")
	}
	manage := authRequest("DELETE", "/__gate/manage/sessions", "")
	manage.Header.Set("Authorization", "Bearer "+cfg.AdminToken)
	if runAuth(restarted, manage).Code != 204 {
		t.Fatal("admin revocation failed")
	}
	if len(restarted.state.Gates) != 0 {
		t.Fatal("gate collection retained")
	}
}
func TestAuthFailClosedOnStateWriteAndInvalidHash(t *testing.T) {
	a, _ := newTestAuth(t)
	broken := filepath.Join(t.TempDir(), "file")
	if err := os.WriteFile(broken, []byte("synthetic"), 0600); err != nil {
		t.Fatal(err)
	}
	a.cfg.StateDir = broken
	w := runAuth(a, loginRequest(a, syntheticPassword, totpCode(a.secret, a.now().Unix()/30, 6), nil))
	if w.Code != 503 || len(w.Result().Cookies()) != 0 {
		t.Fatal("failed write issued authentication")
	}
	for _, hash := range []string{"", "$argon2id$v=19$m=999999999,t=3,p=1$bad$bad", syntheticHash + "$extra"} {
		if _, err := parsePasswordHash(hash); err == nil {
			t.Fatal("unbounded/invalid hash accepted")
		}
	}
}
func TestAuthAdmissionAndHostCredentialBoundaries(t *testing.T) {
	a, cfg := newTestAuth(t)
	r := authRequest("GET", "/?transport=wss", "")
	r.Header.Set("Accept", "text/html")
	w := runAuth(a, r)
	if w.Code != 200 || !strings.Contains(w.Body.String(), `id="totp"`) {
		t.Fatal("anonymous TOTP login not immediate")
	}
	if runAuth(a, authRequest("GET", "/receiver.html", "")).Code != 404 {
		t.Fatal("private asset admitted")
	}
	if a.Handle(httptest.NewRecorder(), authRequest("GET", "/bubbleplay.css", "")) {
		t.Fatal("login style not admitted")
	}
	r = authRequest("GET", "/host-ws", "")
	r.Header.Set("Authorization", "Bearer "+cfg.Token)
	if a.AuthorizedHost(r) {
		t.Fatal("registration token passed endpoint gate")
	}
	r.Header.Set("Authorization", "Bearer "+cfg.HostGateToken)
	if !a.AuthorizedHost(r) {
		t.Fatal("host gate token rejected")
	}
	admin := authRequest("GET", "/__gate/manage/sessions", "")
	if runAuth(a, admin).Code != 404 {
		t.Fatal("management exposed")
	}
}

type syntheticNotice struct {
	Actions []struct {
		Body    string            `json:"body"`
		Headers map[string]string `json:"headers"`
	} `json:"actions"`
}

func ntfyTestAuth(t *testing.T) (*Auth, Config, <-chan syntheticNotice, func()) {
	t.Helper()
	notices := make(chan syntheticNotice, 20)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var notice syntheticNotice
		if json.NewDecoder(r.Body).Decode(&notice) != nil {
			w.WriteHeader(400)
			return
		}
		notices <- notice
		w.WriteHeader(204)
	}))
	cfg := authConfig(t)
	cfg.NtfyURL = server.URL
	cfg.NtfyTopic = "synthetic-private"
	cfg.NtfyToken = strings.Repeat("n", 32)
	cfg.AllowInsecureLoopback = true
	a, err := NewAuth(cfg)
	if err != nil {
		server.Close()
		t.Fatal(err)
	}
	return a, cfg, notices, server.Close
}
func heldRequest(a *Auth, path, ip string, ctx context.Context) <-chan *httptest.ResponseRecorder {
	result := make(chan *httptest.ResponseRecorder, 1)
	go func() {
		r := authRequest("GET", path, "").WithContext(ctx)
		r.Header.Set("Accept", "text/html")
		r.RemoteAddr = ip + ":1234"
		result <- runAuth(a, r)
	}()
	return result
}
func waitNotice(t *testing.T, ch <-chan syntheticNotice) syntheticNotice {
	t.Helper()
	select {
	case n := <-ch:
		return n
	case <-time.After(3 * time.Second):
		t.Fatal("no synthetic notification")
		return syntheticNotice{}
	}
}
func waitHeld(t *testing.T, ch <-chan *httptest.ResponseRecorder) *httptest.ResponseRecorder {
	t.Helper()
	select {
	case w := <-ch:
		return w
	case <-time.After(3 * time.Second):
		t.Fatal("held navigation not released")
		return nil
	}
}
func decideNotice(a *Auth, notice syntheticNotice, index int, body string) *httptest.ResponseRecorder {
	r := authRequest("POST", "/__gate/decision", body)
	r.Header.Set("Authorization", notice.Actions[index].Headers["Authorization"])
	r.Header.Del("Origin")
	return runAuth(a, r)
}
func TestAuthNtfyCapabilitiesBoundToHeldNavigationAndSingleUse(t *testing.T) {
	a, _, notices, close := ntfyTestAuth(t)
	defer close()
	held := heldRequest(a, "/?approval=ntfy&transport=wss", "192.0.2.10", context.Background())
	notice := waitNotice(t, notices)
	if len(notice.Actions) != 2 {
		t.Fatal("missing decision capabilities")
	}
	if notice.Actions[0].Headers["Authorization"] == notice.Actions[1].Headers["Authorization"] {
		t.Fatal("capabilities reused")
	}
	if decideNotice(a, notice, 0, "deny").Code != 204 {
		t.Fatal("bad decision response")
	}
	select {
	case <-held:
		t.Fatal("wrong action released navigation")
	default:
	}
	if runAuth(a, authRequest("GET", "/__gate/check", "")).Code != 404 {
		t.Fatal("different navigation approved")
	}
	decideNotice(a, notice, 0, "approve")
	w := waitHeld(t, held)
	if w.Code != 200 || strings.Contains(w.Body.String(), `id="totp"`) {
		t.Fatal("approved navigation did not get password-only form")
	}
	cookies := w.Result().Cookies()
	if len(cookies) != 1 || cookies[0].Name != gateCookieName {
		t.Fatal("wrong gate cookie")
	}
	count := len(a.state.Gates)
	decideNotice(a, notice, 0, "approve")
	if len(a.state.Gates) != count {
		t.Fatal("decision replay accepted")
	}
	if runAuth(a, loginRequest(a, syntheticPassword, "", cookies)).Code != 204 {
		t.Fatal("approved browser password login failed")
	}
}
func TestAuthNtfyDenyExpiryDisconnectAndBounds(t *testing.T) {
	a, _, notices, close := ntfyTestAuth(t)
	defer close()
	a.approvalTTL = 100 * time.Millisecond
	held := heldRequest(a, "/?approval=ntfy", "192.0.2.10", context.Background())
	notice := waitNotice(t, notices)
	decideNotice(a, notice, 1, "deny")
	if waitHeld(t, held).Code != 404 {
		t.Fatal("denial did not hide")
	}
	held = heldRequest(a, "/?approval=ntfy", "198.51.100.10", context.Background())
	notice = waitNotice(t, notices)
	if waitHeld(t, held).Code != 404 {
		t.Fatal("expiry did not hide")
	}
	before := len(a.state.Gates)
	decideNotice(a, notice, 0, "approve")
	if len(a.state.Gates) != before {
		t.Fatal("expired capability accepted")
	}
	a.approvalTTL = time.Second
	ctx, cancel := context.WithCancel(context.Background())
	held = heldRequest(a, "/?approval=ntfy", "203.0.113.10", ctx)
	notice = waitNotice(t, notices)
	cancel()
	if waitHeld(t, held).Code != 404 {
		t.Fatal("disconnect did not hide")
	}
	decideNotice(a, notice, 0, "approve")
	if len(a.state.Gates) != before {
		t.Fatal("disconnected capability accepted")
	}
	a.approvalTTL = 2 * time.Second
	contexts := make([]context.CancelFunc, 0, 10)
	results := make([]<-chan *httptest.ResponseRecorder, 0, 10)
	for i := 0; i < 10; i++ {
		ctx, cancel := context.WithCancel(context.Background())
		contexts = append(contexts, cancel)
		results = append(results, heldRequest(a, "/?approval=ntfy", fmtIP(i), ctx))
		waitNotice(t, notices)
	}
	if waitHeld(t, heldRequest(a, "/?approval=ntfy", "10.99.0.10", context.Background())).Code != 404 {
		t.Fatal("pending global cap exceeded")
	}
	if waitHeld(t, heldRequest(a, "/?approval=ntfy", fmtIP(0), context.Background())).Code != 404 {
		t.Fatal("prefix has duplicate pending navigation")
	}
	for _, cancel := range contexts {
		cancel()
	}
	for _, result := range results {
		waitHeld(t, result)
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if len(a.pending) != 0 || len(a.capabilities) != 0 {
		t.Fatal("pending capabilities leaked")
	}
}
func fmtIP(i int) string { return "10." + strconv.Itoa(i) + ".0.10" }

func TestAuthCookiePairExpiryAndShutdown(t *testing.T) {
	a, _ := newTestAuth(t)
	first := loginCookies(t, a)
	a.now = func() time.Time { return time.Unix(1700000060, 0) }
	second := loginCookies(t, a)
	mixed := []*http.Cookie{first[0], second[1]}
	if a.AuthorizedBrowser(withCookies(authRequest("GET", "/", ""), mixed)) {
		t.Fatal("session accepted with another gate cookie")
	}
	a.now = func() time.Time { return time.Unix(1700000060, 0).Add(31 * 24 * time.Hour) }
	if a.AuthorizedBrowser(withCookies(authRequest("GET", "/", ""), second)) {
		t.Fatal("expired remembered browser accepted")
	}
	a, _, notices, close := ntfyTestAuth(t)
	defer close()
	held := heldRequest(a, "/?approval=ntfy", "192.0.2.10", context.Background())
	_ = waitNotice(t, notices)
	a.Close()
	if waitHeld(t, held).Code != 404 {
		t.Fatal("shutdown did not release pending navigation")
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.healthy || len(a.pending) != 0 || len(a.capabilities) != 0 {
		t.Fatal("shutdown retained approval/admission state")
	}
}
