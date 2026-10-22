package main

import (
	"crypto/sha256"
	"encoding/hex"
	"testing"
)

// Compare the production codec and verifier with the independent fixture reader.
func TestRuntimeDeviceKeysContract(t *testing.T) {
	fixture := dkcLoad(t)
	for _, ident := range fixture.IDs {
		pub := dkcB64(t, ident.Public)
		if got := deviceID(pub); got != ident.ID {
			t.Fatalf("derived ID = %q, want %q", got, ident.ID)
		}
		if ident.Parse != nil {
			got, ok := canonicalDeviceID(ident.Parse.Input)
			if !ok || got != ident.Parse.Canonical {
				t.Fatalf("parse = %q, %v", got, ok)
			}
		}
	}
	for _, vector := range fixture.RelayProof.Vectors {
		if vector.Role != "host" {
			continue
		}
		pub, nonce, signature := dkcB64(t, vector.Public), dkcB64(t, vector.Nonce), dkcB64(t, vector.Signature)
		sum := relayProofHash(vector.Role, vector.Origin, nonce, vector.ID, pub)
		if hex.EncodeToString(sum[:]) != vector.SHA256Hex {
			t.Fatal("runtime proof hash differs from fixture")
		}
		want := vector.Expected == "valid"
		if validRegistrationProof(vector.Origin, nonce, vector.ID, pub, signature) != want {
			t.Fatalf("proof %s acceptance differs", vector.Name)
		}
		encoded := dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte(vector.Role), []byte(vector.Origin), nonce, []byte(vector.ID), pub)
		if independent := sha256.Sum256(encoded); sum != independent {
			t.Fatal("runtime proof encoding differs")
		}
	}
}
