package main

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/ed25519"
	"crypto/hkdf"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"testing"

	"golang.org/x/crypto/argon2"
)

// Independent check of fixtures/device-keys-contract.json. Every vector is
// recomputed from its inputs with the Go standard library and x/crypto. Expected
// outputs (IDs, keys, hashes, signatures, KDF output, AAD strings, ciphertexts)
// are only compared against, or verified and opened as a check. No computation
// takes an expected output as its input.

const dkcFixturePath = "../fixtures/device-keys-contract.json"

const dkcBase32Alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

type dkcKeyFile map[string]any

type dkcIdentity struct {
	Name      string `json:"name"`
	SeedHex   string `json:"seedHex"`
	Public    string `json:"public"`
	SHA256Hex string `json:"sha256Hex"`
	ID        string `json:"id"`
	Parse     *struct {
		Input     string `json:"input"`
		Canonical string `json:"canonical"`
	} `json:"parse"`
}

type dkcParty struct {
	SeedHex             string `json:"seedHex"`
	ID                  string `json:"id"`
	Public              string `json:"public"`
	Nonce               string `json:"nonce"`
	EphemeralPrivateHex string `json:"ephemeralPrivateHex"`
	Ephemeral           string `json:"ephemeral"`
}

type dkcDTLS struct {
	LocalFingerprint  string `json:"localFingerprint"`
	RemoteFingerprint string `json:"remoteFingerprint"`
	SignedBytesHex    string `json:"signedBytesHex"`
	Signature         string `json:"signature"`
}

type dkcControlMessage struct {
	Direction     string `json:"direction"`
	Sequence      uint64 `json:"sequence"`
	Type          string `json:"type"`
	PlaintextUTF8 string `json:"plaintextUtf8"`
	NonceHex      string `json:"nonceHex"`
	Ciphertext    string `json:"ciphertext"`
}

type dkcSignal struct {
	Type      string   `json:"type"`
	ID        string   `json:"id"`
	Public    string   `json:"public"`
	Nonce     string   `json:"nonce"`
	Ephemeral string   `json:"ephemeral"`
	Caps      []string `json:"caps"`
	Signature string   `json:"signature"`
}

type dkcHandshake struct {
	RelayOrigin          string   `json:"relayOrigin"`
	SessionEpoch         string   `json:"sessionEpoch"`
	Viewer               dkcParty `json:"viewer"`
	PC                   dkcParty `json:"pc"`
	TranscriptHex        string   `json:"transcriptHex"`
	TranscriptHashHex    string   `json:"transcriptHashHex"`
	PCSignedBytesHex     string   `json:"pcSignedBytesHex"`
	PCSignature          string   `json:"pcSignature"`
	ViewerSignedBytesHex string   `json:"viewerSignedBytesHex"`
	ViewerSignature      string   `json:"viewerSignature"`
	SharedSecretHex      string   `json:"sharedSecretHex"`
	ControlKeys          struct {
		ViewerToPCHex string `json:"viewerToPcHex"`
		PCToViewerHex string `json:"pcToViewerHex"`
	} `json:"controlKeys"`
	ControlMessages []dkcControlMessage `json:"controlMessages"`
	DTLS            struct {
		Viewer dkcDTLS `json:"viewer"`
		PC     dkcDTLS `json:"pc"`
	} `json:"dtls"`
	Messages struct {
		AuthHello  dkcSignal `json:"authHello"`
		AuthHost   dkcSignal `json:"authHost"`
		AuthFinish dkcSignal `json:"authFinish"`
		AuthBind   dkcSignal `json:"authBind"`
	} `json:"messages"`
}

type dkcRelayVector struct {
	Name        string `json:"name"`
	Role        string `json:"role"`
	Origin      string `json:"origin"`
	NonceHex    string `json:"nonceHex"`
	Nonce       string `json:"nonce"`
	ID          string `json:"id"`
	Public      string `json:"public"`
	EncodingHex string `json:"encodingHex"`
	SHA256Hex   string `json:"sha256Hex"`
	Signature   string `json:"signature"`
	Expected    string `json:"expected"`
}

type dkcRelayProof struct {
	Vectors  []dkcRelayVector          `json:"vectors"`
	Messages map[string]map[string]any `json:"messages"`
}

type dkcContract struct {
	IDs      []dkcIdentity `json:"ids"`
	KeyFiles struct {
		Plain struct {
			SeedHex string     `json:"seedHex"`
			File    dkcKeyFile `json:"file"`
		} `json:"plain"`
		Passphrase struct {
			SeedHex    string     `json:"seedHex"`
			Passphrase string     `json:"passphrase"`
			KDFKeyHex  string     `json:"kdfKeyHex"`
			AADUTF8    string     `json:"aadUtf8"`
			AADHex     string     `json:"aadHex"`
			File       dkcKeyFile `json:"file"`
		} `json:"passphrase"`
		Failures []struct {
			Name       string     `json:"name"`
			Passphrase string     `json:"passphrase"`
			File       dkcKeyFile `json:"file"`
			Expected   string     `json:"expected"`
		} `json:"failures"`
	} `json:"keyFiles"`
	Handshake  dkcHandshake  `json:"handshake"`
	RelayProof dkcRelayProof `json:"relayProof"`
}

func dkcLoad(t *testing.T) dkcContract {
	t.Helper()
	data, err := os.ReadFile(dkcFixturePath)
	if err != nil {
		t.Fatal(err)
	}
	var fixture dkcContract
	if err := json.Unmarshal(data, &fixture); err != nil {
		t.Fatal(err)
	}
	return fixture
}

func dkcIdentityNamed(t *testing.T, fixture dkcContract, name string) dkcIdentity {
	t.Helper()
	for _, ident := range fixture.IDs {
		if ident.Name == name {
			return ident
		}
	}
	t.Fatalf("no identity %q in fixture", name)
	return dkcIdentity{}
}

func dkcSame(t *testing.T, name, got, want string) {
	t.Helper()
	if got != want {
		t.Errorf("%s:\n got  %s\n want %s", name, got, want)
	}
}

// dkcExactlyOnce requires got to contain each wanted entry exactly once and nothing else.
func dkcExactlyOnce(t *testing.T, what string, got, want []string) {
	t.Helper()
	counts := map[string]int{}
	for _, g := range got {
		counts[g]++
	}
	for _, w := range want {
		if counts[w] != 1 {
			t.Errorf("%s: %q appears %d times, want 1", what, w, counts[w])
		}
	}
	if len(got) != len(want) {
		t.Errorf("%s: %d entries, want %d", what, len(got), len(want))
	}
}

func dkcEnc(b []byte) string {
	return base64.RawURLEncoding.EncodeToString(b)
}

// Unpadded base64url only: a padded or non-canonical field fails here.
func dkcB64(t *testing.T, s string) []byte {
	t.Helper()
	b, err := base64.RawURLEncoding.DecodeString(s)
	if err != nil {
		t.Fatalf("base64url %q: %v", s, err)
	}
	if dkcEnc(b) != s {
		t.Fatalf("base64url %q is not canonical", s)
	}
	return b
}

func dkcHex(t *testing.T, s string) []byte {
	t.Helper()
	b, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("hex %q: %v", s, err)
	}
	return b
}

func dkcConcat(parts ...[]byte) []byte {
	var out []byte
	for _, p := range parts {
		out = append(out, p...)
	}
	return out
}

// dkcLengthPrefixed frames each field with a uint32 big-endian byte length.
func dkcLengthPrefixed(fields ...[]byte) []byte {
	var out []byte
	for _, f := range fields {
		var n [4]byte
		binary.BigEndian.PutUint32(n[:], uint32(len(f)))
		out = dkcConcat(out, n[:], f)
	}
	return out
}

func dkcPrivate(t *testing.T, seedHex string) ed25519.PrivateKey {
	t.Helper()
	seed := dkcHex(t, seedHex)
	if len(seed) != ed25519.SeedSize {
		t.Fatalf("seed has %d bytes, want %d", len(seed), ed25519.SeedSize)
	}
	return ed25519.NewKeyFromSeed(seed)
}

func dkcPub(priv ed25519.PrivateKey) ed25519.PublicKey {
	return priv.Public().(ed25519.PublicKey)
}

func dkcX25519(t *testing.T, privHex string) *ecdh.PrivateKey {
	t.Helper()
	key, err := ecdh.X25519().NewPrivateKey(dkcHex(t, privHex))
	if err != nil {
		t.Fatal(err)
	}
	return key
}

// dkcBase32 is Crockford base32 without the group separators.
func dkcBase32(b []byte) string {
	var out []byte
	var acc uint32
	var bits uint
	for _, c := range b {
		acc = acc<<8 | uint32(c)
		bits += 8
		for bits >= 5 {
			bits -= 5
			out = append(out, dkcBase32Alphabet[(acc>>bits)&31])
		}
	}
	if bits > 0 {
		out = append(out, dkcBase32Alphabet[(acc<<(5-bits))&31])
	}
	return string(out)
}

// dkcGroupID turns 16 base32 characters into the four-group form.
func dkcGroupID(s string) string {
	return s[0:4] + "-" + s[4:8] + "-" + s[8:12] + "-" + s[12:16]
}

// dkcDeviceID is the first 80 bits of SHA-256 over the raw public key.
func dkcDeviceID(pub []byte) string {
	sum := sha256.Sum256(pub)
	return dkcGroupID(dkcBase32(sum[:10]))
}

// dkcParseID accepts hyphens, any case, and O/I/L for 0/1/1, then returns the canonical form.
func dkcParseID(t *testing.T, input string) string {
	t.Helper()
	var digits []byte
	for i := 0; i < len(input); i++ {
		c := input[i]
		if c == '-' {
			continue
		}
		if c >= 'a' && c <= 'z' {
			c -= 'a' - 'A'
		}
		switch c {
		case 'O':
			c = '0'
		case 'I', 'L':
			c = '1'
		}
		if strings.IndexByte(dkcBase32Alphabet, c) < 0 {
			t.Fatalf("invalid Crockford character %q in %q", c, input)
		}
		digits = append(digits, c)
	}
	if len(digits) != 16 {
		t.Fatalf("%q has %d characters after removing hyphens, want 16", input, len(digits))
	}
	return dkcGroupID(string(digits))
}

func dkcArgon2(passphrase string, salt []byte) []byte {
	return argon2.IDKey([]byte(passphrase), salt, 3, 64*1024, 1, 32)
}

func dkcGCM(t *testing.T, key []byte) cipher.AEAD {
	t.Helper()
	block, err := aes.NewCipher(key)
	if err != nil {
		t.Fatal(err)
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		t.Fatal(err)
	}
	return gcm
}

func dkcHKDF(t *testing.T, secret, salt []byte, info string) []byte {
	t.Helper()
	key, err := hkdf.Key(sha256.New, secret, salt, info, 32)
	if err != nil {
		t.Fatal(err)
	}
	return key
}

func dkcStr(t *testing.T, m map[string]any, key string) string {
	t.Helper()
	s, ok := m[key].(string)
	if !ok {
		t.Fatalf("field %q is %T, want string", key, m[key])
	}
	return s
}

func dkcMap(t *testing.T, m map[string]any, key string) map[string]any {
	t.Helper()
	v, ok := m[key].(map[string]any)
	if !ok {
		t.Fatalf("field %q is %T, want object", key, m[key])
	}
	return v
}

// dkcCanonical is RFC 8785 for the value shapes in this fixture. Object keys
// here are ASCII, so byte order matches the required UTF-16 order.
func dkcCanonical(t *testing.T, value any) string {
	t.Helper()
	var b strings.Builder
	dkcWriteCanonical(t, &b, value)
	return b.String()
}

func dkcWriteCanonical(t *testing.T, b *strings.Builder, value any) {
	t.Helper()
	switch v := value.(type) {
	case nil:
		b.WriteString("null")
	case bool:
		b.WriteString(strconv.FormatBool(v))
	case float64:
		b.WriteString(strconv.FormatFloat(v, 'f', -1, 64))
	case string:
		dkcWriteString(b, v)
	case []any:
		b.WriteByte('[')
		for i, item := range v {
			if i > 0 {
				b.WriteByte(',')
			}
			dkcWriteCanonical(t, b, item)
		}
		b.WriteByte(']')
	case map[string]any:
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		b.WriteByte('{')
		for i, k := range keys {
			if i > 0 {
				b.WriteByte(',')
			}
			dkcWriteString(b, k)
			b.WriteByte(':')
			dkcWriteCanonical(t, b, v[k])
		}
		b.WriteByte('}')
	default:
		t.Fatalf("unsupported JSON value %T", value)
	}
}

func dkcWriteString(b *strings.Builder, s string) {
	b.WriteByte('"')
	for _, r := range s {
		switch {
		case r == '"':
			b.WriteString(`\"`)
		case r == '\\':
			b.WriteString(`\\`)
		case r == '\b':
			b.WriteString(`\b`)
		case r == '\f':
			b.WriteString(`\f`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\t':
			b.WriteString(`\t`)
		case r < 0x20:
			fmt.Fprintf(b, `\u%04x`, r)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
}

// dkcAAD is the key file's associated data: canonical JSON without private and
// aead.nonce. aead.alg and the kdf object stay in.
func dkcAAD(t *testing.T, file dkcKeyFile) string {
	t.Helper()
	aad := map[string]any{}
	for k, v := range file {
		if k == "private" {
			continue
		}
		if k == "aead" {
			aead, ok := v.(map[string]any)
			if !ok {
				t.Fatalf("aead is %T, want object", v)
			}
			trimmed := map[string]any{}
			for ak, av := range aead {
				if ak != "nonce" {
					trimmed[ak] = av
				}
			}
			v = trimmed
		}
		aad[k] = v
	}
	return dkcCanonical(t, aad)
}

// dkcCheckFileIdentity checks a key file's header fields against its public key.
func dkcCheckFileIdentity(t *testing.T, name string, file dkcKeyFile, pub ed25519.PublicKey) {
	t.Helper()
	dkcSame(t, name+" format", dkcStr(t, file, "format"), "bubbleplay-device-key")
	dkcSame(t, name+" type", dkcStr(t, file, "type"), "ed25519")
	if file["version"] != float64(1) {
		t.Errorf("%s version is %v, want 1", name, file["version"])
	}
	dkcSame(t, name+" public", dkcStr(t, file, "public"), dkcEnc(pub))
	dkcSame(t, name+" id from public", dkcStr(t, file, "id"), dkcDeviceID(pub))
}

func TestDeviceKeysContractIDs(t *testing.T) {
	fixture := dkcLoad(t)
	var names []string
	for _, ident := range fixture.IDs {
		names = append(names, ident.Name)
		if ident.Name == "crockford-aliases" && ident.Parse == nil {
			t.Errorf("crockford-aliases has no parse vector")
		}
	}
	dkcExactlyOnce(t, "ids", names, []string{"viewer", "pc", "crockford-aliases"})
	for _, ident := range fixture.IDs {
		priv := dkcPrivate(t, ident.SeedHex)
		pub := dkcPub(priv)
		dkcSame(t, ident.Name+" public", ident.Public, dkcEnc(pub))
		sum := sha256.Sum256(pub)
		dkcSame(t, ident.Name+" sha256", ident.SHA256Hex, hex.EncodeToString(sum[:]))
		dkcSame(t, ident.Name+" id", ident.ID, dkcDeviceID(pub))
		if ident.Parse != nil {
			dkcSame(t, ident.Name+" parse input "+ident.Parse.Input, dkcParseID(t, ident.Parse.Input), ident.Parse.Canonical)
			dkcSame(t, ident.Name+" parse canonical vs id", ident.Parse.Canonical, ident.ID)
		}
		dkcSame(t, ident.Name+" canonical round trip", dkcParseID(t, ident.ID), ident.ID)
	}
}

func TestDeviceKeysContractKeyFiles(t *testing.T) {
	fixture := dkcLoad(t)

	plain := fixture.KeyFiles.Plain
	plainPub := dkcPub(dkcPrivate(t, plain.SeedHex))
	if plain.File["kdf"] != nil || plain.File["aead"] != nil {
		t.Errorf("plain key file has kdf or aead, want null")
	}
	dkcCheckFileIdentity(t, "plain", plain.File, plainPub)
	plainPrivate := dkcB64(t, dkcStr(t, plain.File, "private"))
	if len(plainPrivate) != 32 {
		t.Errorf("plain private is %d bytes, want 32", len(plainPrivate))
	}
	dkcSame(t, "plain private", hex.EncodeToString(plainPrivate), plain.SeedHex)

	pp := fixture.KeyFiles.Passphrase
	dkcCheckFileIdentity(t, "passphrase", pp.File, dkcPub(dkcPrivate(t, pp.SeedHex)))
	kdf := dkcMap(t, pp.File, "kdf")
	if dkcStr(t, kdf, "alg") != "argon2id" || kdf["m"] != float64(65536) || kdf["t"] != float64(3) || kdf["p"] != float64(1) {
		t.Errorf("kdf is %v, want argon2id m=65536 t=3 p=1", kdf)
	}
	salt := dkcB64(t, dkcStr(t, kdf, "salt"))
	if len(salt) != 16 {
		t.Errorf("salt is %d bytes, want 16", len(salt))
	}
	aead := dkcMap(t, pp.File, "aead")
	if dkcStr(t, aead, "alg") != "aes-256-gcm" {
		t.Errorf("aead alg is %q, want aes-256-gcm", dkcStr(t, aead, "alg"))
	}
	nonce := dkcB64(t, dkcStr(t, aead, "nonce"))
	if len(nonce) != 12 {
		t.Errorf("nonce is %d bytes, want 12", len(nonce))
	}
	key := dkcArgon2(pp.Passphrase, salt)
	dkcSame(t, "passphrase kdfKey", hex.EncodeToString(key), pp.KDFKeyHex)
	aad := dkcAAD(t, pp.File)
	dkcSame(t, "passphrase AAD", aad, pp.AADUTF8)
	dkcSame(t, "passphrase AAD hex", hex.EncodeToString([]byte(aad)), pp.AADHex)
	sealed := dkcB64(t, dkcStr(t, pp.File, "private"))
	if len(sealed) != 48 {
		t.Errorf("protected private is %d bytes, want 48 (32-byte seed plus 16-byte tag)", len(sealed))
	}
	seed, err := dkcGCM(t, key).Open(nil, nonce, sealed, []byte(aad))
	if err != nil {
		t.Errorf("passphrase file does not decrypt: %v", err)
	} else {
		dkcSame(t, "passphrase seed", hex.EncodeToString(seed), pp.SeedHex)
	}

	var failureNames []string
	for _, f := range fixture.KeyFiles.Failures {
		failureNames = append(failureNames, f.Name)
		dkcSame(t, f.Name+" expected", f.Expected, "authentication-failed")
		fkdf := dkcMap(t, f.File, "kdf")
		fsalt := dkcB64(t, dkcStr(t, fkdf, "salt"))
		fnonce := dkcB64(t, dkcStr(t, dkcMap(t, f.File, "aead"), "nonce"))
		fsealed := dkcB64(t, dkcStr(t, f.File, "private"))
		if _, err := dkcGCM(t, dkcArgon2(f.Passphrase, fsalt)).Open(nil, fnonce, fsealed, []byte(dkcAAD(t, f.File))); err == nil {
			t.Errorf("%s: decrypted, want authentication failure", f.Name)
		}
		// Positive control: the failing file's salt, nonce and ciphertext must be the
		// passphrase file's own, and they must open with the right passphrase and the
		// AAD computed from the passphrase file (aad above). The failing case differs
		// from this only by its passphrase or its AAD.
		dkcSame(t, f.Name+" salt matches passphrase file", dkcEnc(fsalt), dkcEnc(salt))
		dkcSame(t, f.Name+" nonce matches passphrase file", dkcEnc(fnonce), dkcEnc(nonce))
		dkcSame(t, f.Name+" ciphertext matches passphrase file", dkcEnc(fsealed), dkcEnc(sealed))
		if _, err := dkcGCM(t, dkcArgon2(pp.Passphrase, fsalt)).Open(nil, fnonce, fsealed, []byte(aad)); err != nil {
			t.Errorf("%s: correct inputs do not decrypt: %v", f.Name, err)
		}
	}
	dkcExactlyOnce(t, "failures", failureNames, []string{"wrong-passphrase", "tampered-aad"})
}

func TestDeviceKeysContractHandshake(t *testing.T) {
	fixture := dkcLoad(t)
	hs := fixture.Handshake

	viewerKey, viewerEph := dkcCheckParty(t, "viewer", hs.Viewer, dkcIdentityNamed(t, fixture, "viewer"))
	pcKey, pcEph := dkcCheckParty(t, "pc", hs.PC, dkcIdentityNamed(t, fixture, "pc"))
	viewerPub := dkcPub(viewerKey)
	pcPub := dkcPub(pcKey)
	viewerEphPub := viewerEph.PublicKey().Bytes()
	pcEphPub := pcEph.PublicKey().Bytes()
	viewerNonce := dkcB64(t, hs.Viewer.Nonce)
	pcNonce := dkcB64(t, hs.PC.Nonce)

	// The epoch's UTF-8 decimal bytes go into the transcript as-is; no integer range is imposed.
	if !regexp.MustCompile(`^[0-9]+$`).MatchString(hs.SessionEpoch) {
		t.Errorf("sessionEpoch %q is not a decimal string", hs.SessionEpoch)
	}

	transcript := dkcConcat()
	for _, field := range [][]byte{
		[]byte("bubbleplay-auth-v1"),
		[]byte(hs.RelayOrigin),
		[]byte(hs.SessionEpoch),
		[]byte(dkcDeviceID(viewerPub)),
		viewerPub,
		[]byte(dkcDeviceID(pcPub)),
		pcPub,
		viewerNonce,
		pcNonce,
		viewerEphPub,
		pcEphPub,
	} {
		var n [4]byte
		binary.BigEndian.PutUint32(n[:], uint32(len(field)))
		transcript = dkcConcat(transcript, n[:], field)
	}
	dkcSame(t, "transcript", hex.EncodeToString(transcript), hs.TranscriptHex)
	digest := sha256.Sum256(transcript)
	dkcSame(t, "transcript hash", hex.EncodeToString(digest[:]), hs.TranscriptHashHex)

	pcSigned := dkcConcat([]byte("bubbleplay-auth-host-v1"), digest[:])
	dkcSame(t, "pc signed bytes", hex.EncodeToString(pcSigned), hs.PCSignedBytesHex)
	pcSig := ed25519.Sign(pcKey, pcSigned)
	dkcSame(t, "pc signature", dkcEnc(pcSig), hs.PCSignature)
	if !ed25519.Verify(pcPub, pcSigned, dkcB64(t, hs.PCSignature)) {
		t.Errorf("fixture pc signature does not verify over the recomputed bytes")
	}

	viewerSigned := dkcConcat([]byte("bubbleplay-auth-viewer-v1"), digest[:], pcSig)
	dkcSame(t, "viewer signed bytes", hex.EncodeToString(viewerSigned), hs.ViewerSignedBytesHex)
	viewerSig := ed25519.Sign(viewerKey, viewerSigned)
	dkcSame(t, "viewer signature", dkcEnc(viewerSig), hs.ViewerSignature)
	if !ed25519.Verify(viewerPub, viewerSigned, dkcB64(t, hs.ViewerSignature)) {
		t.Errorf("fixture viewer signature does not verify over the recomputed bytes")
	}

	shared, err := viewerEph.ECDH(pcEph.PublicKey())
	if err != nil {
		t.Fatal(err)
	}
	sharedFromPC, err := pcEph.ECDH(viewerEph.PublicKey())
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(shared, sharedFromPC) {
		t.Errorf("X25519 shared secret differs between the two sides")
	}
	dkcSame(t, "shared secret", hex.EncodeToString(shared), hs.SharedSecretHex)

	viewerToPC := dkcHKDF(t, shared, digest[:], "bubbleplay-ctl-v1 viewer-to-pc")
	pcToViewer := dkcHKDF(t, shared, digest[:], "bubbleplay-ctl-v1 pc-to-viewer")
	dkcSame(t, "viewer-to-pc key", hex.EncodeToString(viewerToPC), hs.ControlKeys.ViewerToPCHex)
	dkcSame(t, "pc-to-viewer key", hex.EncodeToString(pcToViewer), hs.ControlKeys.PCToViewerHex)
	if bytes.Equal(viewerToPC, pcToViewer) {
		t.Errorf("viewer-to-pc and pc-to-viewer control keys are equal")
	}

	var pairs []string
	for _, m := range hs.ControlMessages {
		pairs = append(pairs, fmt.Sprintf("%s/%d", m.Direction, m.Sequence))
	}
	dkcExactlyOnce(t, "control messages", pairs, []string{"viewer-to-pc/0", "viewer-to-pc/1", "pc-to-viewer/0", "pc-to-viewer/1"})

	keys := map[string][]byte{"viewer-to-pc": viewerToPC, "pc-to-viewer": pcToViewer}
	for i, m := range hs.ControlMessages {
		key, ok := keys[m.Direction]
		if !ok {
			t.Fatalf("control message %d: unknown direction %q", i, m.Direction)
		}
		nonce := make([]byte, 12)
		binary.BigEndian.PutUint64(nonce[4:], m.Sequence)
		dkcSame(t, fmt.Sprintf("control %d nonce", i), hex.EncodeToString(nonce), m.NonceHex)
		var plain map[string]any
		if err := json.Unmarshal([]byte(m.PlaintextUTF8), &plain); err != nil {
			t.Fatalf("control %d plaintext: %v", i, err)
		}
		if plain["type"] != m.Type {
			t.Errorf("control %d plaintext type is %v, want %q", i, plain["type"], m.Type)
		}
		sealed := dkcGCM(t, key).Seal(nil, nonce, []byte(m.PlaintextUTF8), []byte(m.Type))
		dkcSame(t, fmt.Sprintf("control %d ciphertext", i), dkcEnc(sealed), m.Ciphertext)
		opened, err := dkcGCM(t, key).Open(nil, nonce, dkcB64(t, m.Ciphertext), []byte(m.Type))
		if err != nil {
			t.Errorf("control %d ciphertext does not open: %v", i, err)
		} else if !bytes.Equal(opened, []byte(m.PlaintextUTF8)) {
			t.Errorf("control %d opens to %q, want %q", i, opened, m.PlaintextUTF8)
		}
	}

	fingerprint := regexp.MustCompile(`^sha-256 ([0-9A-F]{2}:){31}[0-9A-F]{2}$`)
	dtls := []struct {
		name string
		seed ed25519.PrivateKey
		pub  ed25519.PublicKey
		d    dkcDTLS
	}{
		{"viewer", viewerKey, viewerPub, hs.DTLS.Viewer},
		{"pc", pcKey, pcPub, hs.DTLS.PC},
	}
	for _, c := range dtls {
		if !fingerprint.MatchString(c.d.LocalFingerprint) || !fingerprint.MatchString(c.d.RemoteFingerprint) {
			t.Errorf("%s DTLS fingerprint is not sha-256 colon-hex", c.name)
		}
		signed := dkcConcat([]byte("bubbleplay-dtls-v1"), digest[:], []byte(c.d.LocalFingerprint+"\n"+c.d.RemoteFingerprint))
		dkcSame(t, c.name+" DTLS signed bytes", hex.EncodeToString(signed), c.d.SignedBytesHex)
		dkcSame(t, c.name+" DTLS signature", dkcEnc(ed25519.Sign(c.seed, signed)), c.d.Signature)
		if !ed25519.Verify(c.pub, signed, dkcB64(t, c.d.Signature)) {
			t.Errorf("%s DTLS signature does not verify over the recomputed bytes", c.name)
		}
	}
	dkcSame(t, "viewer local fingerprint is pc remote", hs.DTLS.Viewer.LocalFingerprint, hs.DTLS.PC.RemoteFingerprint)
	dkcSame(t, "pc local fingerprint is viewer remote", hs.DTLS.PC.LocalFingerprint, hs.DTLS.Viewer.RemoteFingerprint)

	msgs := hs.Messages
	dkcSame(t, "authHello type", msgs.AuthHello.Type, "auth-hello")
	dkcSame(t, "authHello id", msgs.AuthHello.ID, dkcDeviceID(viewerPub))
	dkcSame(t, "authHello public", msgs.AuthHello.Public, dkcEnc(viewerPub))
	dkcSame(t, "authHello nonce", msgs.AuthHello.Nonce, hs.Viewer.Nonce)
	dkcSame(t, "authHello ephemeral", msgs.AuthHello.Ephemeral, dkcEnc(viewerEphPub))
	dkcSame(t, "authHost type", msgs.AuthHost.Type, "auth-host")
	dkcSame(t, "authHost id", msgs.AuthHost.ID, dkcDeviceID(pcPub))
	dkcSame(t, "authHost public", msgs.AuthHost.Public, dkcEnc(pcPub))
	dkcSame(t, "authHost nonce", msgs.AuthHost.Nonce, hs.PC.Nonce)
	dkcSame(t, "authHost ephemeral", msgs.AuthHost.Ephemeral, dkcEnc(pcEphPub))
	dkcSame(t, "authHost signature", msgs.AuthHost.Signature, dkcEnc(pcSig))
	dkcSame(t, "authFinish type", msgs.AuthFinish.Type, "auth-finish")
	dkcSame(t, "authFinish signature", msgs.AuthFinish.Signature, dkcEnc(viewerSig))
	dkcSame(t, "authBind type", msgs.AuthBind.Type, "auth-bind")
	dkcSame(t, "authBind signature is viewer DTLS signature", msgs.AuthBind.Signature, hs.DTLS.Viewer.Signature)
}

// dkcCheckParty recomputes a party's public key, ID, ephemeral key and nonce length.
func dkcCheckParty(t *testing.T, name string, party dkcParty, ident dkcIdentity) (ed25519.PrivateKey, *ecdh.PrivateKey) {
	t.Helper()
	dkcSame(t, name+" seedHex", party.SeedHex, ident.SeedHex)
	priv := dkcPrivate(t, party.SeedHex)
	pub := dkcPub(priv)
	dkcSame(t, name+" public", party.Public, dkcEnc(pub))
	dkcSame(t, name+" id from public", party.ID, dkcDeviceID(pub))
	dkcSame(t, name+" id from identity", party.ID, ident.ID)
	if n := len(dkcB64(t, party.Nonce)); n != 32 {
		t.Errorf("%s nonce is %d bytes, want 32", name, n)
	}
	eph := dkcX25519(t, party.EphemeralPrivateHex)
	dkcSame(t, name+" ephemeral", party.Ephemeral, dkcEnc(eph.PublicKey().Bytes()))
	return priv, eph
}

func TestDeviceKeysContractRelayProof(t *testing.T) {
	fixture := dkcLoad(t)
	hs := fixture.Handshake
	rp := fixture.RelayProof

	var names []string
	byName := map[string]dkcRelayVector{}
	for _, v := range rp.Vectors {
		names = append(names, v.Name)
		byName[v.Name] = v
	}
	dkcExactlyOnce(t, "relay vectors", names, []string{"host", "viewer", "wrong-origin"})
	identityOf := map[string]string{"host": "pc", "viewer": "viewer", "wrong-origin": "pc"}
	roleOf := map[string]string{"host": "host", "viewer": "viewer", "wrong-origin": "host"}
	expectedOf := map[string]string{"host": "valid", "viewer": "valid", "wrong-origin": "invalid"}

	type relayCheck struct {
		pub      ed25519.PublicKey
		sum      []byte
		sig      []byte
		encoding []byte
	}
	checks := map[string]relayCheck{}
	for _, v := range rp.Vectors {
		ident := dkcIdentityNamed(t, fixture, identityOf[v.Name])
		priv := dkcPrivate(t, ident.SeedHex)
		pub := dkcPub(priv)
		dkcSame(t, v.Name+" role", v.Role, roleOf[v.Name])
		dkcSame(t, v.Name+" expected", v.Expected, expectedOf[v.Name])
		dkcSame(t, v.Name+" id from identity", v.ID, ident.ID)
		dkcSame(t, v.Name+" id from public", v.ID, dkcDeviceID(pub))
		dkcSame(t, v.Name+" public", v.Public, dkcEnc(pub))
		nonce := dkcB64(t, v.Nonce)
		if len(nonce) != 32 {
			t.Errorf("%s nonce is %d bytes, want 32", v.Name, len(nonce))
		}
		dkcSame(t, v.Name+" nonceHex", hex.EncodeToString(nonce), v.NonceHex)
		if v.Expected == "valid" {
			dkcSame(t, v.Name+" origin", v.Origin, hs.RelayOrigin)
		} else {
			dkcSame(t, v.Name+" origin", v.Origin, "https://other-relay.example.test")
		}
		encoding := dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte(v.Role), []byte(v.Origin), nonce, []byte(v.ID), pub)
		dkcSame(t, v.Name+" encoding", hex.EncodeToString(encoding), v.EncodingHex)
		sum := sha256.Sum256(encoding)
		dkcSame(t, v.Name+" sha256", hex.EncodeToString(sum[:]), v.SHA256Hex)
		sig := dkcB64(t, v.Signature)
		if len(sig) != 64 {
			t.Errorf("%s signature is %d bytes, want 64", v.Name, len(sig))
		}
		if v.Expected == "valid" {
			dkcSame(t, v.Name+" signature", dkcEnc(ed25519.Sign(priv, sum[:])), v.Signature)
			if !ed25519.Verify(pub, sum[:], sig) {
				t.Errorf("%s signature does not verify over the recomputed hash", v.Name)
			}
		} else if ed25519.Verify(pub, sum[:], sig) {
			t.Errorf("%s signature verifies over the changed encoding, want rejection", v.Name)
		}
		checks[v.Name] = relayCheck{pub: pub, sum: sum[:], sig: sig, encoding: encoding}
	}

	host, wrong := checks["host"], checks["wrong-origin"]
	hostVec, wrongVec := byName["host"], byName["wrong-origin"]
	dkcSame(t, "wrong-origin signature", wrongVec.Signature, hostVec.Signature)
	dkcSame(t, "wrong-origin role", wrongVec.Role, hostVec.Role)
	dkcSame(t, "wrong-origin nonce", wrongVec.Nonce, hostVec.Nonce)
	dkcSame(t, "wrong-origin id", wrongVec.ID, hostVec.ID)
	dkcSame(t, "wrong-origin public", wrongVec.Public, hostVec.Public)
	if bytes.Equal(wrong.encoding, host.encoding) {
		t.Errorf("wrong-origin encoding equals the host encoding")
	}
	// The host signature covers the original encoding and must not cover the substituted origin.
	if !ed25519.Verify(host.pub, host.sum, host.sig) {
		t.Errorf("host signature does not verify over the original encoding")
	}
	if ed25519.Verify(host.pub, wrong.sum, host.sig) {
		t.Errorf("host signature verifies over the substituted origin")
	}

	// Changing role, ID or nonce alone must break the host signature.
	viewerID := dkcIdentityNamed(t, fixture, "viewer").ID
	hostNonce := dkcB64(t, hostVec.Nonce)
	flipped := append([]byte(nil), hostNonce...)
	flipped[0] ^= 1
	mutants := map[string][]byte{
		"role":  dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte("viewer"), []byte(hs.RelayOrigin), hostNonce, []byte(hostVec.ID), host.pub),
		"id":    dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte("host"), []byte(hs.RelayOrigin), hostNonce, []byte(viewerID), host.pub),
		"nonce": dkcLengthPrefixed([]byte("bubbleplay-relay-v1"), []byte("host"), []byte(hs.RelayOrigin), flipped, []byte(hostVec.ID), host.pub),
	}
	for name, encoding := range mutants {
		sum := sha256.Sum256(encoding)
		if ed25519.Verify(host.pub, sum[:], host.sig) {
			t.Errorf("%s mutation still verifies with the host signature", name)
		}
	}

	var keys []string
	for k := range rp.Messages {
		keys = append(keys, k)
	}
	dkcExactlyOnce(t, "relay messages", keys, []string{"hello", "registerChallenge", "registerProof", "helloOk"})
	wantShapes := map[string]map[string]any{
		"hello":             {"type": "hello", "role": "host", "token": "test-only-registration-token", "id": hostVec.ID, "public": hostVec.Public},
		"registerChallenge": {"type": "register-challenge", "nonce": hostVec.Nonce},
		"registerProof":     {"type": "register-proof", "signature": hostVec.Signature},
	}
	for name, want := range wantShapes {
		if !reflect.DeepEqual(rp.Messages[name], want) {
			t.Errorf("relay message %s:\n got  %v\n want %v", name, rp.Messages[name], want)
		}
	}
	if n := len(rp.Messages["helloOk"]); n != 2 {
		t.Errorf("helloOk has %d fields, want 2 (type, ticket)", n)
	}
	dkcSame(t, "helloOk type", dkcStr(t, rp.Messages["helloOk"], "type"), "hello-ok")
	if n := len(dkcB64(t, dkcStr(t, rp.Messages["helloOk"], "ticket"))); n != 32 {
		t.Errorf("helloOk ticket is %d bytes, want 32", n)
	}
}
