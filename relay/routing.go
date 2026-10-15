package main

import (
	"crypto/sha256"
	"crypto/subtle"
	"encoding/json"
	"net/http"
	"sort"
	"strings"

	"github.com/gorilla/websocket"
)

func pcPeerKey(kind, epoch, role, pc string) string {
	key := peerKey(kind, epoch, role)
	if pc != "" {
		key += ":" + pc
	}
	return key
}

func (s *Relay) findRoomLocked(pc string) *room {
	if pc == "" {
		return &s.room
	}
	return s.rooms[pc]
}

func (s *Relay) roomLocked(pc string) *room {
	rm := s.findRoomLocked(pc)
	if rm == nil {
		rm = &room{cachedHost: make(map[string][]byte)}
		s.rooms[pc] = rm
	}
	return rm
}

func peerOpen(p *peer) bool {
	if p == nil {
		return false
	}
	select {
	case <-p.done:
		return false
	default:
		return true
	}
}

// All room/peer decisions hold Relay.mu before consulting durable auth state.
func (s *Relay) defaultPCLocked() string {
	s.auth.mu.Lock()
	defer s.auth.mu.Unlock()
	latest, online := "", ""
	var latestSeen, onlineSeen int64
	for id, record := range s.auth.state.PCs {
		if latest == "" || record.LastSeen > latestSeen || (record.LastSeen == latestSeen && id > latest) {
			latest, latestSeen = id, record.LastSeen
		}
		if peerOpen(s.peers[pcPeerKey("signal", "", "host", id)]) && (online == "" || record.LastSeen > onlineSeen || (record.LastSeen == onlineSeen && id > online)) {
			online, onlineSeen = id, record.LastSeen
		}
	}
	if online != "" {
		return online
	}
	if peerOpen(s.peers["signal:host"]) {
		return ""
	}
	return latest
}

func (s *Relay) canRegisterPC(id, public string, hello map[string]json.RawMessage) bool {
	s.auth.mu.Lock()
	defer s.auth.mu.Unlock()
	return s.canRegisterPCLocked(id, public, hello)
}

// auth.mu must be held; a supplied token is checked even for a remembered PC.
func (s *Relay) canRegisterPCLocked(id, public string, hello map[string]json.RawMessage) bool {
	if !s.auth.healthy || (has(hello, "token") && !secureEqual(str(hello, "token"), s.cfg.Token)) {
		return false
	}
	record, known := s.auth.state.PCs[id]
	if known {
		return record.Public == public
	}
	return len(s.auth.state.PCs) < maxAuthRecords && secureEqual(str(hello, "token"), s.cfg.Token)
}

func (s *Relay) registerPCLocked(id, public string, hello map[string]json.RawMessage) bool {
	s.auth.mu.Lock()
	defer s.auth.mu.Unlock()
	// Recheck after proof so deletion during a challenge revokes remembered admission.
	if !s.canRegisterPCLocked(id, public, hello) {
		return false
	}
	now := s.now().UnixMilli()
	record, known := s.auth.state.PCs[id]
	if !known {
		record = pcRecord{Public: public, FirstSeen: now}
	}
	record.LastSeen = now
	s.auth.state.PCs[id] = record
	return s.auth.persistLocked() == nil
}

func (s *Relay) validTicketLocked(ticket, pc, role, epoch string) bool {
	if _, ok := decodeKeyField(ticket, 32); !ok {
		return false
	}
	issuer := s.peers[pcPeerKey("signal", "", role, pc)]
	if !peerOpen(issuer) || !s.now().Before(issuer.ticketExpires) || (role == "client" && issuer.epoch != epoch) {
		return false
	}
	hash := sha256.Sum256([]byte(ticket))
	return subtle.ConstantTimeCompare(hash[:], issuer.ticketHash[:]) == 1
}

func (s *Relay) managePCs(w http.ResponseWriter, r *http.Request) bool {
	const base = "/__gate/manage/pcs"
	if r.URL.Path != base && !strings.HasPrefix(r.URL.Path, base+"/") {
		return false
	}
	if !authBearer(r, s.cfg.AdminToken) || !SameOrigin(r, s.cfg, true) {
		hiddenAuth(w)
		return true
	}
	s.mu.Lock()
	s.auth.mu.Lock()
	if !s.auth.healthy || s.stopped {
		s.auth.mu.Unlock()
		s.mu.Unlock()
		emptyAuth(w, 503)
		return true
	}
	if r.Method == "GET" && r.URL.Path == base {
		type listedPC struct {
			ID string `json:"id"`
			pcRecord
			Online bool `json:"online"`
		}
		records := make([]listedPC, 0, len(s.auth.state.PCs))
		for id, record := range s.auth.state.PCs {
			records = append(records, listedPC{id, record, peerOpen(s.peers[pcPeerKey("signal", "", "host", id)])})
		}
		s.auth.mu.Unlock()
		s.mu.Unlock()
		sort.Slice(records, func(i, j int) bool { return records[i].ID < records[j].ID })
		authJSON(w, records)
		return true
	}
	if r.Method == "DELETE" && strings.HasPrefix(r.URL.Path, base+"/") {
		id, ok := canonicalDeviceID(strings.TrimPrefix(r.URL.Path, base+"/"))
		if !ok {
			s.auth.mu.Unlock()
			s.mu.Unlock()
			hiddenAuth(w)
			return true
		}
		delete(s.auth.state.PCs, id)
		if s.auth.persistLocked() != nil {
			s.auth.mu.Unlock()
			s.mu.Unlock()
			emptyAuth(w, 503)
			return true
		}
		s.auth.mu.Unlock()
		var hosts []*peer
		for p := range s.all {
			if p.pc == id && (p.expected == "host" || p.expected == "version-probe" || strings.HasSuffix(p.expected, "-host")) {
				hosts = append(hosts, p)
				// Detach now so the ticket is revoked before another upgrade can run.
				s.removePeerLocked(p)
			}
		}
		s.mu.Unlock()
		for _, p := range hosts {
			p.close(websocket.ClosePolicyViolation, "peer removed")
		}
		emptyAuth(w, 204)
		return true
	}
	s.auth.mu.Unlock()
	s.mu.Unlock()
	hiddenAuth(w)
	return true
}
