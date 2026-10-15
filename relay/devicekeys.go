package main

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"strings"
)

const deviceAlphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

func canonicalDeviceID(input string) (string, bool) {
	if len(input) > 64 {
		return "", false
	}
	s := strings.ToUpper(strings.ReplaceAll(input, "-", ""))
	s = strings.NewReplacer("O", "0", "I", "1", "L", "1").Replace(s)
	if len(s) != 16 {
		return "", false
	}
	for _, c := range s {
		if !strings.ContainsRune(deviceAlphabet, c) {
			return "", false
		}
	}
	return s[:4] + "-" + s[4:8] + "-" + s[8:12] + "-" + s[12:], true
}

func deviceID(public []byte) string {
	sum := sha256.Sum256(public)
	var encoded strings.Builder
	var acc uint32
	var bits uint
	for _, b := range sum[:10] {
		acc = acc<<8 | uint32(b)
		bits += 8
		for bits >= 5 {
			bits -= 5
			encoded.WriteByte(deviceAlphabet[(acc>>bits)&31])
		}
	}
	id, _ := canonicalDeviceID(encoded.String())
	return id
}

func decodeKeyField(value string, size int) ([]byte, bool) {
	if len(value) != (size*8+5)/6 {
		return nil, false
	}
	b, err := base64.RawURLEncoding.Strict().DecodeString(value)
	return b, err == nil && len(b) == size && base64.RawURLEncoding.EncodeToString(b) == value
}

func relayProofHash(role, origin string, nonce []byte, id string, public []byte) [32]byte {
	h := sha256.New()
	for _, field := range [][]byte{[]byte("bubbleplay-relay-v1"), []byte(role), []byte(origin), nonce, []byte(id), public} {
		var n [4]byte
		binary.BigEndian.PutUint32(n[:], uint32(len(field)))
		_, _ = h.Write(n[:])
		_, _ = h.Write(field)
	}
	var sum [32]byte
	copy(sum[:], h.Sum(nil))
	return sum
}

func validRegistrationProof(origin string, nonce []byte, id string, public, signature []byte) bool {
	if len(nonce) != 32 || len(public) != ed25519.PublicKeySize || len(signature) != ed25519.SignatureSize {
		return false
	}
	sum := relayProofHash("host", origin, nonce, id, public)
	return ed25519.Verify(ed25519.PublicKey(public), sum[:], signature)
}
