package main

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base32"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"html"
	"io"
	"mime"
	"net/http"
	"net/netip"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"golang.org/x/crypto/argon2"
)

const (
	gateCookieName     = "__Host-gate"
	appCookieName      = "__Host-bp-session"
	shortSessionTTL    = 12 * time.Hour
	rememberSessionTTL = 30 * 24 * time.Hour
	authRateWindow     = 10 * time.Minute
	authLockout        = 15 * time.Minute
	maxAuthRecords     = 4096
)

type passwordPHC struct{ salt, digest []byte }

// HashPassword is shared with the provisioning CLI. No plaintext is persisted.
func HashPassword(password string) (string, error) {
	if !validNewPassword(password) {
		return "", errors.New("password must contain 12-256 characters")
	}
	salt := make([]byte, 16)
	if _, err := rand.Read(salt); err != nil {
		return "", err
	}
	digest := argon2.IDKey([]byte(password), salt, 3, 64*1024, 1, 32)
	return "$argon2id$v=19$m=65536,t=3,p=1$" + base64.RawStdEncoding.EncodeToString(salt) + "$" + base64.RawStdEncoding.EncodeToString(digest), nil
}
func validNewPassword(password string) bool {
	n := utf8.RuneCountInString(password)
	return n >= 12 && n <= 256 && strings.TrimSpace(password) != "" && utf8.ValidString(password)
}
func parsePasswordHash(encoded string) (passwordPHC, error) {
	parts := strings.Split(encoded, "$")
	if len(parts) != 6 || parts[0] != "" || parts[1] != "argon2id" || parts[2] != "v=19" || parts[3] != "m=65536,t=3,p=1" {
		return passwordPHC{}, errors.New("password hash must use the provisioning CLI Argon2id format")
	}
	salt, e1 := base64.RawStdEncoding.DecodeString(parts[4])
	digest, e2 := base64.RawStdEncoding.DecodeString(parts[5])
	if e1 != nil || e2 != nil || len(salt) != 16 || len(digest) != 32 {
		return passwordPHC{}, errors.New("invalid password hash")
	}
	return passwordPHC{salt, digest}, nil
}
func (p passwordPHC) verify(password string) bool {
	if len(password) == 0 || len(password) > 1024 || !utf8.ValidString(password) {
		return false
	}
	return subtle.ConstantTimeCompare(argon2.IDKey([]byte(password), p.salt, 3, 64*1024, 1, 32), p.digest) == 1
}
func tokenHash(value string) string {
	sum := sha256.Sum256([]byte(value))
	return hex.EncodeToString(sum[:])
}
func randomCapability() (string, error) {
	value := make([]byte, 32)
	if _, err := rand.Read(value); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(value), nil
}
func authBearer(request *http.Request, secret string) bool {
	header := request.Header.Get("Authorization")
	if len(secret) < 32 || !strings.HasPrefix(header, "Bearer ") || len(header) > 1024 {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(tokenHash(strings.TrimPrefix(header, "Bearer "))), []byte(tokenHash(secret))) == 1
}
func cookieHash(request *http.Request, name string) string {
	cookie, err := request.Cookie(name)
	if err != nil || len(cookie.Value) != 43 {
		return ""
	}
	decoded, err := base64.RawURLEncoding.DecodeString(cookie.Value)
	if err != nil || len(decoded) != 32 {
		return ""
	}
	return tokenHash(cookie.Value)
}

// TOTP follows RFC 6238: SHA1, 30-second steps, six decimal digits for apps.
// The primitive permits eight digits for the RFC's published test vectors.
func totpCode(secret []byte, counter int64, digits int) string {
	var input [8]byte
	binary.BigEndian.PutUint64(input[:], uint64(counter))
	mac := hmac.New(sha1.New, secret)
	_, _ = mac.Write(input[:])
	sum := mac.Sum(nil)
	offset := sum[len(sum)-1] & 15
	value := binary.BigEndian.Uint32(sum[offset:offset+4]) & 0x7fffffff
	mod := uint32(1)
	for i := 0; i < digits; i++ {
		mod *= 10
	}
	return fmt.Sprintf("%0*d", digits, value%mod)
}
func matchingTOTP(secret []byte, code string, now time.Time, last int64) (int64, bool) {
	if len(code) != 6 {
		return 0, false
	}
	for _, c := range code {
		if c < '0' || c > '9' {
			return 0, false
		}
	}
	step := now.Unix() / 30
	for _, delta := range []int64{0, -1, 1} {
		counter := step + delta
		if counter >= 0 && counter > last && subtle.ConstantTimeCompare([]byte(totpCode(secret, counter, 6)), []byte(code)) == 1 {
			return counter, true
		}
	}
	return 0, false
}

type authSession struct {
	ID          string `json:"id"`
	CreatedAt   int64  `json:"createdAt"`
	LastSeenAt  int64  `json:"lastSeenAt"`
	ExpiresAt   int64  `json:"expiresAt"`
	Remembered  bool   `json:"remembered"`
	GateHash    string `json:"gateHash,omitempty"`
	PasswordTag string `json:"passwordTag,omitempty"`
}
type authRate struct {
	StartedAt   int64 `json:"startedAt"`
	Failures    int   `json:"failures"`
	LockedUntil int64 `json:"lockedUntil"`
}

// pcRecord is one host PC registered with this relay. Its map key is the
// canonical device ID; FirstSeen and LastSeen are Unix milliseconds, like sessions.
type pcRecord struct {
	Public    string `json:"public"`
	FirstSeen int64  `json:"firstSeen"`
	LastSeen  int64  `json:"lastSeen"`
}
type authState struct {
	Version                int                    `json:"version"`
	ConfiguredPasswordHash string                 `json:"configuredPasswordHash"`
	PasswordHash           string                 `json:"passwordHash"`
	FactorTag              string                 `json:"factorTag"`
	TOTPTag                string                 `json:"totpTag"`
	LastTOTPCounter        int64                  `json:"lastTotpCounter"`
	Gates                  map[string]authSession `json:"gates"`
	Sessions               map[string]authSession `json:"sessions"`
	Rates                  map[string]authRate    `json:"rates"`
	PCs                    map[string]pcRecord    `json:"pcs"`
}

// validDurablePCs accepts a PC table only when every key is the canonical device
// ID of its own unpadded base64url 32-byte public key. A nil table is empty, so
// version 1 files written before PCs existed still load.
func validDurablePCs(pcs map[string]pcRecord) bool {
	if len(pcs) > maxAuthRecords {
		return false
	}
	for id, record := range pcs {
		canonical, ok := canonicalDeviceID(id)
		if !ok || canonical != id || record.FirstSeen <= 0 || record.LastSeen < record.FirstSeen {
			return false
		}
		public, err := base64.RawURLEncoding.DecodeString(record.Public)
		if err != nil || len(public) != 32 || base64.RawURLEncoding.EncodeToString(public) != record.Public || deviceID(public) != id {
			return false
		}
	}
	return true
}

type pendingApproval struct {
	id, prefix, approveHash, denyHash, navigation string
	expires                                       time.Time
	result                                        chan string // Only the held navigation receives its new gate token.
	cancel                                        context.CancelFunc
	host                                          *peer
	context                                       context.Context
}

// Auth's JSON state belongs to one server process. Run a single replica for a
// StateDir; a mutex gives concurrent requests atomic replay/session updates.
type Auth struct {
	mu           sync.Mutex
	cfg          Config
	state        authState
	password     passwordPHC
	secret       []byte
	healthy      bool
	pending      map[string]*pendingApproval
	capabilities map[string]*pendingApproval
	cooldown     map[string]time.Time
	loginSlots   chan struct{}
	client       *http.Client
	now          func() time.Time
	approvalTTL  time.Duration
	hostOnline   func() bool
	requestHost  func(*pendingApproval, string) bool
}

func NewAuth(cfg Config) (*Auth, error) {
	if err := cfg.validateApprovalConfig(); err != nil {
		return nil, err
	}
	password, err := parsePasswordHash(cfg.PasswordHash)
	if err != nil {
		return nil, err
	}
	if len(cfg.HostGateToken) < 32 || len(cfg.AdminToken) < 32 || cfg.StateDir == "" {
		return nil, errors.New("authentication credentials and state directory are required")
	}
	var secret []byte
	if cfg.methodEnabled("totp") {
		secret, err = base32.StdEncoding.WithPadding(base32.NoPadding).DecodeString(strings.ToUpper(strings.TrimRight(cfg.TOTPSecret, "=")))
		if err != nil || len(secret) < 20 || len(secret) > 64 {
			return nil, errors.New("TOTP secret must encode 20-64 bytes in base32")
		}
	}
	ttl := cfg.ApprovalTTLSeconds
	if ttl == 0 {
		ttl = 90
	}
	if ttl < 30 || ttl > 120 {
		return nil, errors.New("approval TTL must be 30-120 seconds")
	}
	if err := os.MkdirAll(cfg.StateDir, 0700); err != nil {
		return nil, err
	}
	a := &Auth{cfg: cfg, password: password, secret: secret, healthy: true, pending: make(map[string]*pendingApproval), capabilities: make(map[string]*pendingApproval), cooldown: make(map[string]time.Time), loginSlots: make(chan struct{}, 2), client: &http.Client{Timeout: 10 * time.Second}, now: time.Now, approvalTTL: time.Duration(ttl) * time.Second}
	a.state = authState{Version: 1, ConfiguredPasswordHash: cfg.PasswordHash, PasswordHash: cfg.PasswordHash, FactorTag: tokenHash(cfg.AdminToken + "\x00" + cfg.HostGateToken + "\x00" + string(secret)), TOTPTag: tokenHash(string(secret)), LastTOTPCounter: -1, Gates: make(map[string]authSession), Sessions: make(map[string]authSession), Rates: make(map[string]authRate), PCs: make(map[string]pcRecord)}
	if cfg.ApprovalMethods != nil {
		// Preserve the legacy factor identity until methods are explicitly configured.
		a.state.FactorTag = tokenHash(a.state.FactorTag + "\x00" + strings.Join(cfg.approvalNotifiers(), ",") + "\x00" + strconv.FormatBool(cfg.methodEnabled("totp")))
	}
	path := filepath.Join(cfg.StateDir, "auth.json")
	if file, err := os.Open(path); err == nil {
		decoder := json.NewDecoder(io.LimitReader(file, 8<<20))
		decoder.DisallowUnknownFields()
		var stored authState
		err = decoder.Decode(&stored)
		var extra any
		end := decoder.Decode(&extra)
		_ = file.Close()
		if err != nil || end != io.EOF || stored.Version != 1 || stored.Gates == nil || stored.Sessions == nil || stored.Rates == nil || len(stored.Gates) > maxAuthRecords || len(stored.Sessions) > maxAuthRecords || len(stored.Rates) > maxAuthRecords || !validDurablePCs(stored.PCs) {
			return nil, errors.New("invalid durable authentication state")
		}
		if stored.ConfiguredPasswordHash == cfg.PasswordHash {
			a.state.PasswordHash = stored.PasswordHash
			a.password, err = parsePasswordHash(stored.PasswordHash)
			if err != nil {
				return nil, err
			}
		}
		if stored.TOTPTag == a.state.TOTPTag {
			a.state.LastTOTPCounter = stored.LastTOTPCounter
		}
		if stored.ConfiguredPasswordHash == cfg.PasswordHash && stored.FactorTag == a.state.FactorTag {
			a.state.Gates = stored.Gates
			a.state.Sessions = stored.Sessions
			a.state.Rates = stored.Rates
		}
		// PCs are registrations, not session state: they survive password and factor changes.
		if stored.PCs != nil {
			a.state.PCs = stored.PCs
		}
	} else if !os.IsNotExist(err) {
		return nil, err
	}
	if err := a.persistLocked(); err != nil {
		return nil, err
	}
	return a, nil
}

// persistLocked commits before cookies/replay acceptance become externally
// visible. An I/O error disables authentication until the service is repaired.
func (a *Auth) persistLocked() error {
	data, err := json.Marshal(a.state)
	if err != nil {
		a.healthy = false
		return err
	}
	file, err := os.CreateTemp(a.cfg.StateDir, ".auth-*.tmp")
	if err != nil {
		a.healthy = false
		return err
	}
	name := file.Name()
	defer os.Remove(name)
	if err = file.Chmod(0600); err == nil {
		_, err = file.Write(data)
	}
	if err == nil {
		err = file.Sync()
	}
	closeErr := file.Close()
	if err == nil {
		err = closeErr
	}
	if err == nil {
		err = os.Rename(name, filepath.Join(a.cfg.StateDir, "auth.json"))
	}
	if err == nil && runtime.GOOS != "windows" {
		dir, e := os.Open(a.cfg.StateDir)
		if e == nil {
			e = dir.Sync()
			_ = dir.Close()
		}
		err = e
	}
	if err != nil {
		a.healthy = false
	}
	return err
}
func (a *Auth) pruneLocked(now int64) {
	for key, record := range a.state.Gates {
		if record.ExpiresAt <= now {
			delete(a.state.Gates, key)
		}
	}
	for key, record := range a.state.Sessions {
		if record.ExpiresAt <= now {
			delete(a.state.Sessions, key)
		}
	}
	for key, record := range a.state.Rates {
		if record.LockedUntil <= now && record.StartedAt+int64(authRateWindow/time.Millisecond) <= now {
			delete(a.state.Rates, key)
		}
	}
}
func (a *Auth) gateLocked(request *http.Request, now int64) (string, bool) {
	key := cookieHash(request, gateCookieName)
	record, found := a.state.Gates[key]
	return key, a.healthy && found && record.ExpiresAt > now
}
func (a *Auth) browserLocked(request *http.Request, now int64) bool {
	gate, ok := a.gateLocked(request, now)
	if !ok {
		return false
	}
	session, found := a.state.Sessions[cookieHash(request, appCookieName)]
	return found && session.ExpiresAt > now && session.GateHash == gate && session.PasswordTag == tokenHash(a.state.PasswordHash)
}
func (a *Auth) AuthorizedBrowser(request *http.Request) bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.browserLocked(request, a.now().UnixMilli())
}
func (a *Auth) AuthorizedHost(request *http.Request) bool {
	return authBearer(request, a.cfg.HostGateToken)
}

func emptyAuth(w http.ResponseWriter, status int) {
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(status)
}
func hiddenAuth(w http.ResponseWriter) {
	w.Header().Set("Cache-Control", "no-store")
	http.Error(w, "Not found", http.StatusNotFound)
}
func authJSON(w http.ResponseWriter, value any) {
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(value)
}
func setAuthCookie(w http.ResponseWriter, name, token string, remember bool, ttl time.Duration) {
	cookie := &http.Cookie{Name: name, Value: token, Path: "/", Secure: true, HttpOnly: true, SameSite: http.SameSiteLaxMode}
	if remember {
		cookie.MaxAge = int(ttl / time.Second)
	}
	http.SetCookie(w, cookie)
}
func expireAuthCookie(w http.ResponseWriter, name string) {
	http.SetCookie(w, &http.Cookie{Name: name, Value: "", Path: "/", Secure: true, HttpOnly: true, SameSite: http.SameSiteLaxMode, MaxAge: -1})
}
func decodeAuthJSON(w http.ResponseWriter, request *http.Request, value any) bool {
	typeName, _, err := mime.ParseMediaType(request.Header.Get("Content-Type"))
	if err != nil || typeName != "application/json" {
		return false
	}
	decoder := json.NewDecoder(http.MaxBytesReader(w, request.Body, 4096))
	decoder.DisallowUnknownFields()
	if decoder.Decode(value) != nil {
		return false
	}
	var extra any
	return decoder.Decode(&extra) == io.EOF
}
func authPrefix(ip string) string {
	address, err := netip.ParseAddr(ip)
	if err != nil {
		return "unknown"
	}
	address = address.Unmap()
	bits := 56
	if address.Is4() {
		bits = 24
	}
	return netip.PrefixFrom(address, bits).Masked().String()
}
func initialAuthNavigation(request *http.Request) bool {
	return request.Method == "GET" && request.URL.Path == "/" && strings.Contains(strings.ToLower(request.Header.Get("Accept")), "text/html") && (request.Header.Get("Sec-Fetch-Mode") == "" || request.Header.Get("Sec-Fetch-Mode") == "navigate") && (request.Header.Get("Sec-Fetch-Dest") == "" || request.Header.Get("Sec-Fetch-Dest") == "document")
}
func publicLoginAsset(path string) bool {
	return path == "/favicon.svg" || path == "/favicon-32.png" || path == "/bubbleplay.css" || path == "/fonts/atkinson-hyperlegible-next.woff2" || path == "/fonts/bricolage-grotesque.woff2" || path == "/fonts/fragment-mono.woff2"
}

// Handle returns false only when the caller should serve admitted assets.
func (a *Auth) Handle(w http.ResponseWriter, request *http.Request) bool {
	path := request.URL.Path
	switch {
	case path == "/__auth/methods":
		if request.Method != "GET" {
			hiddenAuth(w)
			return true
		}
		online := a.hostOnline != nil && a.hostOnline()
		authJSON(w, map[string]any{"totp": a.cfg.methodEnabled("totp"), "approval": a.cfg.approvalNotifiers(), "hostOnline": online, "sourceURL": a.cfg.sourceURL()})
		return true
	case path == "/__auth/login":
		a.login(w, request)
		return true
	case path == "/__auth/logout" || path == "/__auth/forget-browser":
		a.logout(w, request, path == "/__auth/forget-browser")
		return true
	case path == "/__gate/decision":
		a.decide(w, request)
		return true
	case strings.HasPrefix(path, "/__gate/manage/"):
		a.manage(w, request)
		return true
	case path == "/__auth/check" || path == "/__gate/check":
		a.mu.Lock()
		_, gate := a.gateLocked(request, a.now().UnixMilli())
		ok := gate
		if path == "/__auth/check" {
			ok = a.browserLocked(request, a.now().UnixMilli())
		}
		a.mu.Unlock()
		if request.Method == "GET" && ok {
			emptyAuth(w, 204)
		} else {
			hiddenAuth(w)
		}
		return true
	case strings.HasPrefix(path, "/__auth/") || strings.HasPrefix(path, "/__gate/"):
		hiddenAuth(w)
		return true
	}
	if a.AuthorizedBrowser(request) {
		return false
	}
	if request.Method == "GET" && publicLoginAsset(path) {
		return false
	}
	if !initialAuthNavigation(request) {
		hiddenAuth(w)
		return true
	}
	a.mu.Lock()
	_, gate := a.gateLocked(request, a.now().UnixMilli())
	healthy := a.healthy
	a.mu.Unlock()
	if !healthy {
		emptyAuth(w, 503)
		return true
	}
	approval := request.URL.Query().Get("approval")
	if gate || a.cfg.methodEnabled("totp") && approval != "ntfy" && approval != "request" {
		a.serveLogin(w, request, gate)
		return true
	}
	a.holdApproval(w, request)
	return true
}

func (a *Auth) login(w http.ResponseWriter, request *http.Request) {
	if request.Method != "POST" || !SameOrigin(request, a.cfg, false) {
		hiddenAuth(w)
		return
	}
	var payload struct {
		Password string `json:"password"`
		TOTP     string `json:"totp"`
		Remember bool   `json:"remember"`
	}
	parsed := decodeAuthJSON(w, request, &payload)
	select {
	case a.loginSlots <- struct{}{}:
		defer func() { <-a.loginSlots }()
	default:
		emptyAuth(w, 429)
		return
	}
	a.mu.Lock()
	now := a.now().UnixMilli()
	a.pruneLocked(now)
	if !a.healthy {
		a.mu.Unlock()
		emptyAuth(w, 503)
		return
	}
	key := tokenHash(authPrefix(ClientIP(request, a.cfg)))
	rate := a.state.Rates[key]
	if rate.LockedUntil > now {
		a.mu.Unlock()
		w.Header().Set("Retry-After", strconv.FormatInt((rate.LockedUntil-now+999)/1000, 10))
		emptyAuth(w, 429)
		return
	}
	if rate.StartedAt+int64(authRateWindow/time.Millisecond) <= now {
		rate = authRate{StartedAt: now}
	}
	if _, found := a.state.Rates[key]; !found && len(a.state.Rates) >= maxAuthRecords {
		a.mu.Unlock()
		emptyAuth(w, 429)
		return
	}
	// Reserve/count before the expensive hash; concurrent failures cannot evade
	// the shared prefix lockout. A successful complete login clears its record.
	rate.Failures++
	if rate.Failures >= 5 {
		rate.LockedUntil = now + int64(authLockout/time.Millisecond)
	}
	a.state.Rates[key] = rate
	password := a.password
	passwordTag := tokenHash(a.state.PasswordHash)
	if a.persistLocked() != nil {
		a.mu.Unlock()
		emptyAuth(w, 503)
		return
	}
	a.mu.Unlock()
	passwordOK := parsed && password.verify(payload.Password)
	a.mu.Lock()
	defer a.mu.Unlock()
	now = a.now().UnixMilli()
	gateHash, gateOK := a.gateLocked(request, now)
	counter, totpOK := matchingTOTP(a.secret, payload.TOTP, a.now(), a.state.LastTOTPCounter)
	totpOK = totpOK && len(a.secret) > 0 && a.cfg.methodEnabled("totp")
	if !a.healthy {
		emptyAuth(w, 503)
		return
	}
	if !passwordOK || passwordTag != tokenHash(a.state.PasswordHash) || !gateOK && !totpOK {
		if a.state.Rates[key].LockedUntil > now {
			emptyAuth(w, 429)
		} else {
			emptyAuth(w, 401)
		}
		return
	}
	if len(a.state.Gates) >= maxAuthRecords || len(a.state.Sessions) >= maxAuthRecords {
		emptyAuth(w, 429)
		return
	}
	gateToken, err := randomCapability()
	if err != nil {
		emptyAuth(w, 503)
		return
	}
	sessionToken, err := randomCapability()
	if err != nil {
		emptyAuth(w, 503)
		return
	}
	ttl := shortSessionTTL
	if payload.Remember {
		ttl = rememberSessionTTL
	}
	// Rotate both tokens on successful login; never adopt a caller's cookie.
	if gateOK {
		delete(a.state.Gates, gateHash)
		for key, session := range a.state.Sessions {
			if session.GateHash == gateHash {
				delete(a.state.Sessions, key)
			}
		}
	} else {
		a.state.LastTOTPCounter = counter
	}
	gateHash = tokenHash(gateToken)
	a.state.Gates[gateHash] = authSession{ID: gateHash[:24], CreatedAt: now, LastSeenAt: now, ExpiresAt: now + ttl.Milliseconds(), Remembered: payload.Remember}
	a.state.Sessions[tokenHash(sessionToken)] = authSession{ID: tokenHash(sessionToken)[:24], CreatedAt: now, LastSeenAt: now, ExpiresAt: now + ttl.Milliseconds(), Remembered: payload.Remember, GateHash: gateHash, PasswordTag: passwordTag}
	delete(a.state.Rates, key)
	if a.persistLocked() != nil {
		emptyAuth(w, 503)
		return
	}
	setAuthCookie(w, gateCookieName, gateToken, payload.Remember, ttl)
	setAuthCookie(w, appCookieName, sessionToken, payload.Remember, ttl)
	emptyAuth(w, 204)
}
func (a *Auth) logout(w http.ResponseWriter, request *http.Request, forget bool) {
	if request.Method != "POST" || !SameOrigin(request, a.cfg, false) {
		hiddenAuth(w)
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if !a.browserLocked(request, a.now().UnixMilli()) {
		hiddenAuth(w)
		return
	}
	delete(a.state.Sessions, cookieHash(request, appCookieName))
	if forget {
		delete(a.state.Gates, cookieHash(request, gateCookieName))
	}
	if a.persistLocked() != nil {
		emptyAuth(w, 503)
		return
	}
	expireAuthCookie(w, appCookieName)
	if forget {
		expireAuthCookie(w, gateCookieName)
	}
	emptyAuth(w, 204)
}

func (a *Auth) serveLogin(w http.ResponseWriter, request *http.Request, gate bool) {
	nonce, err := randomCapability()
	if err != nil {
		emptyAuth(w, 503)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("X-Frame-Options", "DENY")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Content-Security-Policy", "default-src 'none'; img-src 'self'; style-src 'self' 'nonce-"+nonce+"'; font-src 'self'; script-src 'nonce-"+nonce+"'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'")
	factor := ""
	if !gate {
		factor = `<label class="field-label totp-label" for="totp">Authenticator code</label><input id="totp" name="totp" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]{6}" maxlength="6" required>`
	}
	approval := ""
	if !gate && len(a.cfg.approvalNotifiers()) > 0 {
		query := request.URL.Query()
		query.Set("approval", "request")
		where := "on your PC"
		if a.cfg.methodEnabled("ntfy") {
			where = "by notification"
			if a.cfg.methodEnabled("host") {
				where = "on your PC and by notification"
			}
		}
		approval = `<p><a href="/?` + html.EscapeString(query.Encode()) + `">Request approval instead (` + where + `)</a></p>`
	}
	page := strings.NewReplacer("{{FACTOR}}", factor, "{{APPROVAL}}", approval, "{{NONCE}}", nonce).Replace(relayLoginPage)
	if !gate {
		page = strings.Replace(page, "Browser approved", "Private access", 1)
		page = strings.Replace(page, "This browser has passed the private approval gate. The application password is the final boundary before control begins.", "Use your password and authenticator code to open a private remote session.", 1)
		page = strings.Replace(page, "Enter the BubblePlay password to open a private remote session.", "Enter your password and the six-digit code from your authenticator.", 1)
	}
	_, _ = io.WriteString(w, page)
}

func (a *Auth) detachPendingLocked(p *pendingApproval) {
	delete(a.pending, p.id)
	delete(a.capabilities, p.approveHash)
	delete(a.capabilities, p.denyHash)
}
func (a *Auth) cancelHostLocked(p *pendingApproval, source *peer) {
	if p.host != nil && p.host != source {
		p.host.json(map[string]any{"type": "approval-cancel", "id": p.id})
	}
}
func (a *Auth) holdApproval(w http.ResponseWriter, request *http.Request) {
	if len(a.cfg.approvalNotifiers()) == 0 {
		hiddenAuth(w)
		return
	}
	if !a.cfg.methodEnabled("ntfy") && (a.hostOnline == nil || !a.hostOnline()) {
		a.hostUnavailable(w)
		return
	}
	id, e := randomCapability()
	if e != nil {
		emptyAuth(w, 503)
		return
	}
	approve, e := randomCapability()
	if e != nil {
		emptyAuth(w, 503)
		return
	}
	deny, e := randomCapability()
	if e != nil {
		emptyAuth(w, 503)
		return
	}
	ctx, cancel := context.WithTimeout(request.Context(), a.approvalTTL)
	defer cancel()
	a.mu.Lock()
	now := a.now()
	prefix := authPrefix(ClientIP(request, a.cfg))
	for key, until := range a.cooldown {
		if !until.After(now) {
			delete(a.cooldown, key)
		}
	}
	busy := false
	for _, pending := range a.pending {
		if pending.prefix == prefix {
			busy = true
		}
	}
	if !a.healthy || busy || len(a.pending) >= 10 || len(a.cooldown) >= maxAuthRecords || a.cooldown[prefix].After(now) {
		a.mu.Unlock()
		hiddenAuth(w)
		return
	}
	p := &pendingApproval{id: id, prefix: prefix, approveHash: tokenHash(approve), denyHash: tokenHash(deny), navigation: request.URL.RequestURI(), expires: now.Add(a.approvalTTL), result: make(chan string, 1), cancel: cancel, context: ctx}
	a.pending[id] = p
	if a.cfg.methodEnabled("ntfy") {
		a.capabilities[p.approveHash] = p
		a.capabilities[p.denyHash] = p
	}
	a.mu.Unlock()
	defer func() {
		a.mu.Lock()
		if a.pending[p.id] == p {
			a.detachPendingLocked(p)
			a.cancelHostLocked(p, nil)
		}
		a.mu.Unlock()
	}()
	hostSent := a.cfg.methodEnabled("host") && a.requestHost != nil && a.requestHost(p, browserAgent(request.UserAgent()))
	if !hostSent && !a.cfg.methodEnabled("ntfy") {
		a.hostUnavailable(w)
		return
	}
	published := make(chan error, 1)
	if a.cfg.methodEnabled("ntfy") {
		go func() { published <- a.publishApproval(ctx, request, approve, deny) }()
	}
	for {
		select {
		case token := <-p.result:
			if token == "" {
				hiddenAuth(w)
				return
			}
			setAuthCookie(w, gateCookieName, token, false, shortSessionTTL)
			a.serveLogin(w, request, true)
			return
		case err := <-published:
			if err == nil || hostSent {
				published = nil
				continue
			}
			// A decision can arrive while its notification response is in flight.
			a.mu.Lock()
			decided := a.pending[p.id] != p
			if !decided {
				a.cooldown[prefix] = a.now().Add(15 * time.Second)
				a.detachPendingLocked(p)
			}
			a.mu.Unlock()
			if decided {
				published = nil
				continue
			}
			hiddenAuth(w)
			return
		case <-ctx.Done():
			a.mu.Lock()
			a.cooldown[prefix] = a.now().Add(15 * time.Second)
			a.mu.Unlock()
			hiddenAuth(w)
			return
		}
	}
}
func (a *Auth) hostUnavailable(w http.ResponseWriter) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'; base-uri 'none'")
	w.WriteHeader(http.StatusServiceUnavailable)
	_, _ = io.WriteString(w, "<!doctype html><title>PC approval unavailable</title><h1>Your PC is offline or cannot receive browser approval</h1><p>Connect a host with browser approval enabled, then retry.</p>")
}
func (a *Auth) publishApproval(ctx context.Context, request *http.Request, approve, deny string) error {
	action := func(label, token, body string) map[string]any {
		return map[string]any{"action": "http", "label": label, "url": strings.TrimRight(a.cfg.PublicURL, "/") + "/__gate/decision", "method": "POST", "headers": map[string]string{"Authorization": "Bearer " + token}, "body": body, "clear": true}
	}
	body := map[string]any{"topic": a.cfg.NtfyTopic, "title": "BubblePlay access request", "message": "Browser navigation awaiting approval from " + ClientIP(request, a.cfg), "tags": []string{"lock"}, "actions": []any{action("Approve", approve, "approve"), action("Deny", deny, "deny")}}
	data, e := json.Marshal(body)
	if e != nil {
		return e
	}
	notification, e := http.NewRequestWithContext(ctx, "POST", a.cfg.NtfyURL, bytes.NewReader(data))
	if e != nil {
		return e
	}
	notification.Header.Set("Authorization", "Bearer "+a.cfg.NtfyToken)
	notification.Header.Set("Content-Type", "application/json")
	response, e := a.client.Do(notification)
	if e != nil {
		return errors.New("ntfy request failed")
	}
	defer response.Body.Close()
	_, _ = io.Copy(io.Discard, io.LimitReader(response.Body, 4096))
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		return errors.New("ntfy rejected notification")
	}
	return nil
}
func (a *Auth) decide(w http.ResponseWriter, request *http.Request) {
	if request.Method != "POST" {
		hiddenAuth(w)
		return
	}
	token := strings.TrimPrefix(request.Header.Get("Authorization"), "Bearer ")
	decoded, e := base64.RawURLEncoding.DecodeString(token)
	if e != nil || len(decoded) != 32 {
		emptyAuth(w, 204)
		return
	}
	body, e := io.ReadAll(http.MaxBytesReader(w, request.Body, 32))
	if e != nil {
		emptyAuth(w, 204)
		return
	}
	decision := strings.ToLower(strings.TrimSpace(string(body)))
	a.mu.Lock()
	defer a.mu.Unlock()
	p := a.capabilities[tokenHash(token)]
	now := a.now()
	if !a.healthy || p == nil || p.context.Err() != nil || !p.expires.After(now) || decision != "approve" && decision != "deny" || decision == "approve" && tokenHash(token) != p.approveHash || decision == "deny" && tokenHash(token) != p.denyHash {
		emptyAuth(w, 204)
		return
	}
	a.finishApprovalLocked(p, decision == "approve", nil)
	emptyAuth(w, 204)
}
func (a *Auth) decideHost(host *peer, m map[string]json.RawMessage) bool {
	if !validApprovalDecision(m) {
		return false
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	p := a.pending[str(m, "id")]
	if !a.healthy || p == nil || p.host != host || p.context.Err() != nil || !p.expires.After(a.now()) {
		return false
	}
	a.finishApprovalLocked(p, boolean(m, "approve"), host)
	return true
}
func (a *Auth) finishApprovalLocked(p *pendingApproval, approve bool, source *peer) {
	now := a.now()
	a.detachPendingLocked(p)
	a.cancelHostLocked(p, source)
	if !approve {
		a.cooldown[p.prefix] = now.Add(15 * time.Second)
		p.result <- ""
		return
	}
	gateToken, e := randomCapability()
	if e != nil {
		p.result <- ""
		return
	}
	a.pruneLocked(now.UnixMilli())
	if len(a.state.Gates) >= maxAuthRecords {
		p.result <- ""
		return
	}
	key := tokenHash(gateToken)
	a.state.Gates[key] = authSession{ID: key[:24], CreatedAt: now.UnixMilli(), LastSeenAt: now.UnixMilli(), ExpiresAt: now.Add(shortSessionTTL).UnixMilli()}
	if a.persistLocked() != nil {
		p.result <- ""
		return
	}
	p.result <- gateToken
}
func (a *Auth) cancelApprovalsLocked() {
	for _, p := range a.pending {
		a.detachPendingLocked(p)
		a.cancelHostLocked(p, nil)
		p.cancel()
		p.result <- ""
	}
}

// Close releases outstanding held navigations and refuses subsequent admission.
// No plaintext session or decision tokens are written during shutdown.
func (a *Auth) Close() {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.healthy = false
	a.cancelApprovalsLocked()
}
func (a *Auth) manage(w http.ResponseWriter, request *http.Request) {
	if !authBearer(request, a.cfg.AdminToken) || !SameOrigin(request, a.cfg, true) {
		hiddenAuth(w)
		return
	}
	path := request.URL.Path
	if path == "/__gate/manage/password" && request.Method == "POST" {
		var payload struct {
			Password string `json:"password"`
		}
		if !decodeAuthJSON(w, request, &payload) {
			emptyAuth(w, 400)
			return
		}
		hash, e := HashPassword(payload.Password)
		if e != nil {
			emptyAuth(w, 400)
			return
		}
		password, _ := parsePasswordHash(hash)
		a.mu.Lock()
		defer a.mu.Unlock()
		if !a.healthy {
			emptyAuth(w, 503)
			return
		}
		a.password = password
		a.state.PasswordHash = hash
		a.state.Gates = make(map[string]authSession)
		a.state.Sessions = make(map[string]authSession)
		a.cancelApprovalsLocked()
		if a.persistLocked() != nil {
			emptyAuth(w, 503)
			return
		}
		emptyAuth(w, 204)
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if !a.healthy {
		emptyAuth(w, 503)
		return
	}
	a.pruneLocked(a.now().UnixMilli())
	collection := a.state.Gates
	auth := strings.HasPrefix(path, "/__gate/manage/auth-sessions")
	if auth {
		collection = a.state.Sessions
	}
	base := "/__gate/manage/sessions"
	if auth {
		base = "/__gate/manage/auth-sessions"
	}
	if path == base && request.Method == "GET" {
		records := make([]authSession, 0, len(collection))
		for _, record := range collection {
			record.GateHash = ""
			record.PasswordTag = ""
			records = append(records, record)
		}
		authJSON(w, records)
		return
	}
	if request.Method == "DELETE" && (path == base || strings.HasPrefix(path, base+"/")) {
		id := strings.TrimPrefix(path, base+"/")
		for key, record := range collection {
			if path == base || record.ID == id {
				delete(collection, key)
			}
		}
		if !auth && path == base {
			a.cancelApprovalsLocked()
		}
		if a.persistLocked() != nil {
			emptyAuth(w, 503)
			return
		}
		emptyAuth(w, 204)
		return
	}
	hiddenAuth(w)
}

// Existing worker/public/login.html family structure, adapted for the second
// factor. Shared assets and styles remain unchanged.
const relayLoginPage = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>BubblePlay · Sign in</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="icon" href="/favicon-32.png" type="image/png" sizes="32x32">
<link rel="stylesheet" href="/bubbleplay.css">
<style nonce="{{NONCE}}">.auth-card #totp{width:100%;min-height:48px;padding:10px 12px;color:var(--milk-cloth);background:var(--dark-surface);border:1px solid var(--thread-strong);border-radius:6px 10px 6px 8px}.totp-label{margin-top:16px}</style>
</head>
<body class="auth-page">
<section class="auth-identity" aria-label="BubblePlay">
  <div class="brand">
    <!-- BRAND-IDENTITY:start -->
    <span class="signal-cut" aria-hidden="true"><svg xmlns="http://www.w3.org/2000/svg" viewBox="92.833 95.408 1089.258 1089.258" width="26" height="26" aria-hidden="true"><g stroke="none"><rect x="92.833" y="95.408" width="1089.258" height="1089.258" rx="239.637" fill="#F3EFE7"/><defs><linearGradient id="identity-ear-highlight" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#EBC095"/><stop offset="1" stop-color="#DE945F"/></linearGradient></defs><g transform="translate(-20 -41)"><path d="M284.4444 414.8148L414.8148 272.5926H450.3704L580.7407 402.963L711.1111 272.5926H746.6667L877.037 414.8148V675.5556L758.5185 865.1852H379.2593L284.4444 770.3704Z" fill="#FFFAF1"/><path d="M284.4444 699.2593L391.1111 794.0741H805.9259L758.5185 865.1852H379.2593L284.4444 770.3704Z" fill="#E4DCCF"/><path d="M322.9444 436.3148L414.8148 272.5926L450.3704 272.5926L549.7407 394.963L515.98 397.37L460.8704 403.8148L385.4074 419.8704Z" fill="#282A2F" stroke="#282A2F" stroke-width="54.519"/><path d="M346.3333 401L426.5926 311.3704L494.2222 384L455.54 381.03L406.04 385.41Z" fill="#FFFAF1"/><path d="M877.037 414.8148L746.6667 272.5926L711.1111 272.5926L602.7407 415.963L702.1111 428.8148L796.13 430.99L859.0741 446.3704Z" fill="url(#identity-ear-highlight)"/><path d="M808.1481 400L728.8889 322.3704L657.2593 392L709.39 404.05L782.39 406.14Z" transform="translate(0 4)" fill="#FFFAF1"/><path d="M877.037 675.5556V414.8148L746.6667 272.5926H711.1111L580.7407 402.963L450.3704 272.5926H414.8148L284.4444 414.8148V770.3704L379.2593 865.1852H758.5185Z" fill="none" stroke="#282A2F" stroke-width="54.519" stroke-linejoin="bevel"/><path d="M391.1111 568.8889L444.4444 515.5556H462.2222L515.5556 568.8889" fill="none" stroke="#282A2F" stroke-width="41.481" stroke-linecap="round" stroke-linejoin="bevel"/><path d="M645.9259 568.8889L699.2593 515.5556H717.037L770.3704 568.8889" fill="none" stroke="#282A2F" stroke-width="41.481" stroke-linecap="round" stroke-linejoin="bevel"/><path d="M564.1481 616.2963H597.3333Q605.6296 616.2963 599.7037 622.8148L586.6667 635.2593Q580.7407 640.5926 574.8148 635.2593L561.7778 622.8148Q555.8519 616.2963 564.1481 616.2963Z" fill="#DE945F"/><path d="M503.7037 651.8519C503.7037 740.7407 580.7407 740.7407 580.7407 657.7778C580.7407 740.7407 657.7778 740.7407 657.7778 651.8519" fill="none" stroke="#282A2F" stroke-width="37.926" stroke-linecap="round"/><path d="M367.4074 640L402.963 648.2963" fill="none" stroke="#DDA39D" stroke-width="26.074" stroke-linecap="round"/><path d="M758.5185 648.2963L794.0741 640" fill="none" stroke="#DDA39D" stroke-width="26.074" stroke-linecap="round"/></g><path d="M744 677V1055L859 950L1017 939Z" fill="#F0A064" stroke="#282A2F" stroke-width="41.48" stroke-linejoin="round"/></g></svg></span>
    <!-- BRAND-IDENTITY:end -->
    <span class="wordmark">BubblePlay</span>
  </div>
  <div class="auth-statement">Your desktop,<br><span>held close.</span></div>
  <div class="auth-proof">This browser has passed the private approval gate. The application password is the final boundary before control begins.</div>
</section>

<main class="auth-form-field">
  <div class="auth-card">
    <!-- BRAND-MARK:start -->
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="92.833 95.408 1089.258 1395.612" width="64" height="82" role="img" aria-label="BubblePlay mark"><rect x="92.833" y="95.408" width="1089.258" height="1089.258" rx="239.637" fill="#F3EFE7"/><defs><linearGradient id="ear-highlight" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#EBC095"/><stop offset="1" stop-color="#DE945F"/></linearGradient></defs><g transform="translate(-20 -41)"><path d="M284.4444 414.8148L414.8148 272.5926H450.3704L580.7407 402.963L711.1111 272.5926H746.6667L877.037 414.8148V675.5556L758.5185 865.1852H379.2593L284.4444 770.3704Z" fill="#FFFAF1"/><path d="M284.4444 699.2593L391.1111 794.0741H805.9259L758.5185 865.1852H379.2593L284.4444 770.3704Z" fill="#E4DCCF"/><path d="M322.9444 436.3148L414.8148 272.5926L450.3704 272.5926L549.7407 394.963L515.98 397.37L460.8704 403.8148L385.4074 419.8704Z" fill="#282A2F" stroke="#282A2F" stroke-width="54.519"/><path d="M346.3333 401L426.5926 311.3704L494.2222 384L455.54 381.03L406.04 385.41Z" fill="#FFFAF1"/><path d="M877.037 414.8148L746.6667 272.5926L711.1111 272.5926L602.7407 415.963L702.1111 428.8148L796.13 430.99L859.0741 446.3704Z" fill="url(#ear-highlight)"/><path d="M808.1481 400L728.8889 322.3704L657.2593 392L709.39 404.05L782.39 406.14Z" transform="translate(0 4)" fill="#FFFAF1"/><path d="M877.037 675.5556V414.8148L746.6667 272.5926H711.1111L580.7407 402.963L450.3704 272.5926H414.8148L284.4444 414.8148V770.3704L379.2593 865.1852H758.5185Z" fill="none" stroke="#282A2F" stroke-width="54.519" stroke-linejoin="bevel"/><path d="M391.1111 568.8889L444.4444 515.5556H462.2222L515.5556 568.8889" fill="none" stroke="#282A2F" stroke-width="41.481" stroke-linecap="round" stroke-linejoin="bevel"/><path d="M645.9259 568.8889L699.2593 515.5556H717.037L770.3704 568.8889" fill="none" stroke="#282A2F" stroke-width="41.481" stroke-linecap="round" stroke-linejoin="bevel"/><path d="M564.1481 616.2963H597.3333Q605.6296 616.2963 599.7037 622.8148L586.6667 635.2593Q580.7407 640.5926 574.8148 635.2593L561.7778 622.8148Q555.8519 616.2963 564.1481 616.2963Z" fill="#DE945F"/><path d="M503.7037 651.8519C503.7037 740.7407 580.7407 740.7407 580.7407 657.7778C580.7407 740.7407 657.7778 740.7407 657.7778 651.8519" fill="none" stroke="#282A2F" stroke-width="37.926" stroke-linecap="round"/><path d="M367.4074 640L402.963 648.2963" fill="none" stroke="#DDA39D" stroke-width="26.074" stroke-linecap="round"/><path d="M758.5185 648.2963L794.0741 640" fill="none" stroke="#DDA39D" stroke-width="26.074" stroke-linecap="round"/></g><path d="M744 677V1055L859 950L1017 939Z" fill="#F0A064" stroke="#282A2F" stroke-width="41.48" stroke-linejoin="round"/></svg>
    <!-- BRAND-MARK:end -->
    <p class="auth-kicker">Browser approved</p>
    <h1>Welcome back</h1>
    <p class="auth-intro">Enter the BubblePlay password to open a private remote session.</p>
    <form id="login">
      <label class="sr-only">Username<input name="username" autocomplete="username" value="BubblePlay"></label>
      <label class="field-label" for="password">Password</label>
      <input id="password" name="password" type="password" autocomplete="current-password" required autofocus>
{{FACTOR}}
      <label class="remember remember-control"><input id="remember" type="checkbox" checked> <span>Remember this browser for 30 days</span></label>
      <button class="auth-submit" type="submit">Open BubblePlay</button>
      <div id="status" class="auth-status" role="alert" aria-live="polite"></div>
    </form>
{{APPROVAL}}
    <div class="security-note">The password creates a server-held browser session. It is never stored in this page or local storage.</div>
  </div>
</main>

<script nonce="{{NONCE}}">
  const form = document.getElementById("login");
  const password = document.getElementById("password");
  const remember = document.getElementById("remember");
  const status = document.getElementById("status");
  const button = form.querySelector("button");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    button.disabled = true;
    button.textContent = "Opening…";
    status.textContent = "";
    try {
      const response = await fetch("/__auth/login", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password: password.value, totp: document.getElementById("totp")?.value || "", remember: remember.checked }),
      });
      if (response.ok) {
        // The held navigation may intentionally select a transport/profile.
        // Keep its same-origin path and query; never reflect an external URL.
        location.replace(location.pathname + location.search);
        return;
      }
      password.select();
      status.textContent = response.status === 429
        ? "Too many attempts. This browser can try again after the cooldown."
        : "Password or authenticator code did not match. Please try again.";
    } catch {
      status.textContent = "The connection failed. Check the network and retry.";
    } finally {
      button.disabled = false;
      button.textContent = "Open BubblePlay";
      password.focus();
    }
  });
</script>
</body>
</html>
`
