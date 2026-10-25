//! Checks the phase 2 API against the shared contract fixture: `handshake` (transcript,
//! signatures, control keys and messages, DTLS binding) and `phase2` (pairing, admit hash, MACs,
//! SAS and the ctl envelope). The JS checks in `tools/tests/device-keys-contract.test.mjs`
//! recompute the same vectors independently.

#![cfg(feature = "phase2")]

use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::phase2::{
    auth_bind_signed_bytes, auth_finish_signed_bytes, auth_host_signed_bytes, ctl_nonce,
    sign_auth_bind, sign_auth_finish, sign_auth_host, verify_auth_bind, verify_auth_finish,
    verify_auth_host, CtlKeys, CtlOpener, EphemeralSecret, PairingCode, Sas, Side, Transcript,
};
use bp_device_key::{DeviceId, DeviceIdentity, DeviceKeyError};
use serde_json::Value;

fn fixture() -> Value {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/device-keys-contract.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("fixture parses")
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

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("field {key} is not a string"))
}

fn arr32(bytes: Vec<u8>) -> [u8; 32] {
    bytes.try_into().expect("32 bytes")
}

fn arr64(bytes: Vec<u8>) -> [u8; 64] {
    bytes.try_into().expect("64 bytes")
}

struct Party {
    identity: DeviceIdentity,
    id: DeviceId,
    public: [u8; 32],
    nonce: [u8; 32],
    ephemeral_private: [u8; 32],
    ephemeral: [u8; 32],
}

fn party(h: &Value, name: &str) -> Party {
    let p = &h[name];
    let identity = DeviceIdentity::from_seed(&arr32(unhex(field(p, "seedHex"))));
    let id = DeviceId::parse(field(p, "id")).expect("id");
    let public = arr32(b64(field(p, "public")));
    assert_eq!(identity.public_key(), public);
    assert_eq!(identity.id(), id);
    assert_eq!(DeviceId::from_public_key(&public), id);
    let ephemeral_private = arr32(unhex(field(p, "ephemeralPrivateHex")));
    let ephemeral = arr32(b64(field(p, "ephemeral")));
    assert_eq!(
        EphemeralSecret::from_bytes(ephemeral_private).public_key(),
        ephemeral,
        "{name} ephemeral public key"
    );
    Party {
        identity,
        id,
        public,
        nonce: arr32(b64(field(p, "nonce"))),
        ephemeral_private,
        ephemeral,
    }
}

struct Handshake {
    fixture: Value,
    viewer: Party,
    pc: Party,
    h: [u8; 32],
}

fn handshake() -> Handshake {
    let fixture = fixture();
    let hs = &fixture["handshake"];
    let viewer = party(hs, "viewer");
    let pc = party(hs, "pc");
    let transcript = Transcript {
        relay_origin: field(hs, "relayOrigin"),
        epoch: field(hs, "sessionEpoch"),
        viewer_id: viewer.id,
        viewer_public: viewer.public,
        pc_id: pc.id,
        pc_public: pc.public,
        viewer_nonce: viewer.nonce,
        pc_nonce: pc.nonce,
        viewer_ephemeral: viewer.ephemeral,
        pc_ephemeral: pc.ephemeral,
    };
    assert_eq!(hex(&transcript.encode()), field(hs, "transcriptHex"));
    let h = transcript.hash();
    assert_eq!(hex(&h), field(hs, "transcriptHashHex"));
    Handshake {
        fixture,
        viewer,
        pc,
        h,
    }
}

fn ctl_keys(hs: &Handshake) -> CtlKeys {
    let shared = EphemeralSecret::from_bytes(hs.pc.ephemeral_private)
        .diffie_hellman(&hs.viewer.ephemeral)
        .expect("shared");
    CtlKeys::derive(&shared, &hs.h)
}

#[test]
fn transcript_and_hash() {
    let hs = handshake();
    // Bytes of the epoch are preserved: 2^53 + 1 would not survive a float parse.
    assert_eq!(
        field(&hs.fixture["handshake"], "sessionEpoch"),
        "9007199254740993"
    );
    // Any field change moves H.
    let hs_json = &hs.fixture["handshake"];
    let changed = Transcript {
        relay_origin: field(hs_json, "relayOrigin"),
        epoch: "9007199254740992",
        viewer_id: hs.viewer.id,
        viewer_public: hs.viewer.public,
        pc_id: hs.pc.id,
        pc_public: hs.pc.public,
        viewer_nonce: hs.viewer.nonce,
        pc_nonce: hs.pc.nonce,
        viewer_ephemeral: hs.viewer.ephemeral,
        pc_ephemeral: hs.pc.ephemeral,
    };
    assert_ne!(changed.hash(), hs.h);
}

#[test]
fn handshake_signatures() {
    let hs = handshake();
    let v = &hs.fixture["handshake"];

    assert_eq!(
        hex(&auth_host_signed_bytes(&hs.h)),
        field(v, "pcSignedBytesHex")
    );
    let pc_signature = sign_auth_host(&hs.pc.identity, &hs.h);
    assert_eq!(pc_signature.to_vec(), b64(field(v, "pcSignature")));
    verify_auth_host(&hs.pc.public, &hs.h, &pc_signature).expect("auth-host verifies");

    assert_eq!(
        hex(&auth_finish_signed_bytes(&hs.h, &pc_signature)),
        field(v, "viewerSignedBytesHex")
    );
    let viewer_signature = sign_auth_finish(&hs.viewer.identity, &hs.h, &pc_signature);
    assert_eq!(viewer_signature.to_vec(), b64(field(v, "viewerSignature")));
    verify_auth_finish(&hs.viewer.public, &hs.h, &pc_signature, &viewer_signature)
        .expect("auth-finish verifies");

    // The message vectors carry the same signatures.
    let messages = &v["messages"];
    assert_eq!(
        b64(field(&messages["authHost"], "signature")),
        pc_signature.to_vec()
    );
    assert_eq!(
        b64(field(&messages["authFinish"], "signature")),
        viewer_signature.to_vec()
    );

    // Tampered bytes fail: the PC signature inside auth-finish, H, the signatures themselves,
    // and the wrong key.
    let mut tampered_pc = pc_signature;
    tampered_pc[63] ^= 1;
    assert!(matches!(
        verify_auth_finish(&hs.viewer.public, &hs.h, &tampered_pc, &viewer_signature),
        Err(DeviceKeyError::InvalidHandshakeSignature)
    ));
    assert!(matches!(
        verify_auth_host(&hs.pc.public, &hs.h, &tampered_pc),
        Err(DeviceKeyError::InvalidHandshakeSignature)
    ));
    let mut tampered_h = hs.h;
    tampered_h[0] ^= 1;
    assert!(verify_auth_host(&hs.pc.public, &tampered_h, &pc_signature).is_err());
    assert!(verify_auth_finish(
        &hs.viewer.public,
        &tampered_h,
        &pc_signature,
        &viewer_signature
    )
    .is_err());
    let mut tampered_viewer = viewer_signature;
    tampered_viewer[0] ^= 1;
    assert!(verify_auth_finish(&hs.viewer.public, &hs.h, &pc_signature, &tampered_viewer).is_err());
    assert!(verify_auth_host(&hs.viewer.public, &hs.h, &pc_signature).is_err());
    // Wrong lengths are errors, never panics.
    assert!(matches!(
        verify_auth_host(&hs.pc.public[..31], &hs.h, &pc_signature),
        Err(DeviceKeyError::InvalidLength("public"))
    ));
    assert!(matches!(
        verify_auth_host(&hs.pc.public, &hs.h, &pc_signature[..63]),
        Err(DeviceKeyError::InvalidLength("signature"))
    ));
}

#[test]
fn shared_secret_and_control_keys() {
    let hs = handshake();
    let v = &hs.fixture["handshake"];
    let viewer_shared = EphemeralSecret::from_bytes(hs.viewer.ephemeral_private)
        .diffie_hellman(&hs.pc.ephemeral)
        .expect("viewer shared");
    let pc_shared = EphemeralSecret::from_bytes(hs.pc.ephemeral_private)
        .diffie_hellman(&hs.viewer.ephemeral)
        .expect("pc shared");
    assert_eq!(viewer_shared.as_bytes(), pc_shared.as_bytes());
    assert_eq!(hex(viewer_shared.as_bytes()), field(v, "sharedSecretHex"));
    let keys = CtlKeys::derive(&viewer_shared, &hs.h);
    assert_eq!(
        hex(keys.viewer_to_pc()),
        field(&v["controlKeys"], "viewerToPcHex")
    );
    assert_eq!(
        hex(keys.pc_to_viewer()),
        field(&v["controlKeys"], "pcToViewerHex")
    );
    assert!(matches!(
        EphemeralSecret::from_bytes(hs.viewer.ephemeral_private).diffie_hellman(&[0u8; 32]),
        Err(DeviceKeyError::WeakSharedSecret)
    ));
}

fn sealer_side(direction: &str) -> Side {
    match direction {
        "viewer-to-pc" => Side::Viewer,
        "pc-to-viewer" => Side::Pc,
        other => panic!("direction {other}"),
    }
}

fn opener_side(direction: &str) -> Side {
    match sealer_side(direction) {
        Side::Viewer => Side::Pc,
        Side::Pc => Side::Viewer,
    }
}

#[test]
fn phase0_control_messages() {
    let hs = handshake();
    let keys = ctl_keys(&hs);
    for direction in ["viewer-to-pc", "pc-to-viewer"] {
        let (mut sealer, _) = keys.channel(sealer_side(direction));
        let (_, mut opener) = keys.channel(opener_side(direction));
        for vector in hs.fixture["handshake"]["controlMessages"]
            .as_array()
            .expect("controlMessages")
            .iter()
            .filter(|vector| vector["direction"] == direction)
        {
            let seq = vector["sequence"].as_u64().expect("sequence");
            let kind = field(vector, "type");
            let plain = field(vector, "plaintextUtf8").as_bytes();
            assert_eq!(hex(&ctl_nonce(seq)), field(vector, "nonceHex"));
            let (sealed_seq, data) = sealer.seal(kind, plain).expect("seal");
            assert_eq!(sealed_seq, seq);
            assert_eq!(data, b64(field(vector, "ciphertext")));
            assert_eq!(opener.open(kind, seq, &data).expect("open"), plain);
        }
    }
}

#[test]
fn phase2_ctl_messages() {
    let hs = handshake();
    let keys = ctl_keys(&hs);
    let messages = hs.fixture["phase2"]["ctl"]["messages"]
        .as_array()
        .expect("messages");
    assert_eq!(messages.len(), 4);
    assert!(messages.iter().any(|vector| vector["encoding"] == "binary"));
    for direction in ["viewer-to-pc", "pc-to-viewer"] {
        let (mut sealer, _) = keys.channel(sealer_side(direction));
        let (_, mut opener) = keys.channel(opener_side(direction));
        let mut count = 0;
        for vector in messages
            .iter()
            .filter(|vector| vector["direction"] == direction)
        {
            let seq = vector["seq"].as_u64().expect("seq");
            let kind = field(vector, "kind");
            let plain = unhex(field(vector, "plaintextHex"));
            if vector["encoding"] == "json" {
                assert_eq!(plain, field(vector, "plaintextUtf8").as_bytes());
            }
            assert_eq!(field(vector, "aadUtf8"), kind);
            assert_eq!(hex(&ctl_nonce(seq)), field(vector, "nonceHex"));
            let message = &vector["message"];
            assert_eq!(field(message, "type"), "ctl");
            assert_eq!(field(message, "kind"), kind);
            assert_eq!(message["seq"].as_u64(), Some(seq));
            assert_eq!(sealer.next_seq(), seq);
            let (sealed_seq, data) = sealer.seal(kind, &plain).expect("seal");
            assert_eq!(sealed_seq, seq);
            assert_eq!(data, b64(field(message, "data")), "{direction} seq {seq}");
            assert_eq!(opener.expected_seq(), seq);
            assert_eq!(opener.open(kind, seq, &data).expect("open"), plain);
            count += 1;
        }
        assert_eq!(count, 2, "{direction}");
    }
}

fn opener_at(keys: &CtlKeys, fixture: &Value, direction: &str, expected: u64) -> CtlOpener {
    let (_, mut opener) = keys.channel(opener_side(direction));
    for vector in fixture["phase2"]["ctl"]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|vector| vector["direction"] == direction)
        .filter(|vector| vector["seq"].as_u64().expect("seq") < expected)
    {
        let message = &vector["message"];
        opener
            .open(
                field(message, "kind"),
                message["seq"].as_u64().expect("seq"),
                &b64(field(message, "data")),
            )
            .expect("valid earlier message");
    }
    assert_eq!(opener.expected_seq(), expected);
    opener
}

#[test]
fn phase2_ctl_failures_end_the_channel() {
    let hs = handshake();
    let keys = ctl_keys(&hs);
    let failures = hs.fixture["phase2"]["ctl"]["failures"]
        .as_array()
        .expect("failures");
    let names: Vec<&str> = failures
        .iter()
        .map(|vector| field(vector, "name"))
        .collect();
    assert_eq!(
        names,
        [
            "replayed-seq",
            "skipped-seq",
            "wrong-kind-aad",
            "wrong-direction-key"
        ]
    );
    for vector in failures {
        let name = field(vector, "name");
        assert_eq!(field(vector, "expected"), "end-session");
        let direction = field(vector, "direction");
        let expected = vector["expectedSeq"].as_u64().expect("expectedSeq");
        let mut opener = opener_at(&keys, &hs.fixture, direction, expected);
        let message = &vector["message"];
        let result = opener.open(
            field(message, "kind"),
            message["seq"].as_u64().expect("seq"),
            &b64(field(message, "data")),
        );
        match name {
            "replayed-seq" | "skipped-seq" => {
                assert!(matches!(result, Err(DeviceKeyError::CtlSequence)), "{name}")
            }
            _ => assert!(
                matches!(result, Err(DeviceKeyError::CtlAuthenticationFailed)),
                "{name}"
            ),
        }
        assert!(opener.is_dead(), "{name}");
        // The valid next message for this opener is refused afterwards.
        if let Some(next) = hs.fixture["phase2"]["ctl"]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|v| v["direction"] == direction && v["seq"].as_u64() == Some(expected))
        {
            let next = &next["message"];
            assert!(
                matches!(
                    opener.open(field(next, "kind"), expected, &b64(field(next, "data"))),
                    Err(DeviceKeyError::CtlChannelClosed)
                ),
                "{name}"
            );
        }
    }
}

#[test]
fn pairing_code_admit_key_and_macs() {
    let hs = handshake();
    let p = &hs.fixture["phase2"]["pairing"];
    let code = PairingCode::parse(field(p, "code")).expect("code");
    assert_eq!(code.as_str(), field(p, "code"));
    assert_eq!(code.display().as_str(), field(p, "codeDisplay"));
    for case in p["normalise"].as_array().expect("normalise") {
        let parsed = PairingCode::parse(field(case, "input")).expect("normalises");
        assert_eq!(parsed.as_str(), field(case, "canonical"));
    }
    assert_eq!(
        PairingCode::parse(field(p, "codeDisplay"))
            .expect("display form")
            .as_str(),
        field(p, "code")
    );

    let pc_id = DeviceId::parse(field(p, "pcId")).expect("pc id");
    assert_eq!(pc_id, hs.pc.id);
    let admit_bytes = unhex(field(p, "admitBytesHex"));
    assert_eq!(
        admit_bytes,
        [
            b"bubbleplay-pair-admit-v1".as_slice(),
            field(p, "pcId").as_bytes(),
            code.as_str().as_bytes()
        ]
        .concat()
    );
    assert_eq!(code.admit_hash(&pc_id).to_vec(), b64(field(p, "admit")));
    // A non-canonical spelling of the same code gives the same admit hash.
    let messy = PairingCode::parse(field(&p["normalise"][0], "input")).expect("messy");
    assert_eq!(messy.admit_hash(&pc_id), code.admit_hash(&pc_id));

    let key = code.pair_key();
    assert_eq!(hex(key.as_bytes()), field(p, "pairKeyHex"));
    let viewer_mac = key.mac(Side::Viewer, &hs.h);
    let pc_mac = key.mac(Side::Pc, &hs.h);
    assert_eq!(viewer_mac.to_vec(), b64(field(p, "viewerMac")));
    assert_eq!(pc_mac.to_vec(), b64(field(p, "pcMac")));
    assert_ne!(viewer_mac, pc_mac);
    key.verify_mac(Side::Viewer, &hs.h, &viewer_mac)
        .expect("viewer mac");
    key.verify_mac(Side::Pc, &hs.h, &pc_mac).expect("pc mac");
    assert!(matches!(
        key.verify_mac(Side::Pc, &hs.h, &viewer_mac),
        Err(DeviceKeyError::PairingMacMismatch)
    ));
    assert!(key
        .verify_mac(Side::Viewer, &hs.h, &viewer_mac[..31])
        .is_err());

    let wrong = PairingCode::parse(field(p, "wrongCode")).expect("wrong code");
    assert_ne!(wrong.as_str(), code.as_str());
    let wrong_mac = b64(field(p, "wrongCodeViewerMac"));
    assert_eq!(
        wrong.pair_key().mac(Side::Viewer, &hs.h).to_vec(),
        wrong_mac
    );
    assert!(matches!(
        key.verify_mac(Side::Viewer, &hs.h, &wrong_mac),
        Err(DeviceKeyError::PairingMacMismatch)
    ));
    assert!(wrong
        .pair_key()
        .verify_mac(Side::Viewer, &hs.h, &viewer_mac)
        .is_err());

    let messages = &p["messages"];
    assert_eq!(
        b64(field(&messages["pairProofViewer"], "mac")),
        viewer_mac.to_vec()
    );
    assert_eq!(b64(field(&messages["pairProofPc"], "mac")), pc_mac.to_vec());
    assert_eq!(field(&messages["pairOpen"], "admit"), field(p, "admit"));
}

#[test]
fn comparison_number() {
    let hs = handshake();
    let p = &hs.fixture["phase2"]["pairing"];
    assert_eq!(
        unhex(field(p, "sasBytesHex")),
        [b"bubbleplay-pair-sas-v1".as_slice(), &hs.h[..]].concat()
    );
    let sas = Sas::from_transcript_hash(&hs.h);
    assert_eq!(Some(u64::from(sas.raw())), p["sasU32"].as_u64());
    assert_eq!(sas.number(), field(p, "sasNumber"));
    assert_eq!(sas.display(), field(p, "sasDisplay"));
}

#[test]
fn dtls_bind_signatures() {
    let hs = handshake();
    let dtls = &hs.fixture["handshake"]["dtls"];
    let messages = &hs.fixture["handshake"]["messages"];
    for (side, party, message) in [
        ("viewer", &hs.viewer, "authBind"),
        ("pc", &hs.pc, "authBindPc"),
    ] {
        let v = &dtls[side];
        let local = field(v, "localFingerprint");
        let remote = field(v, "remoteFingerprint");
        assert_eq!(
            hex(&auth_bind_signed_bytes(&hs.h, local, remote).expect("fingerprints")),
            field(v, "signedBytesHex"),
            "{side}"
        );
        let signature = sign_auth_bind(&party.identity, &hs.h, local, remote).expect("sign");
        assert_eq!(signature.to_vec(), b64(field(v, "signature")), "{side}");
        assert_eq!(
            signature.to_vec(),
            b64(field(&messages[message], "signature"))
        );
        verify_auth_bind(&party.public, &hs.h, local, remote, &signature).expect("verifies");
        // Reversed order fails.
        assert!(matches!(
            verify_auth_bind(&party.public, &hs.h, remote, local, &signature),
            Err(DeviceKeyError::InvalidHandshakeSignature)
        ));
        // Lowercase hex is not the canonical form and is refused before verifying.
        assert!(matches!(
            verify_auth_bind(
                &party.public,
                &hs.h,
                &local.to_lowercase(),
                remote,
                &signature
            ),
            Err(DeviceKeyError::InvalidFingerprint)
        ));
        let mut tampered = arr64(signature.to_vec());
        tampered[10] ^= 1;
        assert!(verify_auth_bind(&party.public, &hs.h, local, remote, &tampered).is_err());
    }
    assert!(matches!(
        sign_auth_bind(&hs.viewer.identity, &hs.h, "sha-256 AB", "sha-256 CD"),
        Err(DeviceKeyError::InvalidFingerprint)
    ));
}
