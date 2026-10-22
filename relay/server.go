package main

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha1"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
)

const signalLimit = 512 * 1024
const mediaLimit = 8*1024*1024 + 28
const fileLimit = 64*1024 + 16
const queueByteLimit = 16 * 1024 * 1024

var epochPattern = regexp.MustCompile(`^[a-f0-9]{32}$`)

type wireFrame struct {
	kind      int
	data      []byte
	delivered chan struct{}
}
type peer struct {
	server                           *Relay
	conn                             *websocket.Conn
	expected, role, kind, epoch, key string
	pc                               string
	hostBearer                       bool
	ticketHash                       [32]byte
	ticketExpires                    time.Time
	queue                            chan wireFrame
	done                             chan struct{}
	once                             sync.Once
	queued                           atomic.Int64
	registered                       bool
	browserApproval                  bool
	authContext                      *http.Request
}

func (p *peer) send(kind int, data []byte) {
	for {
		old := p.queued.Load()
		if old+int64(len(data)) > queueByteLimit {
			go p.close(websocket.CloseTryAgainLater, "slow peer")
			return
		}
		if p.queued.CompareAndSwap(old, old+int64(len(data))) {
			break
		}
	}
	select {
	case <-p.done:
		p.queued.Add(-int64(len(data)))
	case p.queue <- wireFrame{kind: kind, data: data}:
	default:
		p.queued.Add(-int64(len(data)))
		go p.close(websocket.CloseTryAgainLater, "slow peer")
	}
}
func (p *peer) json(value any) {
	data, err := json.Marshal(value)
	if err == nil {
		p.send(websocket.TextMessage, data)
	}
}
func (p *peer) close(code int, reason string) {
	p.once.Do(func() {
		close(p.done)
		_ = p.conn.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(code, reason), time.Now().Add(time.Second))
		_ = p.conn.Close()
		p.server.remove(p)
	})
}
func (p *peer) writer() {
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-p.done:
			return
		case <-ticker.C:
			if p.authContext != nil && !p.server.auth.AuthorizedBrowser(p.authContext) {
				p.close(websocket.ClosePolicyViolation, "browser session revoked")
				return
			}
		case frame := <-p.queue:
			p.queued.Add(-int64(len(frame.data)))
			_ = p.conn.SetWriteDeadline(time.Now().Add(10 * time.Second))
			if err := p.conn.WriteMessage(frame.kind, frame.data); err != nil {
				p.close(websocket.CloseGoingAway, "write failed")
				return
			}
			if frame.delivered != nil {
				close(frame.delivered)
			}
		}
	}
}

func (p *peer) helloAndWait() bool {
	data := []byte(`{"type":"hello-ok"}`)
	delivered := make(chan struct{})
	p.queued.Add(int64(len(data)))
	select {
	case p.queue <- wireFrame{kind: websocket.TextMessage, data: data, delivered: delivered}:
	case <-p.done:
		p.queued.Add(-int64(len(data)))
		return false
	}
	select {
	case <-delivered:
		return true
	case <-p.done:
		return false
	case <-time.After(2 * time.Second):
		return false
	}
}

type room struct {
	pending      [][]byte
	cachedHost   map[string][]byte
	currentEpoch string
}

type Relay struct {
	room
	rooms           map[string]*room
	now             func() time.Time
	cfg             Config
	auth            *Auth
	mu              sync.Mutex
	peers           map[string]*peer
	all             map[*peer]bool
	pendingUpgrades int
	stopped         bool
	assets          string
	proxyWarning    sync.Once
}

func NewRelay(cfg Config) (*Relay, error) {
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	if err := ensurePrivateDirectory(cfg.StateDir); err != nil {
		return nil, err
	}
	a, err := NewAuth(cfg)
	if err != nil {
		return nil, err
	}
	assets, err := filepath.Abs(cfg.AssetsDir)
	if err != nil {
		return nil, err
	}
	assets, err = filepath.EvalSymlinks(assets)
	if err != nil {
		return nil, errors.New("assets directory unavailable")
	}
	s := &Relay{cfg: cfg, auth: a, peers: make(map[string]*peer), all: make(map[*peer]bool), room: room{cachedHost: make(map[string][]byte)}, rooms: make(map[string]*room), now: time.Now, assets: assets}
	a.hostOnline = s.approvalHostOnline
	a.requestHost = s.requestHostApproval
	return s, nil
}
func (s *Relay) approvalHostOnline() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	p := s.peers[pcPeerKey("signal", "", "host", s.defaultPCLocked())]
	if s.stopped || p == nil || !p.browserApproval {
		return false
	}
	select {
	case <-p.done:
		return false
	default:
		return true
	}
}
func (s *Relay) requestHostApproval(pending *pendingApproval, agent string) bool {
	if !validApprovalID(pending.id) || !validApprovalAgent(agent) {
		return false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	host := s.peers[pcPeerKey("signal", "", "host", s.defaultPCLocked())]
	if s.stopped || host == nil || !host.browserApproval {
		return false
	}
	select {
	case <-host.done:
		return false
	default:
	}
	s.auth.mu.Lock()
	defer s.auth.mu.Unlock()
	if s.auth.pending[pending.id] != pending || pending.context.Err() != nil || !pending.expires.After(s.auth.now()) {
		return false
	}
	pending.host = host
	host.json(map[string]any{"type": "approval-request", "id": pending.id, "client": pending.prefix, "agent": agent, "expiresAt": pending.expires.UnixMilli()})
	return true
}
func (s *Relay) Close() {
	s.mu.Lock()
	if s.stopped {
		s.mu.Unlock()
		return
	}
	s.stopped = true
	peers := make([]*peer, 0, len(s.all))
	for p := range s.all {
		peers = append(peers, p)
	}
	s.mu.Unlock()
	for _, p := range peers {
		p.close(websocket.CloseGoingAway, "relay stopped")
	}
	s.auth.Close()
}
func (s *Relay) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if len(s.cfg.TrustedProxyCIDRs) == 0 && (r.Header.Get("X-Forwarded-For") != "" || r.Header.Get("Forwarded") != "") {
		s.proxyWarning.Do(func() {
			log.Print("Forwarding headers received without trustedProxyCIDRs; client lockouts share the proxy address. Configure init --trusted-proxy with the actual proxy CIDR.")
		})
	}
	u, _ := url.Parse(s.cfg.PublicURL)
	if !strings.EqualFold(r.Host, u.Host) {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Referrer-Policy", "no-referrer")
	if r.URL.Path == "/healthz" && (r.Method == "GET" || r.Method == "HEAD") {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		if r.Method != "HEAD" {
			_, _ = w.Write([]byte(`{"ok":true,"backend":"go"}`))
		}
		return
	}
	kind, role := socketRoute(r.URL.Path)
	if role != "" {
		s.upgrade(w, r, kind, role)
		return
	}
	if strings.HasPrefix(r.URL.Path, "/agent") || strings.HasPrefix(r.URL.Path, "/lab-") {
		http.NotFound(w, r)
		return
	}
	if s.managePCs(w, r) {
		return
	}
	if s.auth.Handle(w, r) {
		return
	}
	s.serveAsset(w, r)
}
func (s *Relay) serveAsset(w http.ResponseWriter, r *http.Request) {
	if r.Method != "GET" && r.Method != "HEAD" {
		http.NotFound(w, r)
		return
	}
	clean := path.Clean("/" + r.URL.Path)
	if clean == "/" {
		clean = "/index.html"
	}
	if strings.Contains(clean, "\\") {
		http.NotFound(w, r)
		return
	}
	file := filepath.Join(s.assets, filepath.FromSlash(strings.TrimPrefix(clean, "/")))
	resolved, err := filepath.EvalSymlinks(file)
	if err != nil {
		http.NotFound(w, r)
		return
	}
	relative, err := filepath.Rel(s.assets, resolved)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		http.NotFound(w, r)
		return
	}
	info, err := os.Stat(resolved)
	if err != nil || !info.Mode().IsRegular() {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Cache-Control", "no-store")
	http.ServeFile(w, r, resolved)
}
func socketRoute(route string) (kind, role string) {
	switch route {
	case "/host-ws":
		return "signal", "host"
	case "/ws":
		return "signal", "client"
	case "/host-version-ws":
		return "signal", "version-probe"
	case "/host-media-ws":
		return "media", "media-host"
	case "/media-ws":
		return "media", "media-client"
	case "/host-file-ws":
		return "file", "file-host"
	case "/file-ws":
		return "file", "file-client"
	}
	return "", ""
}
func (s *Relay) upgrade(w http.ResponseWriter, r *http.Request, kind, role string) {
	if r.Method != "GET" || !websocket.IsWebSocketUpgrade(r) || !SameOrigin(r, s.cfg, true) {
		http.NotFound(w, r)
		return
	}
	pc := ""
	query, queryErr := url.ParseQuery(r.URL.RawQuery)
	if queryErr != nil {
		http.NotFound(w, r)
		return
	}
	if values, present := query["pc"]; present {
		var valid bool
		if len(values) != 1 {
			http.NotFound(w, r)
			return
		}
		pc, valid = canonicalDeviceID(values[0])
		if !valid {
			http.NotFound(w, r)
			return
		}
	}
	host := role == "host" || role == "version-probe" || strings.HasSuffix(role, "-host")
	bearer := s.auth.AuthorizedHost(r)
	if host {
		// Data sockets can authenticate by a ticket in hello; signaling always needs the bearer.
		if kind == "signal" && !bearer {
			http.NotFound(w, r)
			return
		}
	} else if !s.auth.AuthorizedBrowser(r) {
		http.NotFound(w, r)
		return
	}
	epoch := query.Get("epoch")
	s.mu.Lock()
	if !host && !query.Has("pc") {
		pc = s.defaultPCLocked()
	}
	rm := s.findRoomLocked(pc)
	// Old viewers send the signaling epoch but no PC or ticket. Checking it
	// against the resolved room refuses a default that changed since /ws.
	if s.stopped || len(s.all)+s.pendingUpgrades >= s.cfg.MaxConnections || (kind != "signal" && (!epochPattern.MatchString(epoch) || rm == nil || epoch != rm.currentEpoch)) {
		s.mu.Unlock()
		http.NotFound(w, r)
		return
	}
	// Reserve the connection before Upgrade yields, including unauthenticated hello peers.
	p := &peer{server: s, kind: kind, expected: role, epoch: epoch, pc: pc, hostBearer: bearer, queue: make(chan wireFrame, 80), done: make(chan struct{})}
	if !host {
		p.authContext = &http.Request{Header: http.Header{"Cookie": {r.Header.Get("Cookie")}}}
	}
	s.pendingUpgrades++
	s.mu.Unlock()
	u := websocket.Upgrader{ReadBufferSize: 4096, WriteBufferSize: 4096, HandshakeTimeout: 5 * time.Second, CheckOrigin: func(_ *http.Request) bool { return true }, EnableCompression: false}
	conn, err := u.Upgrade(w, r, nil)
	if err != nil {
		s.mu.Lock()
		s.pendingUpgrades--
		s.mu.Unlock()
		return
	}
	s.mu.Lock()
	s.pendingUpgrades--
	if s.stopped {
		s.mu.Unlock()
		_ = conn.Close()
		return
	}
	p.conn = conn
	s.all[p] = true
	s.mu.Unlock()
	_ = conn.SetWriteDeadline(time.Time{})
	_ = conn.SetReadDeadline(time.Now().Add(10 * time.Second))
	limit := int64(signalLimit)
	if kind == "media" {
		limit = mediaLimit
	}
	if kind == "file" {
		limit = fileLimit
	}
	conn.SetReadLimit(limit)
	go p.writer()
	s.read(p)
}
func (s *Relay) read(p *peer) {
	defer p.close(websocket.CloseGoingAway, "peer disconnected")
	for {
		kind, data, err := p.conn.ReadMessage()
		if err != nil {
			return
		}
		if p.authContext != nil && !s.auth.AuthorizedBrowser(p.authContext) {
			p.close(websocket.ClosePolicyViolation, "browser session revoked")
			return
		}
		if !p.registered {
			if kind != websocket.TextMessage || !s.hello(p, data) {
				p.close(websocket.ClosePolicyViolation, "invalid hello")
				return
			}
			if p.role == "version-probe" {
				p.helloAndWait()
				p.close(websocket.CloseNormalClosure, "version-probe-complete")
				return
			}
			p.registered = true
		} else if !s.forward(p, kind, data) {
			p.close(websocket.ClosePolicyViolation, "invalid packet")
			return
		}
		_ = p.conn.SetReadDeadline(time.Now().Add(90 * time.Second))
	}
}
func peerKey(kind, epoch, role string) string {
	if kind == "signal" {
		return kind + ":" + role
	}
	return kind + ":" + epoch + ":" + role
}
func otherRole(role string) string {
	switch role {
	case "host":
		return "client"
	case "client":
		return "host"
	case "media-host":
		return "media-client"
	case "media-client":
		return "media-host"
	case "file-host":
		return "file-client"
	case "file-client":
		return "file-host"
	}
	return ""
}
func newEpoch() string {
	value := make([]byte, 16)
	if _, err := rand.Read(value); err != nil {
		panic("system random source unavailable")
	}
	return hex.EncodeToString(value)
}
func (s *Relay) hello(p *peer, data []byte) bool {
	var m map[string]json.RawMessage
	if len(data) > 16384 || json.Unmarshal(data, &m) != nil || str(m, "type") != "hello" || str(m, "role") != p.expected {
		return false
	}
	public := ""
	if p.expected == "host" {
		if p.pc == "" {
			if has(m, "id") || !secureEqual(str(m, "token"), s.cfg.Token) {
				return false
			}
		} else {
			id, ok := canonicalDeviceID(str(m, "id"))
			key, valid := decodeKeyField(str(m, "public"), 32)
			if !ok || id != p.pc || !valid || deviceID(key) != id {
				return false
			}
			public = str(m, "public")
			if !s.canRegisterPC(id, public, m) {
				return false
			}
			nonce, err := randomCapability()
			if err != nil {
				return false
			}
			p.json(map[string]any{"type": "register-challenge", "nonce": nonce})
			_ = p.conn.SetReadDeadline(time.Now().Add(10 * time.Second))
			kind, proof, err := p.conn.ReadMessage()
			var message map[string]json.RawMessage
			if err != nil || kind != websocket.TextMessage || len(proof) > 16384 || json.Unmarshal(proof, &message) != nil || str(message, "type") != "register-proof" {
				return false
			}
			signature, ok := decodeKeyField(str(message, "signature"), 64)
			nonceBytes, _ := decodeKeyField(nonce, 32)
			if !ok || !validRegistrationProof(s.cfg.PublicURL, nonceBytes, id, key, signature) {
				return false
			}
		}
	}
	if p.expected == "version-probe" && !secureEqual(str(m, "token"), s.cfg.Token) {
		return false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.stopped || !s.all[p] || !peerOpen(p) {
		return false
	}
	if public != "" && !s.registerPCLocked(p.pc, public, m) {
		return false
	}
	rm := s.roomLocked(p.pc)
	if p.kind != "signal" {
		side := "client"
		if strings.HasSuffix(p.expected, "-host") {
			side = "host"
		}
		validTicket := s.validTicketLocked(str(m, "ticket"), p.pc, side, p.epoch)
		if side == "host" && !p.hostBearer && !validTicket {
			return false
		}
		if side == "client" && has(m, "ticket") && !validTicket {
			return false
		}
	}
	p.role = p.expected
	p.key = pcPeerKey(p.kind, p.epoch, p.role, p.pc)
	if p.role == "version-probe" {
		return true
	}
	if p.kind != "signal" && p.epoch != rm.currentEpoch {
		return false
	}
	if old := s.peers[p.key]; old != nil {
		delete(s.peers, p.key)
		if p.role == "host" {
			if viewer := s.peers[pcPeerKey("signal", "", "client", p.pc)]; viewer != nil {
				viewer.json(map[string]any{"type": "peer-left"})
			}
		}
		go old.close(websocket.ClosePolicyViolation, "peer replaced")
	}
	if p.role == "client" {
		rm.pending = nil
		rm.currentEpoch = newEpoch()
		p.epoch = rm.currentEpoch
		for _, old := range s.peers {
			if old.kind != "signal" && old.pc == p.pc {
				go old.close(websocket.CloseGoingAway, "stale epoch")
			}
		}
	}
	s.peers[p.key] = p
	hello := map[string]any{"type": "hello-ok"}
	if p.kind == "signal" {
		ticket, err := randomCapability()
		if err != nil {
			delete(s.peers, p.key)
			return false
		}
		p.ticketHash = sha256.Sum256([]byte(ticket))
		p.ticketExpires = s.now().Add(12 * time.Hour)
		hello["ticket"] = ticket
		if p.pc != "" {
			hello["pc"] = p.pc
		}
	}
	if p.role == "client" {
		hello["mediaEpoch"] = p.epoch
		hello["iceServers"] = s.iceServers(boolean(m, "turn"))
	}
	if p.role == "media-client" {
		hello["registeredAtMs"] = time.Now().UnixMilli()
	}
	p.json(hello)
	if p.kind != "signal" {
		other := s.peers[pcPeerKey(p.kind, p.epoch, otherRole(p.role), p.pc)]
		event := p.kind + "-peer"
		p.json(map[string]any{"type": event, "present": other != nil})
		if other != nil {
			other.json(map[string]any{"type": event, "present": true})
		}
	}
	if p.role == "client" {
		for _, capability := range rm.cachedHost {
			p.send(websocket.TextMessage, capability)
		}
	}
	if p.role == "host" {
		rm.cachedHost = make(map[string][]byte)
		for _, pending := range rm.pending {
			p.send(websocket.TextMessage, pending)
		}
		rm.pending = nil
	}
	return true
}
func (s *Relay) remove(p *peer) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.removePeerLocked(p)
}
func (s *Relay) removePeerLocked(p *peer) {
	delete(s.all, p)
	defer func() {
		if p.pc == "" {
			return
		}
		for other := range s.all {
			if other.pc == p.pc {
				return
			}
		}
		delete(s.rooms, p.pc)
	}()
	if s.peers[p.key] != p {
		return
	}
	delete(s.peers, p.key)
	rm := s.roomLocked(p.pc)
	if p.role == "client" {
		rm.pending = nil
		if rm.currentEpoch == p.epoch {
			rm.currentEpoch = ""
		}
		for _, old := range s.peers {
			if old.kind != "signal" && old.pc == p.pc && old.epoch == p.epoch {
				go old.close(websocket.CloseGoingAway, "viewer departed")
			}
		}
	}
	if p.role == "host" {
		rm.cachedHost = make(map[string][]byte)
	}
	other := s.peers[pcPeerKey(p.kind, p.epoch, otherRole(p.role), p.pc)]
	if other == nil {
		return
	}
	if p.kind == "signal" {
		other.json(map[string]any{"type": "peer-left"})
	} else if p.kind == "file" {
		delete(s.peers, other.key)
		go other.close(websocket.CloseGoingAway, "file-peer-left")
	} else {
		other.json(map[string]any{"type": p.kind + "-peer", "present": false})
	}
}
func (s *Relay) forward(p *peer, kind int, data []byte) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.stopped || s.peers[p.key] != p {
		return false
	}
	rm := s.roomLocked(p.pc)
	if p.kind != "signal" && p.epoch != rm.currentEpoch {
		return false
	}
	other := s.peers[pcPeerKey(p.kind, p.epoch, otherRole(p.role), p.pc)]
	if kind == websocket.BinaryMessage {
		valid := false
		switch p.kind {
		case "signal":
			valid = len(data) > 0 && len(data) <= 16384
		case "media":
			valid = p.role == "media-host" && len(data) > 28 && len(data) <= mediaLimit
		case "file":
			valid = validFilePacket(data, p.role)
		}
		if !valid {
			if p.kind == "file" && other != nil {
				go other.close(websocket.CloseGoingAway, "file-peer-left")
			}
			return p.kind != "file"
		}
		if other != nil {
			other.send(kind, data)
		}
		return true
	}
	if kind != websocket.TextMessage {
		return false
	}
	var m map[string]json.RawMessage
	if json.Unmarshal(data, &m) != nil {
		return false
	}
	typ := str(m, "type")
	if typ == "ping" {
		p.json(map[string]any{"type": "pong"})
		return true
	}
	if p.kind == "media" {
		if p.role != "media-client" || !validMediaControl(m, len(data)) {
			return true
		}
		if other != nil {
			other.send(kind, data)
		}
		return true
	}
	if p.kind == "file" {
		return false
	}
	if typ == "approval-decision" {
		if p.role == "host" {
			s.auth.decideHost(p, m)
		}
		return true
	}
	if !validSignalMessage(m, p.role, p.epoch, len(data)) {
		return true
	}
	if typ == "bye" {
		if other != nil {
			other.json(map[string]any{"type": "peer-left"})
		}
		go p.close(websocket.CloseNormalClosure, "bye")
		return true
	}
	if p.role == "host" && (typ == "host-capabilities" || typ == "input-capabilities") {
		if typ == "host-capabilities" {
			p.browserApproval = boolean(m, "browserApproval")
		}
		rm.cachedHost[typ] = append([]byte(nil), data...)
	}
	if other != nil {
		other.send(kind, data)
		return true
	}
	if p.role == "client" && bufferable(m) {
		if typ == "media-control" || typ == "first-frame" {
			kept := rm.pending[:0]
			for _, old := range rm.pending {
				var previous map[string]json.RawMessage
				_ = json.Unmarshal(old, &previous)
				if str(previous, "type") == typ && (typ == "first-frame" || str(previous, "action") == str(m, "action")) {
					continue
				}
				kept = append(kept, old)
			}
			rm.pending = kept
		}
		rm.pending = append(rm.pending, append([]byte(nil), data...))
		if len(rm.pending) > 64 {
			rm.pending = rm.pending[1:]
		}
	}
	return true
}
func bufferable(m map[string]json.RawMessage) bool {
	typ := str(m, "type")
	action := str(m, "action")
	return typ == "offer" || typ == "ice" || (typ == "first-frame" && str(m, "phase") == "request") || (typ == "media-control" && (action == "start" || action == "monitors" || action == "follow"))
}
func turnAuthority(value string) (string, error) {
	if strings.ContainsAny(value, "/?#@ \t\r\n") {
		return "", errors.New("invalid TURN host")
	}
	host, port, err := net.SplitHostPort(value)
	if err != nil {
		if strings.Contains(value, ":") {
			return "", errors.New("TURN host must be host or host:port")
		}
		host = value
		port = "3478"
	}
	n, err := strconv.Atoi(port)
	if err != nil || n < 1 || n > 65535 || host == "" {
		return "", errors.New("invalid TURN host")
	}
	return net.JoinHostPort(host, port), nil
}
func (s *Relay) iceServers(turn bool) []map[string]any {
	servers := []map[string]any{{"urls": "stun:stun.l.google.com:19302"}}
	if !turn || s.cfg.TurnHost == "" || s.cfg.TurnSecret == "" {
		return servers
	}
	authority, _ := turnAuthority(s.cfg.TurnHost)
	username := fmt.Sprintf("%d:%s", time.Now().Unix()+int64(s.cfg.TurnTTLSeconds), randomCredential(12))
	mac := hmac.New(sha1.New, []byte(s.cfg.TurnSecret))
	_, _ = mac.Write([]byte(username))
	return append(servers, map[string]any{"urls": []string{"turn:" + authority + "?transport=udp", "turn:" + authority + "?transport=tcp"}, "username": username, "credential": base64.StdEncoding.EncodeToString(mac.Sum(nil))})
}
