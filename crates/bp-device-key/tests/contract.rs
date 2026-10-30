//! Checks this crate against two contract fixtures.
//!
//! - `fixtures/shared/device-keys-contract.json`, vendored from the shared
//!   `bubblebuffer-device-key` crate: IDs, key files in the shared format and relay proofs with
//!   neutral labels. Never edited here.
//! - `fixtures/device-keys-contract.json`, BubblePlay's own: the relay proofs with
//!   `bubbleplay-relay-v1` (the bytes the live relay verifies), the handshake origin and phase 2.
//!
//! No expected output is used as an input: every vector is recomputed from its seed, nonce and
//! origin.

use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::{
    relay_origin, relay_proof_encoding, relay_proof_hash, verify_relay_proof, DeviceId,
    DeviceIdentity, DeviceKeyError, Role,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;

fn read_fixture(relative: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("fixture parses")
}

/// BubblePlay's own fixture.
fn fixture() -> Value {
    read_fixture("../../fixtures/device-keys-contract.json")
}

/// The shared crate's fixture, vendored unchanged.
fn shared_fixture() -> Value {
    read_fixture("../../fixtures/shared/device-keys-contract.json")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn b64(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text).expect("base64url")
}

fn seed_of(seed_hex: &str) -> [u8; 32] {
    unhex(seed_hex).try_into().expect("32-byte seed")
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("field {key} is not a string"))
}

fn seed_hex_for(fixture: &Value, id: &str) -> String {
    fixture["ids"]
        .as_array()
        .expect("ids")
        .iter()
        .find(|identity| identity["id"] == id)
        .map(|identity| field(identity, "seedHex").to_owned())
        .unwrap_or_else(|| panic!("no identity with id {id}"))
}

#[test]
fn contract_ids() {
    let fixture = fixture();
    let identities = fixture["ids"].as_array().expect("ids");
    let names: Vec<&str> = identities
        .iter()
        .map(|identity| field(identity, "name"))
        .collect();
    assert_eq!(names, ["viewer", "pc", "crockford-aliases"]);
    for identity in identities {
        let seed = seed_of(field(identity, "seedHex"));
        let owner = DeviceIdentity::from_seed(&seed);
        assert_eq!(owner.public_b64(), field(identity, "public"));
        let id = DeviceId::from_public_key(&owner.public_key());
        let canonical = field(identity, "id");
        assert_eq!(id.to_string(), canonical);
        assert_eq!(owner.id(), id);
        for spelling in [
            canonical.to_owned(),
            canonical.to_lowercase(),
            canonical.replace('-', ""),
        ] {
            assert_eq!(
                DeviceId::parse(&spelling).expect("parse"),
                id,
                "spelling {spelling:?}"
            );
        }
        if let Some(parse) = identity.get("parse") {
            let parsed = DeviceId::parse(field(parse, "input")).expect("alias input");
            assert_eq!(parsed.to_string(), field(parse, "canonical"));
            assert_eq!(parsed.to_string(), canonical);
        }
    }
    // ASCII only: no non-ASCII symbol is ever an ID character.
    assert!(matches!(
        DeviceId::parse("é7QK2M9XH4PDR8T").map_err(DeviceKeyError::from),
        Err(DeviceKeyError::InvalidId)
    ));
}

#[test]
fn contract_relay_proof_vectors() {
    let fixture = fixture();
    let vectors = fixture["relayProof"]["vectors"]
        .as_array()
        .expect("vectors");
    let names: Vec<&str> = vectors.iter().map(|vector| field(vector, "name")).collect();
    assert_eq!(names, ["host", "viewer", "wrong-origin"]);
    for vector in vectors {
        let role = Role::parse(field(vector, "role")).expect("role");
        let nonce: [u8; 32] = b64(field(vector, "nonce"))
            .try_into()
            .expect("32-byte nonce");
        let origin = field(vector, "origin");
        let id = field(vector, "id");
        let public: [u8; 32] = b64(field(vector, "public"))
            .try_into()
            .expect("32-byte public key");
        let signature = b64(field(vector, "signature"));

        assert_eq!(
            hex(
                &relay_proof_encoding(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public)
                    .unwrap()
            ),
            field(vector, "encodingHex")
        );
        assert_eq!(
            hex(
                &relay_proof_hash(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public)
                    .unwrap()
            ),
            field(vector, "sha256Hex")
        );

        let verified = verify_relay_proof(role, origin, &nonce, id, &public, &signature);
        match field(vector, "expected") {
            "valid" => {
                // The signature is reproduced from the seed the fixture names for that ID.
                let seed = seed_of(&seed_hex_for(&fixture, id));
                let identity = DeviceIdentity::from_seed(&seed);
                assert_eq!(identity.id().to_string(), id);
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(identity.sign_relay_proof(role, origin, &nonce)),
                    field(vector, "signature")
                );
                assert_eq!(verified.expect("valid proof").to_string(), id);
            }
            // The fixture signature was made over the original origin, so a re-signed value
            // for the substituted origin would not match it. Check that it fails to verify.
            "invalid" => {
                assert!(matches!(verified, Err(DeviceKeyError::InvalidSignature)));
                let host = vector_named(&fixture, "host");
                assert_eq!(field(vector, "signature"), field(&host, "signature"));
                assert_ne!(field(vector, "origin"), field(&host, "origin"));
            }
            other => panic!("unknown expectation {other}"),
        }
    }
}

fn vector_named(fixture: &Value, name: &str) -> Value {
    fixture["relayProof"]["vectors"]
        .as_array()
        .expect("vectors")
        .iter()
        .find(|vector| vector["name"] == name)
        .unwrap_or_else(|| panic!("no vector named {name}"))
        .clone()
}

#[test]
fn contract_relay_proof_messages_use_the_same_signature() {
    let fixture = fixture();
    let proof = &fixture["relayProof"]["messages"];
    assert_eq!(field(&proof["registerProof"], "type"), "register-proof");
    let host = fixture["relayProof"]["vectors"]
        .as_array()
        .expect("vectors")
        .iter()
        .find(|vector| vector["name"] == "host")
        .expect("host vector");
    assert_eq!(
        field(&proof["registerProof"], "signature"),
        field(host, "signature")
    );
    let challenge = &proof["registerChallenge"];
    assert_eq!(field(challenge, "type"), "register-challenge");
    assert_eq!(field(challenge, "nonce"), field(host, "nonce"));
    let hello = &proof["hello"];
    assert_eq!(field(hello, "id"), field(host, "id"));
    assert_eq!(field(hello, "public"), field(host, "public"));
    assert_eq!(Role::parse(field(hello, "role")).expect("role"), Role::Host);
}

#[test]
fn contract_relay_origin_canonicalises_the_handshake_origin() {
    let fixture = fixture();
    let origin = field(&fixture["handshake"], "relayOrigin");
    assert_eq!(origin, "https://example.test");
    // The fixture's own relay origin is already in canonical form, and the worker URLs the host
    // and relay use all reduce to it.
    assert_eq!(
        relay_origin("https://example.test").as_deref(),
        Some(origin)
    );
    assert_eq!(
        relay_origin("wss://example.test/host-ws?pc=x").as_deref(),
        Some(origin)
    );
    assert_eq!(
        relay_origin("https://Example.Test:443/viewer-ws").as_deref(),
        Some(origin)
    );
    assert_eq!(
        relay_origin("ws://example.test/host-ws").as_deref(),
        Some("http://example.test")
    );
    assert_eq!(relay_origin("wss://user@example.test/host-ws"), None);
    assert_eq!(relay_origin("ftp://example.test/host-ws"), None);
    assert_eq!(relay_origin("/host-ws"), None);
}

#[test]
fn contract_auth_bind_pc_signs_the_dtls_binding() {
    let fixture = fixture();
    let handshake = &fixture["handshake"];
    let message = &handshake["messages"]["authBindPc"];
    assert_eq!(field(message, "type"), "auth-bind");
    let dtls_pc = &handshake["dtls"]["pc"];
    assert_eq!(field(message, "signature"), field(dtls_pc, "signature"));
    // The PC's DTLS binding: Ed25519 over the signed bytes, under the PC public key.
    let public: [u8; 32] = b64(field(&handshake["pc"], "public"))
        .try_into()
        .expect("32-byte public key");
    let verifying = VerifyingKey::from_bytes(&public).expect("PC public key");
    let signature: [u8; 64] = b64(field(message, "signature"))
        .try_into()
        .expect("64-byte signature");
    let signed = unhex(field(dtls_pc, "signedBytesHex"));
    verifying
        .verify_strict(&signed, &Signature::from_bytes(&signature))
        .expect("PC auth-bind signature verifies");
}

#[test]
fn shared_ids_match_this_crate_and_are_unchanged() {
    let shared = shared_fixture();
    let local = fixture();
    let identities = shared["ids"].as_array().expect("shared ids");
    assert!(!identities.is_empty());
    for identity in identities {
        let owner = DeviceIdentity::from_seed(&seed_of(field(identity, "seedHex")));
        assert_eq!(owner.public_b64(), field(identity, "public"));
        assert_eq!(owner.id().to_string(), field(identity, "id"));
        assert_eq!(
            hex(&<sha2::Sha256 as sha2::Digest>::digest(owner.public_key())),
            field(identity, "sha256Hex")
        );
        if let Some(parse) = identity.get("parse") {
            let parsed = DeviceId::parse(field(parse, "input")).expect("alias input");
            assert_eq!(parsed.to_string(), field(parse, "canonical"));
        }
        // Device IDs do not change with the move: BubblePlay's fixture has the same seed and ID.
        let mine = local["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .find(|mine| mine["seedHex"] == identity["seedHex"])
            .expect("same seed in the local fixture");
        assert_eq!(field(mine, "id"), field(identity, "id"));
    }
}

#[test]
fn shared_relay_proof_vectors_verify_with_the_shared_crate() {
    use bubblebuffer_device_key::{relay_proof_bytes, DeviceKey, RelayProof, RelayRole};
    let shared = shared_fixture();
    let vectors = shared["relayProof"]["vectors"].as_array().expect("vectors");
    let mut valid = 0;
    for vector in vectors {
        let label = field(vector, "label");
        let role = RelayRole::parse(field(vector, "role")).expect("role");
        let origin = field(vector, "origin");
        let nonce: [u8; 32] = b64(field(vector, "nonce"))
            .try_into()
            .expect("32-byte nonce");
        let id = DeviceId::parse(field(vector, "id")).expect("id");
        let public: [u8; 32] = b64(field(vector, "public"))
            .try_into()
            .expect("32-byte public key");
        let signature: [u8; 64] = b64(field(vector, "signature"))
            .try_into()
            .expect("64-byte signature");
        let encoding = relay_proof_bytes(label, role, origin, &nonce, id, &public).expect("encode");
        assert_eq!(hex(&encoding), field(vector, "encodingHex"));
        let hash =
            bubblebuffer_device_key::relay_proof_hash(label, role, origin, &nonce, id, &public)
                .expect("hash");
        assert_eq!(hex(&hash), field(vector, "sha256Hex"));
        let verified = RelayProof::verify(label, role, origin, nonce, id, public, signature);
        match field(vector, "expected") {
            "valid" => {
                assert!(verified.is_ok(), "{}", field(vector, "name"));
                let seed = shared["ids"]
                    .as_array()
                    .expect("ids")
                    .iter()
                    .find(|identity| identity["id"] == vector["id"])
                    .map(|identity| seed_of(field(identity, "seedHex")))
                    .expect("seed for the vector's ID");
                assert_eq!(DeviceKey::from_seed(&seed).sign(&hash), signature);
                valid += 1;
            }
            "invalid" => assert!(verified.is_err(), "{}", field(vector, "name")),
            other => panic!("unknown expectation {other}"),
        }
    }
    assert!(valid >= 2);
}

#[test]
fn bubbleplay_relay_proofs_use_the_bubbleplay_label() {
    // The live relay verifies proofs over `bubbleplay-relay-v1`: the local vectors (checked by
    // contract_relay_proof_vectors) start with exactly that label.
    assert_eq!(bp_device_key::RELAY_LABEL, "bubbleplay-relay-v1");
    let fixture = fixture();
    for vector in fixture["relayProof"]["vectors"]
        .as_array()
        .expect("vectors")
    {
        let mut prefix = (bp_device_key::RELAY_LABEL.len() as u32)
            .to_be_bytes()
            .to_vec();
        prefix.extend_from_slice(bp_device_key::RELAY_LABEL.as_bytes());
        assert!(field(vector, "encodingHex").starts_with(&hex(&prefix)));
    }
}

#[cfg(feature = "keyfile")]
mod key_files {
    use super::*;
    use bp_device_key::KeyFile;

    #[test]
    fn contract_keyfile_plain_round_trips() {
        let fixture = shared_fixture();
        let plain = &fixture["keyFiles"]["plain"];
        let file = &plain["file"];
        assert_eq!(field(file, "format"), "bubblebuffer-device-key");
        assert!(file["kdf"].is_null() && file["aead"].is_null());
        let key_file = KeyFile::parse(&file.to_string()).expect("plain file parses");
        assert!(!key_file.is_protected());
        assert_eq!(key_file.id().to_string(), field(file, "id"));
        assert_eq!(key_file.label(), field(file, "label"));
        assert_eq!(key_file.created(), field(file, "created"));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(key_file.public_key()),
            field(file, "public")
        );
        let seed = key_file.unlock(None).expect("plain unlock").to_bytes();
        assert_eq!(seed, seed_of(field(plain, "seedHex")));
        let json = key_file.to_json().expect("serialise");
        assert_eq!(serde_json::from_str::<Value>(&json).expect("json"), *file);
        assert!(!json.ends_with('\n'));
        assert!(
            json.starts_with("{\n  \"format\": \"bubblebuffer-device-key\",\n  \"version\": 1,\n")
        );
    }

    #[test]
    fn contract_keyfile_passphrase_unlocks_and_aad_matches() {
        let fixture = shared_fixture();
        let vector = &fixture["keyFiles"]["passphrase"];
        let key_file = KeyFile::parse(&vector["file"].to_string()).expect("protected file parses");
        assert!(key_file.is_protected());
        let passphrase = field(vector, "passphrase");
        let signing = key_file.unlock(Some(passphrase)).expect("unlock");
        assert_eq!(signing.to_bytes(), seed_of(field(vector, "seedHex")));
        let aad = key_file
            .aad_for_contract()
            .expect("protected file has an AAD");
        assert_eq!(hex(&aad), field(vector, "aadHex"));
        assert!(matches!(
            key_file.unlock(None),
            Err(DeviceKeyError::PassphraseRequired)
        ));
        assert!(matches!(
            key_file.unlock(Some("incorrect horse battery staple")),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
    }

    #[test]
    fn contract_keyfile_failures_are_authentication_failures() {
        let fixture = shared_fixture();
        let failures = fixture["keyFiles"]["failures"]
            .as_array()
            .expect("failures");
        assert!(!failures.is_empty());
        for failure in failures {
            assert_eq!(field(failure, "expected"), "authentication-failed");
            let key_file =
                KeyFile::parse(&failure["file"].to_string()).expect("structurally valid");
            assert!(matches!(
                key_file.unlock(Some(field(failure, "passphrase"))),
                Err(DeviceKeyError::AuthenticationFailed)
            ));
        }
    }

    #[test]
    fn a_file_in_another_format_is_unsupported() {
        let fixture = shared_fixture();
        let mut file = fixture["keyFiles"]["plain"]["file"].clone();
        file["format"] = Value::from("other-format");
        assert!(matches!(
            KeyFile::parse(&file.to_string()),
            Err(DeviceKeyError::UnsupportedFormat)
        ));
    }
}
