//! BubblePlay's relay proof on top of the shared crate, the relay-origin canonical form and an
//! unlocked device identity.
//!
//! A device proves possession of its key by signing the SHA-256 hash of a length-prefixed
//! encoding (see `docs/architecture/device-keys.md`, "Encodings for implementers"): the context
//! label, the role, the relay origin, the relay's raw nonce, the canonical hyphenated device ID
//! and the raw public key, each written as a uint32 big-endian length followed by its bytes. The
//! shared crate builds and checks that encoding; BubblePlay always passes [`RELAY_LABEL`], so the
//! bytes the live relay verifies are unchanged.

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bubblebuffer_device_key::{DeviceKey, RelayProof, RelayRole};
use ed25519_dalek::SigningKey;
use url::Url;

use crate::{DeviceId, DeviceKeyError};

/// BubblePlay's relay proof label, the first field of every proof encoding.
pub const RELAY_LABEL: &str = "bubbleplay-relay-v1";

/// The party a proof is made for. The wire strings are fixed: `host` and `viewer`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Host,
    Viewer,
}

impl Role {
    /// The wire string that goes into the proof encoding.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Viewer => "viewer",
        }
    }

    /// Parses a wire string. Any other spelling is refused; there is no case folding.
    pub fn parse(value: &str) -> Result<Self, DeviceKeyError> {
        match value {
            "host" => Ok(Self::Host),
            "viewer" => Ok(Self::Viewer),
            _ => Err(DeviceKeyError::UnknownRole),
        }
    }

    fn shared(self) -> RelayRole {
        match self {
            Self::Host => RelayRole::Host,
            Self::Viewer => RelayRole::Viewer,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An unlocked device key (the shared crate's `DeviceKey`, zeroized on drop) with its ID.
/// `Debug` shows the ID only.
pub struct DeviceIdentity {
    id: DeviceId,
    key: DeviceKey,
}

impl DeviceIdentity {
    /// Wraps an unlocked signing key. The ID is derived from the public key.
    pub fn from_signing_key(signing: SigningKey) -> Self {
        Self::from_key(DeviceKey::from(signing))
    }

    /// Builds an identity from the raw 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self::from_key(DeviceKey::from_seed(seed))
    }

    /// Wraps a shared-crate key.
    pub fn from_key(key: DeviceKey) -> Self {
        Self { id: key.id(), key }
    }

    /// Generates a new key from OS randomness, in memory only. Storing it is the caller's job;
    /// with the `keyfile` feature, `KeyFile::create` also writes the key file format.
    #[cfg(feature = "keygen")]
    pub fn generate() -> Result<Self, DeviceKeyError> {
        Ok(Self::from_key(DeviceKey::generate()?))
    }

    /// Unlocks a plain (unprotected) key file in the current format. A passphrase-protected
    /// file is refused: nothing can supply the passphrase here.
    #[cfg(feature = "keyfile")]
    pub fn from_plain_key_json(text: &str) -> Result<Self, DeviceKeyError> {
        let file = crate::KeyFile::parse(text)?;
        if file.is_protected() {
            return Err(DeviceKeyError::PassphraseRequired);
        }
        Ok(Self::from_signing_key(file.unlock(None)?))
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.public_key()
    }

    /// The shared-crate key, for wrapping it in a key file.
    pub fn key(&self) -> &DeviceKey {
        &self.key
    }

    /// Ed25519 over `message` itself (phase 2 signs its context-prefixed bytes directly).
    #[cfg(feature = "phase2")]
    pub(crate) fn sign_bytes(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message)
    }

    /// The raw public key as unpadded base64url, the form the relay expects in `hello`.
    pub fn public_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key())
    }

    /// Signs the relay proof for `role` at `origin` and the relay's `nonce`. The signed ID is
    /// always the canonical hyphenated form.
    pub fn sign_relay_proof(&self, role: Role, origin: &str, nonce: &[u8; 32]) -> [u8; 64] {
        // The only failure is a field longer than u32::MAX bytes; an origin never is.
        let hash = relay_proof_hash(role, origin, nonce, self.id, &self.public_key())
            .expect("relay origin shorter than 4 GiB");
        self.key.sign(&hash)
    }
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The relay-proof encoding with BubblePlay's label: each field as uint32 big-endian length then
/// bytes, in this order: the label, the role, the relay origin, the raw nonce, the canonical ID
/// and the raw public key.
pub fn relay_proof_encoding(
    role: Role,
    origin: &str,
    nonce: &[u8; 32],
    id: DeviceId,
    public: &[u8; 32],
) -> Result<Vec<u8>, DeviceKeyError> {
    Ok(bubblebuffer_device_key::relay_proof_bytes(
        RELAY_LABEL,
        role.shared(),
        origin,
        nonce,
        id,
        public,
    )?)
}

/// SHA-256 of the relay-proof encoding. Ed25519 signs these 32 bytes directly.
pub fn relay_proof_hash(
    role: Role,
    origin: &str,
    nonce: &[u8; 32],
    id: DeviceId,
    public: &[u8; 32],
) -> Result<[u8; 32], DeviceKeyError> {
    Ok(bubblebuffer_device_key::relay_proof_hash(
        RELAY_LABEL,
        role.shared(),
        origin,
        nonce,
        id,
        public,
    )?)
}

/// Checks a relay proof and returns the ID it authenticates.
///
/// `claimed_id` must equal the ID derived from `public_key` (the signed ID is the canonical
/// hyphenated form), the hash is recomputed from the supplied fields and the signature is
/// checked with `verify_strict`. Wrong field lengths are errors, never panics.
pub fn verify_relay_proof(
    role: Role,
    origin: &str,
    nonce: &[u8],
    claimed_id: &str,
    public_key: &[u8],
    signature: &[u8],
) -> Result<DeviceId, DeviceKeyError> {
    let nonce: [u8; 32] = nonce
        .try_into()
        .map_err(|_| DeviceKeyError::InvalidLength("nonce"))?;
    let public: [u8; 32] = public_key
        .try_into()
        .map_err(|_| DeviceKeyError::InvalidLength("public"))?;
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| DeviceKeyError::InvalidLength("signature"))?;
    let claimed = DeviceId::parse(claimed_id)?;
    let proof = RelayProof::verify(
        RELAY_LABEL,
        role.shared(),
        origin,
        nonce,
        claimed,
        public,
        signature,
    )?;
    Ok(proof.id)
}

/// The origin the relay signs for a worker URL: `https://host[:port]` for `wss`/`https`,
/// `http://host[:port]` for `ws`/`http`, host lowercased and a default port dropped, matching
/// the relay's canonical origin. `None` for URLs without a usable scheme, host or with userinfo.
pub fn relay_origin(worker_url: &str) -> Option<String> {
    let url = Url::parse(worker_url).ok()?;
    let (scheme, default_port) = match url.scheme() {
        "wss" | "https" => ("https", 443u16),
        "ws" | "http" => ("http", 80u16),
        _ => return None,
    };
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    if host.is_empty() {
        return None;
    }
    Some(match url.port() {
        Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../fixtures/device-keys-contract.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn vector(name: &str) -> Value {
        fixture()["relayProof"]["vectors"]
            .as_array()
            .expect("vectors")
            .iter()
            .find(|vector| vector["name"] == name)
            .expect("vector present")
            .clone()
    }

    fn unhex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn b64(value: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(value).expect("base64url")
    }

    fn nonce_of(vector: &Value) -> [u8; 32] {
        b64(vector["nonce"].as_str().expect("nonce"))
            .try_into()
            .expect("32-byte nonce")
    }

    fn public_of(vector: &Value) -> [u8; 32] {
        b64(vector["public"].as_str().expect("public"))
            .try_into()
            .expect("32-byte public key")
    }

    fn role_of(vector: &Value) -> Role {
        Role::parse(vector["role"].as_str().expect("role")).expect("known role")
    }

    fn seed_of(id: &str) -> [u8; 32] {
        let fixture = fixture();
        let identity = fixture["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .find(|identity| identity["id"] == id)
            .expect("identity present");
        unhex(identity["seedHex"].as_str().expect("seedHex"))
            .try_into()
            .expect("32-byte seed")
    }

    fn identity_of(vector: &Value) -> DeviceIdentity {
        DeviceIdentity::from_seed(&seed_of(vector["id"].as_str().expect("id")))
    }

    #[test]
    fn host_vector_encoding_hash_and_signature_match_the_fixture() {
        let host = vector("host");
        let role = Role::Host;
        let nonce = nonce_of(&host);
        let origin = host["origin"].as_str().expect("origin");
        let id = host["id"].as_str().expect("id");
        let public = public_of(&host);
        let encoding =
            relay_proof_encoding(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public)
                .unwrap();
        assert_eq!(hex(&encoding), host["encodingHex"].as_str().expect("hex"));
        let hash =
            relay_proof_hash(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public).unwrap();
        assert_eq!(hex(&hash), host["sha256Hex"].as_str().expect("hash"));

        let identity = identity_of(&host);
        assert_eq!(identity.id().to_string(), id);
        assert_eq!(
            identity.public_b64(),
            host["public"].as_str().expect("public")
        );
        let signature = identity.sign_relay_proof(role, origin, &nonce);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(signature),
            host["signature"].as_str().expect("signature")
        );
    }

    #[test]
    fn viewer_vector_encoding_hash_and_signature_match_the_fixture() {
        let viewer = vector("viewer");
        let role = Role::Viewer;
        let nonce = nonce_of(&viewer);
        let origin = viewer["origin"].as_str().expect("origin");
        let id = viewer["id"].as_str().expect("id");
        let public = public_of(&viewer);
        let encoding =
            relay_proof_encoding(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public)
                .unwrap();
        assert_eq!(hex(&encoding), viewer["encodingHex"].as_str().expect("hex"));
        let hash =
            relay_proof_hash(role, origin, &nonce, DeviceId::parse(id).unwrap(), &public).unwrap();
        assert_eq!(hex(&hash), viewer["sha256Hex"].as_str().expect("hash"));

        let identity = identity_of(&viewer);
        assert_eq!(identity.id().to_string(), id);
        assert_eq!(
            identity.public_b64(),
            viewer["public"].as_str().expect("public")
        );
        let signature = identity.sign_relay_proof(role, origin, &nonce);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(signature),
            viewer["signature"].as_str().expect("signature")
        );
        // The host role over the same fields is a different message.
        assert_ne!(
            URL_SAFE_NO_PAD.encode(identity.sign_relay_proof(Role::Host, origin, &nonce)),
            viewer["signature"].as_str().expect("signature")
        );
    }

    #[test]
    fn wrong_origin_vector_is_a_different_hash_and_fails_verification() {
        let wrong = vector("wrong-origin");
        let nonce = nonce_of(&wrong);
        let id = wrong["id"].as_str().expect("id");
        let public = public_of(&wrong);
        let origin = wrong["origin"].as_str().expect("origin");
        let hash = relay_proof_hash(
            Role::Host,
            origin,
            &nonce,
            DeviceId::parse(id).unwrap(),
            &public,
        )
        .unwrap();
        assert_eq!(hex(&hash), wrong["sha256Hex"].as_str().expect("hash"));
        let signature = b64(wrong["signature"].as_str().expect("signature"));
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce, id, &public, &signature),
            Err(DeviceKeyError::InvalidSignature)
        ));
        // The same signature is valid for the origin it was made for.
        let host = vector("host");
        let host_origin = host["origin"].as_str().expect("origin");
        assert_eq!(
            verify_relay_proof(Role::Host, host_origin, &nonce, id, &public, &signature)
                .expect("valid proof")
                .to_string(),
            id
        );
    }

    #[test]
    fn verify_relay_proof_accepts_the_fixture_vectors_for_both_roles() {
        for name in ["host", "viewer"] {
            let vector = vector(name);
            let role = role_of(&vector);
            let nonce = nonce_of(&vector);
            let origin = vector["origin"].as_str().expect("origin");
            let id = vector["id"].as_str().expect("id");
            let public = public_of(&vector);
            let signature = b64(vector["signature"].as_str().expect("signature"));
            let verified =
                verify_relay_proof(role, origin, &nonce, id, &public, &signature).expect("valid");
            assert_eq!(verified.to_string(), id);
            // The other role is a different message.
            let other = match role {
                Role::Host => Role::Viewer,
                Role::Viewer => Role::Host,
            };
            assert!(matches!(
                verify_relay_proof(other, origin, &nonce, id, &public, &signature),
                Err(DeviceKeyError::InvalidSignature)
            ));
            // A different nonce is a different message too.
            let mut flipped = nonce;
            flipped[0] ^= 1;
            assert!(matches!(
                verify_relay_proof(role, origin, &flipped, id, &public, &signature),
                Err(DeviceKeyError::InvalidSignature)
            ));
        }
    }

    #[test]
    fn verify_relay_proof_rejects_wrong_lengths_a_foreign_id_and_a_bad_id() {
        let host = vector("host");
        let nonce = nonce_of(&host);
        let origin = host["origin"].as_str().expect("origin");
        let id = host["id"].as_str().expect("id");
        let public = public_of(&host);
        let signature = b64(host["signature"].as_str().expect("signature"));
        assert!(verify_relay_proof(Role::Host, origin, &nonce, id, &public, &signature).is_ok());
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce[..31], id, &public, &signature),
            Err(DeviceKeyError::InvalidLength("nonce"))
        ));
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce, id, &public[..31], &signature),
            Err(DeviceKeyError::InvalidLength("public"))
        ));
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce, id, &public, &signature[..63]),
            Err(DeviceKeyError::InvalidLength("signature"))
        ));
        // Empty inputs are errors, not panics.
        assert!(verify_relay_proof(Role::Host, origin, &[], id, &[], &[]).is_err());
        // The viewer's ID with the host's key is refused before the signature check.
        let viewer_id = fixture()["ids"][0]["id"].as_str().expect("id").to_owned();
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce, &viewer_id, &public, &signature),
            Err(DeviceKeyError::IdMismatch)
        ));
        // An alias spelling authenticates the same key: the proof is over the canonical ID.
        assert!(verify_relay_proof(
            Role::Host,
            origin,
            &nonce,
            &id.to_lowercase().replace('-', ""),
            &public,
            &signature
        )
        .is_ok());
        // Anything that is not an ID is an ID error.
        assert!(matches!(
            verify_relay_proof(Role::Host, origin, &nonce, "nope", &public, &signature),
            Err(DeviceKeyError::InvalidId)
        ));
    }

    #[test]
    fn role_wire_strings_round_trip_and_refuse_other_spellings() {
        for (role, wire) in [(Role::Host, "host"), (Role::Viewer, "viewer")] {
            assert_eq!(role.as_str(), wire);
            assert_eq!(role.to_string(), wire);
            assert_eq!(format!("{role:?}"), wire);
            assert_eq!(Role::parse(wire).expect("wire"), role);
        }
        for bad in ["Host", "VIEWER", " host", "viewer ", "server", ""] {
            assert!(
                matches!(Role::parse(bad), Err(DeviceKeyError::UnknownRole)),
                "accepted {bad:?}"
            );
        }
        // Every fixture vector's role string is a wire string.
        for vector in fixture()["relayProof"]["vectors"]
            .as_array()
            .expect("vectors")
        {
            let wire = vector["role"].as_str().expect("role");
            assert_eq!(role_of(vector).as_str(), wire);
        }
    }

    #[test]
    fn device_identity_derives_the_id_and_never_prints_the_key() {
        let identity = DeviceIdentity::from_seed(&seed_of("77VH-7M56-8GJK-Y12J"));
        assert_eq!(identity.id().to_string(), "77VH-7M56-8GJK-Y12J");
        assert_eq!(
            identity.public_b64(),
            "PUAXw-hDiVqStwqnTRt-vJyYLM8uxJaMwM1V8Sr0Zgw"
        );
        let debug = format!("{identity:?}");
        assert!(debug.contains("77VH-7M56-8GJK-Y12J"));
        assert!(!debug.contains("4ccd") && !debug.contains("PUAX"));
    }

    #[cfg(feature = "keygen")]
    #[test]
    fn generate_makes_distinct_identities_and_their_proofs_verify() {
        let first = DeviceIdentity::generate().expect("entropy");
        let second = DeviceIdentity::generate().expect("entropy");
        assert_ne!(first.public_key(), second.public_key());
        assert_eq!(first.id(), DeviceId::from_public_key(&first.public_key()));
        let origin = "https://example.test";
        let nonce = [7u8; 32];
        let signature = first.sign_relay_proof(Role::Viewer, origin, &nonce);
        assert!(verify_relay_proof(
            Role::Viewer,
            origin,
            &nonce,
            &first.id().to_string(),
            &first.public_key(),
            &signature
        )
        .is_ok());
    }

    #[test]
    fn relay_origin_matches_the_relay_canonical_form() {
        assert_eq!(
            relay_origin("wss://Play.Example/host-ws").as_deref(),
            Some("https://play.example")
        );
        assert_eq!(
            relay_origin("wss://play.example:443/host-ws?pc=x").as_deref(),
            Some("https://play.example")
        );
        assert_eq!(
            relay_origin("wss://play.example:8443/lab-host-ws?run=a").as_deref(),
            Some("https://play.example:8443")
        );
        assert_eq!(
            relay_origin("ws://127.0.0.1:8787/host-ws").as_deref(),
            Some("http://127.0.0.1:8787")
        );
        assert_eq!(
            relay_origin("ws://localhost:80/host-ws").as_deref(),
            Some("http://localhost")
        );
        assert_eq!(
            relay_origin("wss://[::1]:8443/host-ws").as_deref(),
            Some("https://[::1]:8443")
        );
        assert_eq!(relay_origin("wss://user@play.example/host-ws"), None);
        assert_eq!(relay_origin("ftp://play.example/host-ws"), None);
        assert_eq!(relay_origin("/host-ws"), None);
    }
}
