use super::pairing::CROCKFORD;
use super::*;

fn sealer_and_opener() -> (CtlSealer, CtlOpener) {
    let key = [7u8; 32];
    (CtlSealer::new(&key), CtlOpener::new(&key))
}

#[test]
fn channel_is_dead_after_any_failure() {
    // Tag failure.
    let (mut sealer, mut opener) = sealer_and_opener();
    let (seq, mut data) = sealer.seal("input", b"abc").expect("seal");
    data[0] ^= 1;
    assert!(matches!(
        opener.open("input", seq, &data),
        Err(DeviceKeyError::CtlAuthenticationFailed)
    ));
    assert!(opener.is_dead());
    data[0] ^= 1;
    assert!(matches!(
        opener.open("input", seq, &data),
        Err(DeviceKeyError::CtlChannelClosed)
    ));

    // Sequence gap, then the correct message is still refused.
    let (mut sealer, mut opener) = sealer_and_opener();
    let (_, first) = sealer.seal("input", b"a").expect("seal");
    let (_, second) = sealer.seal("input", b"b").expect("seal");
    assert!(matches!(
        opener.open("input", 1, &second),
        Err(DeviceKeyError::CtlSequence)
    ));
    assert!(matches!(
        opener.open("input", 0, &first),
        Err(DeviceKeyError::CtlChannelClosed)
    ));

    // Invalid kind.
    let (mut sealer, mut opener) = sealer_and_opener();
    let (_, data) = sealer.seal("input", b"a").expect("seal");
    let long = "a".repeat(33);
    for bad in ["", "Input", "in put", long.as_str(), "ïnput"] {
        assert!(matches!(
            sealer.seal(bad, b"a"),
            Err(DeviceKeyError::InvalidCtlKind)
        ));
    }
    assert!(matches!(
        opener.open("Input", 0, &data),
        Err(DeviceKeyError::InvalidCtlKind)
    ));
    assert!(matches!(
        opener.open("input", 0, &data),
        Err(DeviceKeyError::CtlChannelClosed)
    ));
}

#[test]
fn sequence_overflow_is_refused() {
    let (mut sealer, mut opener) = sealer_and_opener();
    sealer.next_seq = MAX_CTL_SEQ;
    opener.expected_seq = MAX_CTL_SEQ;
    let (seq, data) = sealer
        .seal("input", b"last")
        .expect("seq 2^53-1 is allowed");
    assert_eq!(seq, MAX_CTL_SEQ);
    assert_eq!(opener.open("input", seq, &data).expect("open"), b"last");
    assert!(matches!(
        sealer.seal("input", b"over"),
        Err(DeviceKeyError::CtlSequenceExhausted)
    ));
    assert!(matches!(
        sealer.seal("input", b"over"),
        Err(DeviceKeyError::CtlChannelClosed)
    ));
    assert!(matches!(
        opener.open("input", MAX_CTL_SEQ + 1, &data),
        Err(DeviceKeyError::CtlSequenceExhausted)
    ));
    assert!(opener.is_dead());

    // A sequence number beyond the limit on a fresh opener is a gap and ends it too.
    let (_, mut fresh) = sealer_and_opener();
    assert!(fresh.open("input", u64::MAX, &data).is_err());
    assert!(fresh.is_dead());
}

#[test]
fn generated_codes_are_canonical() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut seen_all = [false; 32];
    for round in 0..2000u32 {
        let mut bytes = [0u8; PAIRING_CODE_LEN];
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }
        if round == 0 {
            bytes = [0xff; PAIRING_CODE_LEN];
        }
        let code = PairingCode::from_random_bytes(&bytes);
        let text = code.as_str().to_owned();
        assert_eq!(text.len(), PAIRING_CODE_LEN);
        for byte in text.bytes() {
            let index = CROCKFORD.iter().position(|&c| c == byte).expect("alphabet");
            seen_all[index] = true;
        }
        let reparsed = PairingCode::parse(&text).expect("canonical parses");
        assert_eq!(reparsed.as_str(), text);
        let display = code.display();
        assert_eq!(display.len(), 11);
        assert_eq!(&display[5..6], "-");
        assert_eq!(
            PairingCode::parse(&display)
                .expect("display parses")
                .as_str(),
            text
        );
    }
    assert!(seen_all.iter().all(|&seen| seen), "every symbol reachable");
    // Each low-5-bit value maps to its own symbol, so the mapping is uniform.
    let all: [u8; PAIRING_CODE_LEN] = [0, 1, 2, 3, 4, 27, 28, 29, 30, 31];
    assert_eq!(PairingCode::from_random_bytes(&all).as_str(), "01234VWXYZ");
}

#[cfg(feature = "keygen")]
#[test]
fn os_generated_codes_are_canonical() {
    for _ in 0..64 {
        let code = PairingCode::generate().expect("entropy");
        assert_eq!(
            PairingCode::parse(code.as_str()).expect("parses").as_str(),
            code.as_str()
        );
    }
}

#[test]
fn pairing_code_rejects_bad_input() {
    for input in [
        "",
        "70Q1K9MXR",
        "70Q1K9MXR12",
        "70Q1K9MXRU",
        "70Q1K9MXR!",
        "70Q1K9MXRé",
        "70Q1K_9MXR1",
    ] {
        assert!(
            matches!(
                PairingCode::parse(input),
                Err(DeviceKeyError::InvalidPairingCode)
            ),
            "accepted {input:?}"
        );
    }
}

#[test]
fn fingerprints_must_be_canonical_sha256() {
    let good = format!("sha-256 {}", ["AB"; 32].join(":"));
    assert!(check_fingerprint(&good).is_ok());
    for bad in [
        format!("sha-256 {}", ["ab"; 32].join(":")),
        format!("SHA-256 {}", ["AB"; 32].join(":")),
        format!("sha-1 {}", ["AB"; 20].join(":")),
        format!("sha-256 {}", ["AB"; 31].join(":")),
        format!("sha-256 {}:", ["AB"; 32].join(":")),
        format!("sha-256  {}", ["AB"; 32].join(":")),
        format!("sha-256 {}", ["AB"; 32].join("-")),
        format!("sha-256 {}", ["AG"; 32].join(":")),
    ] {
        assert!(
            matches!(
                check_fingerprint(&bad),
                Err(DeviceKeyError::InvalidFingerprint)
            ),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn fingerprints_normalise_to_the_signed_form() {
    let canonical = format!("sha-256 {}", ["AB"; 32].join(":"));
    for input in [
        canonical.clone(),
        format!("sha-256 {}", ["ab"; 32].join(":")),
        format!("SHA-256 {}", ["aB"; 32].join(":")),
        format!("  Sha-256 \t  {}  ", ["Ab"; 32].join(":")),
    ] {
        assert_eq!(
            normalize_fingerprint(&input).expect("normalises"),
            canonical,
            "{input:?}"
        );
    }
    for bad in [
        String::new(),
        "sha-256".to_owned(),
        format!("sha-1 {}", ["ab"; 20].join(":")),
        format!("sha-256 {}", ["ab"; 31].join(":")),
        format!("sha-256 {}", ["ab"; 32].join("")),
        format!("sha-256 {}", ["ag"; 32].join(":")),
    ] {
        assert!(
            matches!(
                normalize_fingerprint(&bad),
                Err(DeviceKeyError::InvalidFingerprint)
            ),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn sdp_fingerprint_picks_sha256() {
    let sha1 = format!("sha-1 {}", ["0f"; 20].join(":"));
    let sha256 = format!("sha-256 {}", ["cd"; 32].join(":"));
    let canonical = format!("sha-256 {}", ["CD"; 32].join(":"));
    // sha-1 first, then sha-256 at session and media level (CRLF and LF lines).
    let sdp = format!(
        "v=0\r\na=fingerprint:{sha1}\r\na=fingerprint:{sha256}\r\nm=application 9 UDP/DTLS/SCTP\na=fingerprint:{canonical}\n"
    );
    assert_eq!(sdp_sha256_fingerprint(&sdp).expect("sha-256"), canonical);
    // sha-256 first, sha-1 after.
    let sdp = format!("a=fingerprint:{sha256}\na=fingerprint:{sha1}\n");
    assert_eq!(sdp_sha256_fingerprint(&sdp).expect("sha-256"), canonical);
}

#[test]
fn sdp_without_a_usable_sha256_fingerprint_is_an_error() {
    let sha1 = format!("sha-1 {}", ["0F"; 20].join(":"));
    let one = format!("sha-256 {}", ["AB"; 32].join(":"));
    let other = format!("sha-256 {}", ["CD"; 32].join(":"));
    for sdp in [
        String::new(),
        "v=0\r\na=setup:actpass\r\n".to_owned(),
        format!("v=0\r\na=fingerprint:{sha1}\r\n"),
        format!("a=fingerprint:sha-256 {}\n", ["AB"; 31].join(":")),
        format!("a=fingerprint:{one}\na=fingerprint:{other}\n"),
    ] {
        assert!(
            matches!(
                sdp_sha256_fingerprint(&sdp),
                Err(DeviceKeyError::InvalidFingerprint)
            ),
            "accepted {sdp:?}"
        );
    }
}

#[test]
fn low_order_peer_key_is_refused() {
    let secret = EphemeralSecret::from_bytes([9u8; 32]);
    assert!(matches!(
        secret.diffie_hellman(&[0u8; 32]),
        Err(DeviceKeyError::WeakSharedSecret)
    ));
}

#[test]
fn debug_output_hides_secrets() {
    let code = PairingCode::parse("70Q1K9MXR1").expect("code");
    let key = code.pair_key();
    let text = format!("{code:?} {key:?}");
    assert!(!text.contains("70Q1K"));
    assert_eq!(text, "PairingCode(..) PairKey(..)");
}
