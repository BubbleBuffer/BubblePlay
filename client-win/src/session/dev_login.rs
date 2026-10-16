//! Dev-only password and TOTP login for `run_dev`. It reads the relay password
//! and the Authenticator secret from local files, sends exactly one login POST
//! through `net::login_blocking` (no redirects, no retries, loopback bypasses
//! proxies), and returns the session cookie jar. Passwords, secrets, codes and
//! cookie values never reach logs, errors or `Debug`.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::net::{
    self, CookieJar, LoginError, LoginRequest, NetError, GATE_COOKIE, SESSION_COOKIE,
};

type HmacSha1 = Hmac<Sha1>;

const TOTP_STEP_SECS: u64 = 30;
const TOTP_DIGITS: u32 = 6;
/// Prefixes written by `writeProvision` in `relay/init.go`.
const SECRET_FIELD: &str = "Authenticator secret:";
const ENROLLMENT_FIELD: &str = "Authenticator enrollment URI:";

/// Local inputs for one dev login. The fields hold the origin and file paths only.
#[derive(Clone)]
pub struct DevLogin {
    /// Exact relay origin, for example `https://relay.example` or `http://127.0.0.1:8787`.
    pub origin: String,
    /// File holding the relay password. Only trailing CR/LF is removed.
    pub password_file: PathBuf,
    /// The `provision.txt` the relay wrote (`relay/init.go`).
    pub provision_file: PathBuf,
}

impl fmt::Debug for DevLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevLogin")
            .field("origin", &display_origin(&self.origin))
            .field("password_file", &self.password_file)
            .field("provision_file", &self.provision_file)
            .finish()
    }
}

/// Scheme, host and port only. An origin with userinfo, a query or a fragment is hidden.
fn display_origin(origin: &str) -> String {
    match url::Url::parse(origin) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https")
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none() =>
        {
            parsed.origin().ascii_serialization()
        }
        _ => "<unparsed origin>".to_owned(),
    }
}

/// Run the one dev login: read the password and secret, compute the current
/// TOTP, and POST once. Blocks on its own runtime, so call it from a plain
/// thread and not from inside a tokio runtime.
pub fn login(dev: &DevLogin) -> anyhow::Result<CookieJar> {
    validate_origin(&dev.origin)?;
    let password = read_password(&dev.password_file)?;
    let secret = read_totp_secret(&dev.provision_file)?;
    let code = totp_at(&secret, unix_now()?, TOTP_DIGITS)?;
    let request = LoginRequest {
        password: password.as_str(),
        totp: Some(code.as_str()),
        remember: false,
    };
    let jar = net::login_blocking(&dev.origin, &request)
        .map_err(|error| anyhow!(login_failure(&error)))?;
    if jar.get(GATE_COOKIE).is_none() || jar.get(SESSION_COOKIE).is_none() {
        bail!("dev login: HTTP 204 without both session cookies");
    }
    eprintln!(
        "dev login: HTTP 204; kept cookies {} and {}",
        GATE_COOKIE, SESSION_COOKIE
    );
    Ok(jar)
}

/// Validate before any file read or network activity. The development tunnel
/// can only terminate on this machine; userinfo/path/query are never accepted.
pub fn validate_origin(origin: &str) -> anyhow::Result<url::Url> {
    let parsed = url::Url::parse(origin)
        .map_err(|_| anyhow!("dev relay requires an http/https loopback origin"))?;
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if !loopback
        || !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        bail!("dev relay requires an http/https loopback origin without credentials, path, query or fragment");
    }
    Ok(parsed)
}

/// User-facing text for a failed login. It names the numeric status and never
/// includes the origin, the transport message or the response body.
fn login_failure(error: &NetError) -> String {
    match error {
        NetError::Login(LoginError::Rejected) => "dev login rejected: HTTP 401".to_owned(),
        NetError::Login(LoginError::RateLimited {
            retry_after_secs: Some(secs),
        }) => format!("dev login rate limited: HTTP 429 (Retry-After {secs} s)"),
        NetError::Login(LoginError::RateLimited {
            retry_after_secs: None,
        }) => "dev login rate limited: HTTP 429".to_owned(),
        NetError::Login(LoginError::Unavailable) => {
            "dev login failed: HTTP 503 (relay unavailable)".to_owned()
        }
        NetError::Login(LoginError::Status(status)) => format!("dev login failed: HTTP {status}"),
        NetError::Login(LoginError::MissingCookies) => {
            "dev login: HTTP 204 without both session cookies".to_owned()
        }
        NetError::Login(LoginError::Transport(_)) => {
            "dev login transport failed (no HTTP response)".to_owned()
        }
        NetError::InvalidRelayUrl(_) | NetError::PlaintextRefused(_) => {
            "dev login origin must be https://, or http://127.0.0.1".to_owned()
        }
        _ => "dev login failed".to_owned(),
    }
}

/// The password as stored, minus trailing CR/LF. Leading and inner spaces are kept.
fn read_password(path: &Path) -> anyhow::Result<String> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read password file {}", path.display()))?;
    let password = text.trim_end_matches(&['\r', '\n'][..]);
    if password.is_empty() {
        bail!("password file {} is empty", path.display());
    }
    Ok(password.to_owned())
}

fn read_totp_secret(path: &Path) -> anyhow::Result<Vec<u8>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read provision file {}", path.display()))?;
    parse_provision(&text)
}

/// The Authenticator secret from the secret line, the enrollment URI, or both
/// when they agree. Conflicting or repeated entries are rejected.
fn parse_provision(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut field: Option<Vec<u8>> = None;
    let mut uri: Option<Vec<u8>> = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(SECRET_FIELD) {
            if field.is_some() {
                bail!("provision file repeats the Authenticator secret");
            }
            field = Some(decode_secret(value.trim())?);
        } else if let Some(value) = line.strip_prefix(ENROLLMENT_FIELD) {
            if uri.is_some() {
                bail!("provision file repeats the Authenticator enrollment URI");
            }
            uri = Some(enrollment_secret(value.trim())?);
        }
    }
    match (field, uri) {
        (Some(a), Some(b)) if a != b => {
            bail!("provision file has conflicting Authenticator secrets")
        }
        (Some(secret), _) | (None, Some(secret)) => Ok(secret),
        (None, None) => bail!("provision file has no Authenticator secret"),
    }
}

/// The secret from an `otpauth://totp/` URI. The URI must state SHA1, 6 digits
/// and a 30-second period, the values the relay writes.
fn enrollment_secret(uri: &str) -> anyhow::Result<Vec<u8>> {
    let parsed =
        url::Url::parse(uri).map_err(|_| anyhow!("enrollment URI is not a valid otpauth URI"))?;
    if parsed.scheme() != "otpauth" || parsed.host_str() != Some("totp") {
        bail!("enrollment URI is not an otpauth totp URI");
    }
    let mut secret: Option<String> = None;
    let mut algorithm: Option<String> = None;
    let mut digits: Option<String> = None;
    let mut period: Option<String> = None;
    for (key, value) in parsed.query_pairs() {
        let slot = match &*key {
            "secret" => &mut secret,
            "algorithm" => &mut algorithm,
            "digits" => &mut digits,
            "period" => &mut period,
            _ => continue,
        };
        if slot.is_some() {
            bail!("enrollment URI repeats the {} parameter", key);
        }
        *slot = Some(value.into_owned());
    }
    if algorithm.as_deref() != Some("SHA1")
        || digits.as_deref() != Some("6")
        || period.as_deref() != Some("30")
    {
        bail!("enrollment URI must use SHA1, 6 digits and a 30-second period");
    }
    let secret = secret.ok_or_else(|| anyhow!("enrollment URI has no secret"))?;
    decode_secret(&secret)
}

/// The relay's secret encoding: unpadded uppercase base32 of 20 to 64 bytes
/// (`relay/auth.go`).
fn decode_secret(encoded: &str) -> anyhow::Result<Vec<u8>> {
    decode_base32(encoded)
        .filter(|secret| (20..=64).contains(&secret.len()))
        .ok_or_else(|| {
            anyhow!("Authenticator secret is not unpadded uppercase base32 of 20 to 64 bytes")
        })
}

/// Strict RFC 4648 base32: uppercase alphabet, no padding, canonical trailing bits.
fn decode_base32(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Five or more leftover bits mean an extra character, and nonzero leftover
    // bits are a non-canonical encoding.
    (bits < 5 && buffer == 0).then_some(out)
}

/// RFC 6238 TOTP with HMAC-SHA1 and dynamic truncation (RFC 4226 section 5.3).
fn totp_at(secret: &[u8], unix_secs: u64, digits: u32) -> anyhow::Result<String> {
    let counter = unix_secs / TOTP_STEP_SECS;
    let mut mac = HmacSha1::new_from_slice(secret).map_err(|_| anyhow!("TOTP key rejected"))?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let code = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    Ok(format!(
        "{:0width$}",
        code % 10u32.pow(digits),
        width = digits as usize
    ))
}

fn unix_now() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| anyhow!("system clock is before 1970; cannot compute a TOTP"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{ErrorKind, Read, Write},
        net::{TcpListener, TcpStream},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    /// "12345678901234567890" in the relay's encoding.
    const SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    /// A different valid 20-byte secret, for conflict tests.
    const OTHER_B32: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    const GOOD_QUERY: &str = "algorithm=SHA1&digits=6&issuer=BubblePlay&period=30&secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    /// How long the recorder waits for more connections after the last one.
    const QUIET: Duration = Duration::from_millis(300);

    /// Removes the directory when the test ends, even on failure.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static SERIAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "bubbleplay-dev-login-{}-{name}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let target = self.0.canonicalize().ok();
            let parent = std::env::temp_dir().canonicalize().ok();
            if let (Some(target), Some(parent)) = (target, parent) {
                if target.parent() == Some(parent.as_path())
                    && target.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .starts_with(&format!("bubbleplay-dev-login-{}-", std::process::id()))
                    })
                {
                    let _ = fs::remove_dir_all(target);
                }
            }
        }
    }

    /// An owned loopback server. It answers every connection with the same
    /// response and returns the raw requests once it has been quiet for `QUIET`.
    /// The caller joins it through `finish`, so no thread outlives its test.
    struct Recorder {
        origin: String,
        handle: JoinHandle<Vec<String>>,
    }

    impl Recorder {
        fn finish(self) -> Vec<String> {
            self.handle.join().unwrap()
        }
    }

    fn serve(response: String) -> Recorder {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            let mut deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        requests.push(read_request(&mut stream));
                        let _ = stream.write_all(response.as_bytes());
                        deadline = Instant::now() + QUIET;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            requests
        });
        Recorder { origin, handle }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut data = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            data.extend_from_slice(&chunk[..read]);
            if let Some(head_end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")?
                            .trim()
                            .parse::<usize>()
                            .ok()
                    })
                    .unwrap_or(0);
                if data.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    /// Trimmed value of the first header named `name` (case-insensitive).
    fn header(request: &str, name: &str) -> Option<String> {
        request
            .lines()
            .skip(1)
            .take_while(|line| !line.is_empty())
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
    }

    fn body_of(request: &str) -> &str {
        request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("")
    }

    fn provision_text(secret: &str) -> String {
        format!(
            "PRIVATE — never publish this file or configuration.\n\
             Host endpoint: wss://127.0.0.1:8787/host-ws\n\
             TOKEN: token-value-1\n\
             HOST_GATE_TOKEN: host-gate-value-2\n\
             GATE_ADMIN_TOKEN: admin-value-3\n\
             Approval methods: totp\n\
             Authenticator enrollment URI: otpauth://totp/BubblePlay%3A127.0.0.1%3A8787?algorithm=SHA1&digits=6&issuer=BubblePlay&period=30&secret={secret}\n\
             Authenticator secret: {secret}\n"
        )
    }

    fn enrollment_line(query: &str) -> String {
        format!(
            "Authenticator enrollment URI: otpauth://totp/BubblePlay%3A127.0.0.1%3A8787?{query}"
        )
    }

    fn dev_login(origin: &str, password: &Path, provision: &Path) -> DevLogin {
        DevLogin {
            origin: origin.to_owned(),
            password_file: password.to_path_buf(),
            provision_file: provision.to_path_buf(),
        }
    }

    fn password_of(dir: &TempDir, contents: &[u8]) -> anyhow::Result<String> {
        read_password(&dir.file("pw.txt", contents))
    }

    fn assert_no_echo(error: &anyhow::Error, secrets: &[&str]) {
        let shown = format!("{error:#}");
        for secret in secrets {
            assert!(!shown.contains(secret), "error text echoed a secret value");
        }
    }

    #[test]
    fn totp_matches_rfc6238_sha1_vectors_in_eight_and_six_digits() {
        let secret = b"12345678901234567890";
        for (time, eight, six) in [
            (59, "94287082", "287082"),
            (1_111_111_109, "07081804", "081804"),
            (1_111_111_111, "14050471", "050471"),
            (1_234_567_890, "89005924", "005924"),
            (2_000_000_000, "69279037", "279037"),
            (20_000_000_000, "65353130", "353130"),
        ] {
            assert_eq!(
                totp_at(secret, time, 8).unwrap(),
                eight,
                "T={time}, 8 digits"
            );
            assert_eq!(totp_at(secret, time, 6).unwrap(), six, "T={time}, 6 digits");
        }
    }

    #[test]
    fn base32_is_strict_and_matches_the_relay_encoding() {
        assert_eq!(
            decode_base32(SECRET_B32).unwrap(),
            b"12345678901234567890".to_vec()
        );
        assert_eq!(decode_base32("MZXW6"), Some(b"foo".to_vec()));
        for refused in ["MZXW7", "MZXW6=", "mzxw6", "MZX1", "MZXW6A", "MZXW 6"] {
            assert_eq!(decode_base32(refused), None, "{refused} must be refused");
        }
        // The relay accepts 20 to 64 bytes only.
        assert!(decode_secret("GEZDGNBVGY3TQOJQ").is_err());
    }

    #[test]
    fn password_file_keeps_spaces_and_trims_only_trailing_crlf() {
        let dir = TempDir::new("password");
        assert_eq!(
            password_of(&dir, b"  two words \r\n").unwrap(),
            "  two words "
        );
        assert_eq!(password_of(&dir, b"pw\n").unwrap(), "pw");
        assert_eq!(password_of(&dir, b"pw").unwrap(), "pw");
        assert_eq!(password_of(&dir, b"pw\r\n\r\n").unwrap(), "pw");
        assert_eq!(password_of(&dir, b"  ").unwrap(), "  ");
        assert!(
            password_of(&dir, b"\r\n").is_err(),
            "empty password refused"
        );
        assert!(
            password_of(&dir, &[0xff, 0xfe]).is_err(),
            "non-UTF-8 refused"
        );
    }

    #[test]
    fn provision_accepts_field_uri_or_both_and_rejects_bad_or_conflicting_secrets() {
        let secret = decode_base32(SECRET_B32).unwrap();
        assert_eq!(
            parse_provision(&provision_text(SECRET_B32)).unwrap(),
            secret
        );
        let field_only = format!("Authenticator secret: {SECRET_B32}\n");
        assert_eq!(parse_provision(&field_only).unwrap(), secret);
        let uri_only = format!("{}\n", enrollment_line(GOOD_QUERY));
        assert_eq!(parse_provision(&uri_only).unwrap(), secret);

        let secrets = [SECRET_B32, OTHER_B32];
        let conflict = format!(
            "{}\nAuthenticator secret: {OTHER_B32}\n",
            enrollment_line(GOOD_QUERY)
        );
        let error = parse_provision(&conflict).unwrap_err();
        assert_no_echo(&error, &secrets);

        let bad_inputs = [
            enrollment_line(&GOOD_QUERY.replace("SHA1", "SHA512")),
            enrollment_line(&GOOD_QUERY.replace("digits=6", "digits=8")),
            enrollment_line(&GOOD_QUERY.replace("period=30", "period=60")),
            enrollment_line(&format!("{GOOD_QUERY}&secret={OTHER_B32}")),
            format!(
                "{}\nAuthenticator enrollment URI: {}\n",
                enrollment_line(GOOD_QUERY),
                enrollment_line(GOOD_QUERY)
            ),
            format!("Authenticator secret: {SECRET_B32}\nAuthenticator secret: {SECRET_B32}\n"),
            "Host endpoint: wss://127.0.0.1:8787/host-ws\n".to_owned(),
            format!("Authenticator enrollment URI: https://totp/x?{GOOD_QUERY}\n"),
            "Authenticator secret: notbase32-SECRETVALUE\n".to_owned(),
            format!("Authenticator secret: {}=\n", &SECRET_B32[..31]),
        ];
        for bad in &bad_inputs {
            let error = parse_provision(bad).unwrap_err();
            assert_no_echo(&error, &secrets);
            assert_no_echo(&error, &["SECRETVALUE"]);
        }
    }

    #[test]
    fn unreadable_provision_files_error_without_echoing_content() {
        let dir = TempDir::new("provision-files");
        let provision = dir.file("provision.txt", b"Authenticator secret: bad-SECRETVALUE\n");
        let password = dir.file("pw.txt", b"pw\n");
        let error = login(&dev_login("http://127.0.0.1:1", &password, &provision)).unwrap_err();
        assert_no_echo(&error, &["SECRETVALUE"]);
        let missing = dir.0.join("absent.txt");
        let error = login(&dev_login("http://127.0.0.1:1", &missing, &provision)).unwrap_err();
        assert!(
            format!("{error:#}").contains("absent.txt"),
            "paths are fine"
        );
    }

    #[test]
    fn debug_shows_paths_and_a_clean_origin_only() {
        let hidden = DevLogin {
            origin: "https://user:hunter2@relay.example".into(),
            password_file: "pw.txt".into(),
            provision_file: "provision.txt".into(),
        };
        let shown = format!("{hidden:?}");
        assert!(!shown.contains("hunter2"), "userinfo must not be printed");
        assert!(shown.contains("pw.txt") && shown.contains("provision.txt"));

        let plain = DevLogin {
            origin: "https://relay.example/".into(),
            ..hidden
        };
        assert!(format!("{plain:?}").contains("https://relay.example"));
    }

    #[test]
    fn origin_errors_never_echo_the_origin_and_send_nothing() {
        let dir = TempDir::new("origin");
        let password = dir.file("pw.txt", b"pw\n");
        let provision = dir.file("provision.txt", provision_text(SECRET_B32).as_bytes());
        let error = login(&dev_login(
            "https://user:hunter2@relay.example",
            &password,
            &provision,
        ))
        .unwrap_err();
        assert_no_echo(&error, &["hunter2"]);
        let error = login(&dev_login("http://relay.example", &password, &provision)).unwrap_err();
        assert!(format!("{error:#}").contains("loopback"));
    }

    #[test]
    fn origins_are_loopback_only_and_refused_before_file_reads() {
        for accepted in [
            "http://127.0.0.1:8788",
            "https://127.0.0.1",
            "http://[::1]:8788",
            "https://[::1]",
            "http://localhost:8788",
            "https://LOCALHOST",
        ] {
            assert!(validate_origin(accepted).is_ok());
        }
        for refused in [
            "http://example.com",
            "https://example.com",
            "http://127.0.0.2",
            "http://[::2]",
            "http://localhost.example.com",
            "file:///tmp/x",
            "ws://127.0.0.1",
            "http://user:password@localhost",
            "http://localhost/path",
            "http://localhost?secret=value",
            "http://localhost#value",
        ] {
            let error = login(&dev_login(
                refused,
                Path::new("absent-password"),
                Path::new("absent-provision"),
            ))
            .unwrap_err();
            let printed = format!("{error:#}");
            assert!(printed.contains("loopback"));
            assert!(!printed.contains("cannot read"));
            assert!(!printed.contains("password@"));
            assert!(!printed.contains("secret=value"));
        }
    }

    /// Subprocess target for capturing the real login logging and the same
    /// stats serializer used by run_dev. No window or desktop API is involved.
    #[test]
    fn login_log_and_stats_probe() {
        let Some(origin) = std::env::var_os("BP_LOGIN_PROBE_ORIGIN") else {
            return;
        };
        let password = PathBuf::from(std::env::var_os("BP_LOGIN_PROBE_PASSWORD_FILE").unwrap());
        let provision = PathBuf::from(std::env::var_os("BP_LOGIN_PROBE_PROVISION_FILE").unwrap());
        let stats_path = PathBuf::from(std::env::var_os("BP_LOGIN_PROBE_STATS").unwrap());
        let jar = login(&dev_login(&origin.to_string_lossy(), &password, &provision))
            .expect("sanitized fake login succeeds");
        eprintln!("cookie jar: {jar:?}");
        let summary = super::super::metrics::PresentationMetrics::default().summary(
            &crate::media::StatsSnapshot::default(),
            &crate::net::InputSnapshot::default(),
            &[],
        );
        super::super::metrics::write_summary(&stats_path, &summary).unwrap();
    }

    #[test]
    fn fake_login_captured_logs_and_stats_never_contain_read_secrets() {
        let dir = TempDir::new("captured-redaction");
        let password_value = "bp-fake-password-DO-NOT-LOG";
        let password = dir.file("pw.txt", format!("{password_value}\r\n").as_bytes());
        let provision_value = provision_text(SECRET_B32);
        let provision = dir.file("provision.txt", provision_value.as_bytes());
        let cookie_headers="Set-Cookie: __Host-gate=bp-fake-gate-DO-NOT-LOG; Path=/; Secure\r\nSet-Cookie: __Host-bp-session=bp-fake-session-DO-NOT-LOG; Path=/; Secure\r\n";
        let server = serve(response("204 No Content", cookie_headers, ""));
        let stats = dir.0.join("stats-out.json");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::dev_login::tests::login_log_and_stats_probe",
                "--nocapture",
            ])
            .env("BP_LOGIN_PROBE_ORIGIN", &server.origin)
            .env("BP_LOGIN_PROBE_PASSWORD_FILE", password)
            .env("BP_LOGIN_PROBE_PROVISION_FILE", provision)
            .env("BP_LOGIN_PROBE_STATS", &stats)
            .output()
            .unwrap();
        let requests = server.finish();
        assert!(child.status.success(), "fake login subprocess failed");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_str(body_of(&requests[0])).unwrap();
        let mut captured = String::from_utf8_lossy(&child.stdout).into_owned();
        captured.push_str(&String::from_utf8_lossy(&child.stderr));
        let stats_text = fs::read_to_string(stats).unwrap();
        for secret in [
            password_value,
            SECRET_B32,
            "12345678901234567890",
            "bp-fake-gate-DO-NOT-LOG",
            "bp-fake-session-DO-NOT-LOG",
            "token-value-1",
            "host-gate-value-2",
            "admin-value-3",
            body["totp"].as_str().unwrap(),
            provision_value.as_str(),
        ] {
            assert!(
                !captured.contains(secret),
                "captured log contains a read secret"
            );
            assert!(
                !stats_text.contains(secret),
                "stats output contains a read secret"
            );
        }
        assert!(captured.contains(GATE_COOKIE) && captured.contains(SESSION_COOKIE));
        fs::write(dir.0.join("captured.log"), captured).unwrap();
    }

    #[test]
    fn success_posts_once_with_origin_and_json_and_keeps_both_cookies_over_http() {
        let dir = TempDir::new("success");
        let password = dir.file("pw.txt", b"  pw with spaces \r\n");
        let provision = dir.file("provision.txt", provision_text(SECRET_B32).as_bytes());
        let cookies = "Set-Cookie: __Host-gate=gate-value; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=3600\r\n\
                       Set-Cookie: __Host-bp-session=session-value; Path=/; Secure; HttpOnly; SameSite=Lax\r\n";
        let server = serve(response("204 No Content", cookies, ""));
        let origin = server.origin.clone();

        let before = unix_now().unwrap();
        let jar = login(&dev_login(&origin, &password, &provision)).unwrap();
        let after = unix_now().unwrap();
        let requests = server.finish();

        assert_eq!(requests.len(), 1, "exactly one POST");
        let request = &requests[0];
        assert!(
            request.starts_with("POST /__auth/login HTTP/1.1\r\n"),
            "request line"
        );
        assert_eq!(header(request, "origin").as_deref(), Some(origin.as_str()));

        let body: serde_json::Value = serde_json::from_str(body_of(request)).unwrap();
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 3, "only password, totp and remember are sent");
        assert_eq!(object["password"], "  pw with spaces ");
        assert_eq!(object["remember"], false);
        let secret = decode_base32(SECRET_B32).unwrap();
        let codes = [
            totp_at(&secret, before, 6).unwrap(),
            totp_at(&secret, after, 6).unwrap(),
        ];
        let totp = object["totp"].as_str().unwrap();
        assert!(
            codes.iter().any(|code| code == totp),
            "current RFC 6238 code"
        );

        assert_eq!(jar.get(GATE_COOKIE), Some("gate-value"));
        assert_eq!(jar.get(SESSION_COOKIE), Some("session-value"));
    }

    #[test]
    fn non_204_statuses_make_one_request_and_never_follow_or_retry() {
        let cases = [
            ("401 Unauthorized", "", "HTTP 401"),
            ("429 Too Many Requests", "Retry-After: 7\r\n", "HTTP 429"),
            (
                "307 Temporary Redirect",
                "Location: /elsewhere\r\n",
                "HTTP 307",
            ),
        ];
        for (status, headers, expected) in cases {
            let dir = TempDir::new(&format!("status-{}", &status[..3]));
            let password = dir.file("pw.txt", b"pw-secret-value\n");
            let provision = dir.file("provision.txt", provision_text(SECRET_B32).as_bytes());
            let server = serve(response(status, headers, "body-echo-value"));
            let origin = server.origin.clone();

            let error = login(&dev_login(&origin, &password, &provision)).unwrap_err();
            let requests = server.finish();

            assert!(
                format!("{error:#}").contains(expected),
                "{status}: {error:#}"
            );
            assert_no_echo(&error, &["body-echo-value", "pw-secret-value"]);
            assert_eq!(
                requests.len(),
                1,
                "{status}: one POST, no retry or redirect"
            );
        }
    }

    #[test]
    fn a_204_without_both_session_cookies_is_refused() {
        let dir = TempDir::new("missing-cookie");
        let password = dir.file("pw.txt", b"pw\n");
        let provision = dir.file("provision.txt", provision_text(SECRET_B32).as_bytes());
        let server = serve(response(
            "204 No Content",
            "Set-Cookie: __Host-gate=only; Path=/\r\n",
            "",
        ));
        let origin = server.origin.clone();
        let error = login(&dev_login(&origin, &password, &provision)).unwrap_err();
        let requests = server.finish();
        assert!(format!("{error:#}").contains("without both session cookies"));
        assert_eq!(requests.len(), 1);
    }
}
