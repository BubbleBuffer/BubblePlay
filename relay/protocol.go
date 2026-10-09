package main

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"math"
	"regexp"
	"strings"
	"unicode/utf8"
)

func str(m map[string]json.RawMessage, key string) string {
	var value string
	_ = json.Unmarshal(m[key], &value)
	return value
}
func boolean(m map[string]json.RawMessage, key string) bool {
	var value bool
	_ = json.Unmarshal(m[key], &value)
	return value
}
func isBoolean(m map[string]json.RawMessage, key string) bool {
	var value *bool
	return json.Unmarshal(m[key], &value) == nil && value != nil
}
func number(m map[string]json.RawMessage, key string) (float64, bool) {
	var value *float64
	err := json.Unmarshal(m[key], &value)
	if err != nil || value == nil {
		return 0, false
	}
	return *value, !math.IsNaN(*value) && !math.IsInf(*value, 0)
}
func integer(m map[string]json.RawMessage, key string, min, max float64) bool {
	value, ok := number(m, key)
	return ok && value >= min && value <= max && value == math.Trunc(value)
}
func uint32Value(m map[string]json.RawMessage, key string) bool {
	return integer(m, key, 0, 4294967295)
}
func object(m map[string]json.RawMessage, key string) map[string]json.RawMessage {
	var result map[string]json.RawMessage
	_ = json.Unmarshal(m[key], &result)
	return result
}
func has(m map[string]json.RawMessage, key string) bool { _, ok := m[key]; return ok }
func only(m map[string]json.RawMessage, keys ...string) bool {
	for key := range m {
		found := false
		for _, allowed := range keys {
			if key == allowed {
				found = true
				break
			}
		}
		if !found {
			return false
		}
	}
	return true
}
func exact(m map[string]json.RawMessage, keys ...string) bool {
	return len(m) == len(keys) && only(m, keys...)
}
func boundedString(m map[string]json.RawMessage, key string, min, max int) bool {
	var value *string
	if json.Unmarshal(m[key], &value) != nil {
		return false
	}
	return value != nil && len(*value) >= min && len(*value) <= max && utf8.ValidString(*value)
}
func protocol(m map[string]json.RawMessage, version float64) bool {
	return integer(m, "protocol", version, version)
}
func validMediaControl(m map[string]json.RawMessage, length int) bool {
	if length > 512 {
		return false
	}
	switch str(m, "type") {
	case "ack":
		stamp, valid := number(m, "recvTsMs")
		return only(m, "type", "generation", "seq", "recvTsMs", "decodedSeq") && uint32Value(m, "generation") && uint32Value(m, "seq") && valid && stamp >= 0 && (!has(m, "decodedSeq") || uint32Value(m, "decodedSeq"))
	case "keyframe":
		return exact(m, "type", "reason") && regexp.MustCompile(`^[A-Za-z0-9_.:-]{1,64}$`).MatchString(str(m, "reason"))
	}
	return false
}
func validSignalMessage(m map[string]json.RawMessage, role, epoch string, length int) bool {
	if m == nil || length > signalLimit {
		return false
	}
	typ := str(m, "type")
	host := role == "host"
	client := role == "client"
	switch typ {
	case "bye":
		return len(m) == 1
	case "offer":
		return client && length <= 256*1024 && boundedString(m, "sdp", 1, 256*1024)
	case "answer":
		return host && length <= 256*1024 && boundedString(m, "sdp", 1, 256*1024)
	case "ice":
		return length <= 16384 && (client || host) && has(m, "candidate")
	case "diag":
		return client && length <= 16384
	case "client-log":
		return client && length <= 2048 && has(m, "seq") && has(m, "atMs") && boundedString(m, "level", 1, 32) && boundedString(m, "event", 1, 128) && object(m, "detail") != nil
	case "qos":
		return client && length <= 2048
	case "quality":
		return host && length <= 1024
	case "host-capabilities":
		if host && exact(m, "type", "browserApproval") && isBoolean(m, "browserApproval") {
			return true
		}
		if !host || length > 512 || !isBoolean(m, "serviceMode") || !isBoolean(m, "coldBoot") || !isBoolean(m, "secureDesktop") || !isBoolean(m, "secureText") {
			return false
		}
		if has(m, "browserApproval") && !isBoolean(m, "browserApproval") {
			return false
		}
		if has(m, "fileTransfer") && !integer(m, "fileTransfer", 0, 255) {
			return false
		}
		if has(m, "fileDrop") && (!integer(m, "fileDrop", 1, 1) || !integer(m, "fileTransfer", 1, 1)) {
			return false
		}
		if has(m, "fileDropPreview") && (!integer(m, "fileDropPreview", 1, 1) || !integer(m, "fileDrop", 1, 1) || boolean(m, "serviceMode")) {
			return false
		}
		return !has(m, "followWindow") || integer(m, "followWindow", 0, 255)
	case "desktop-state":
		return host && length <= 512 && validDesktopState(m)
	case "secure-action":
		return client && length <= 512 && exact(m, "type", "action") && str(m, "action") == "deny"
	case "secure-action-result":
		return host && length <= 512 && exact(m, "type", "action", "ok", "reason") && str(m, "action") == "deny" && isBoolean(m, "ok") && boundedString(m, "reason", 0, 96)
	case "input-status":
		if !host || length > 1024 || !protocol(m, 1) || !uint32Value(m, "generation") {
			return false
		}
		if has(m, "reason") && !bytes.Equal(bytes.TrimSpace(m["reason"]), []byte("null")) && str(m, "reason") != "admin-required" && str(m, "reason") != "admin-unavailable" {
			return false
		}
		if has(m, "route") && str(m, "route") != "local" && str(m, "route") != "admin" && str(m, "route") != "secure" {
			return false
		}
		for _, key := range []string{"receivedPackets", "acceptedPackets", "failedPackets", "droppedPackets", "releasedInputs", "requestedEvents", "appliedEvents"} {
			if !integer(m, key, 0, 9007199254740991) {
				return false
			}
		}
		received, _ := number(m, "receivedPackets")
		for _, key := range []string{"acceptedPackets", "failedPackets", "droppedPackets"} {
			value, _ := number(m, key)
			if value > received {
				return false
			}
		}
		applied, _ := number(m, "appliedEvents")
		requested, _ := number(m, "requestedEvents")
		return applied <= requested && (!has(m, "lastErrorCode") || integer(m, "lastErrorCode", -2147483648, 2147483647)) && (!has(m, "lastSequence") || integer(m, "lastSequence", 0, 65535))
	case "input-capabilities":
		return host && length <= 256 && exact(m, "type", "protocol", "scancode", "clipboard", "clipboardMaxBytes") && protocol(m, 1) && isBoolean(m, "scancode") && isBoolean(m, "clipboard") && integer(m, "clipboardMaxBytes", 0, 1024*1024)
	case "clipboard", "clipboard-image":
		count, _ := number(m, "count")
		maxCount := float64(64)
		if typ == "clipboard-image" {
			maxCount = 342
		}
		if !host || length > 32*1024*6+256 || !protocol(m, 1) || !uint32Value(m, "id") || !integer(m, "count", 1, maxCount) || !integer(m, "index", 0, count-1) || !boundedString(m, "text", 0, 32*1024) {
			return false
		}
		if typ == "clipboard-image" {
			return exact(m, "count", "id", "index", "mimeType", "protocol", "text", "type") && str(m, "mimeType") == "image/png" && regexp.MustCompile(`^[A-Za-z0-9+/]*={0,2}$`).MatchString(str(m, "text"))
		}
		return exact(m, "count", "id", "index", "protocol", "text", "type")
	case "file-offer":
		return host && validFileOffer(m)
	case "file-drag-preview":
		if !client || length > 8192 || !protocol(m, 1) || str(m, "epoch") != epoch || !integer(m, "dragId", 1, 4294967295) || !only(m, "type", "protocol", "epoch", "dragId", "action", "drop", "types") {
			return false
		}
		if str(m, "action") == "cancel" {
			return !has(m, "drop") && !has(m, "types")
		}
		if str(m, "action") != "update" || !validDrop(object(m, "drop")) {
			return false
		}
		var types []string
		if json.Unmarshal(m["types"], &types) != nil || len(types) < 1 || len(types) > 64 {
			return false
		}
		for _, value := range types {
			if len(value) > 96 || strings.IndexFunc(value, func(c rune) bool { return c < 33 || c > 126 }) >= 0 {
				return false
			}
		}
		return true
	case "media-control":
		if !client || length > 2048 {
			return false
		}
		if protocol(m, 4) && str(m, "epoch") != epoch {
			return false
		}
		switch str(m, "action") {
		case "monitors":
			return protocol(m, 4) && str(m, "epoch") == epoch
		case "monitor":
			return boundedString(m, "monitor", 1, 128)
		case "follow":
			return protocol(m, 4) && str(m, "epoch") == epoch && isBoolean(m, "follow")
		case "start", "stop", "viewport", "profile", "keyframe", "mode", "quality":
			return true
		}
		return false
	case "first-frame":
		if length > 1024 || !protocol(m, 1) || !regexp.MustCompile(`^[A-Za-z0-9_-]{8,64}$`).MatchString(str(m, "requestId")) {
			return false
		}
		switch str(m, "phase") {
		case "request":
			return client && (str(m, "transport") == "webrtc" || str(m, "transport") == "wss") && (!has(m, "epoch") || str(m, "epoch") == epoch)
		case "presented":
			return client && uint32Value(m, "generation")
		case "admitted", "ready":
			return host && uint32Value(m, "generation")
		case "failed":
			return host && uint32Value(m, "generation") && boundedString(m, "reason", 1, 96)
		}
		return false
	case "media-view", "monitor-list", "monitor-error":
		return host && length <= 64*1024
	case "bench-status":
		return host && length <= 16384 && protocol(m, 1)
	}
	return false
}

func validDesktopState(m map[string]json.RawMessage) bool {
	if !only(m, "type", "state", "desktop", "input", "generation", "reason", "secureKind", "secureCapabilities") ||
		(str(m, "state") != "active" && str(m, "state") != "unavailable") || !uint32Value(m, "generation") || !boundedString(m, "reason", 0, 96) {
		return false
	}
	if has(m, "desktop") && str(m, "desktop") != "default" && str(m, "desktop") != "secure" && str(m, "desktop") != "none" {
		return false
	}
	if has(m, "input") && str(m, "input") != "enabled" && str(m, "input") != "suspended" && str(m, "input") != "read-only" {
		return false
	}
	if has(m, "secureKind") && (str(m, "desktop") != "secure" || (str(m, "secureKind") != "uac" && str(m, "secureKind") != "lock" && str(m, "secureKind") != "other")) {
		return false
	}
	if has(m, "secureCapabilities") {
		var capabilities []string
		if str(m, "desktop") != "secure" || json.Unmarshal(m["secureCapabilities"], &capabilities) != nil || capabilities == nil || len(capabilities) > 3 {
			return false
		}
		seen := make(map[string]bool)
		for _, capability := range capabilities {
			if (capability != "view" && capability != "deny" && capability != "input") || seen[capability] {
				return false
			}
			seen[capability] = true
		}
	}
	return true
}
func validDrop(m map[string]json.RawMessage) bool {
	return m != nil && exact(m, "x", "y", "generation", "viewportRevision") && integer(m, "x", 0, 65535) && integer(m, "y", 0, 65535) && integer(m, "generation", 1, 4294967295) && uint32Value(m, "viewportRevision")
}
func validFileMeta(m map[string]json.RawMessage) bool {
	if m == nil || !boundedString(m, "name", 1, 1024) || !integer(m, "size", 0, 2*1024*1024*1024) || !boundedString(m, "mime", 0, 255) || !integer(m, "mtimeMs", 0, 9007199254740991) || !uint32Value(m, "batchId") || !integer(m, "batchCount", 1, 64) {
		return false
	}
	count, _ := number(m, "batchCount")
	if !integer(m, "batchIndex", 0, count-1) {
		return false
	}
	switch str(m, "purpose") {
	case "save", "paste":
		return !has(m, "drop")
	case "drop":
		return validDrop(object(m, "drop"))
	}
	return false
}
func validFileOffer(m map[string]json.RawMessage) bool {
	if !protocol(m, 1) || !uint32Value(m, "offerId") || (str(m, "source") != "clipboard" && str(m, "source") != "sendto") {
		return false
	}
	var items []map[string]json.RawMessage
	if json.Unmarshal(m["items"], &items) != nil || len(items) > 64 {
		return false
	}
	for _, item := range items {
		if !uint32Value(item, "id") || !boundedString(item, "name", 1, 1024) || !integer(item, "size", 0, 2*1024*1024*1024) {
			return false
		}
	}
	return true
}
func validFilePacket(data []byte, role string) bool {
	if len(data) < 16 || len(data) > fileLimit || !bytes.Equal(data[:4], []byte("BPF1")) || data[5] != 0 || data[6] != 0 || data[7] != 0 {
		return false
	}
	kind := data[4]
	if kind < 1 || kind > 8 || kind != 3 && binary.LittleEndian.Uint32(data[12:16]) != 0 {
		return false
	}
	payload := data[16:]
	switch kind {
	case 2:
		return len(payload) == 0
	case 3:
		return len(payload) > 0
	case 4:
		return len(payload) == 8 && binary.LittleEndian.Uint64(payload) <= 9007199254740991
	case 5:
		return len(payload) == 32
	}
	if len(payload) == 0 || len(payload) > 4096 || !utf8.Valid(payload) {
		return false
	}
	var m map[string]json.RawMessage
	if json.Unmarshal(payload, &m) != nil || m == nil {
		return false
	}
	switch kind {
	case 1:
		return validFileMeta(m)
	case 6:
		return boundedString(m, "reason", 0, 256)
	case 7:
		return isBoolean(m, "ok") && (!has(m, "error") || bytes.Equal(m["error"], []byte("null")) || boundedString(m, "error", 0, 256))
	case 8:
		return role == "file-client" && uint32Value(m, "offerId") && uint32Value(m, "id")
	}
	return false
}
