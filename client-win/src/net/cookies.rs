//! Session cookies from the relay login. Only the two names the relay sets are kept.

use std::collections::BTreeMap;
use std::fmt;

/// Gate cookie set by `POST /__auth/login` (`relay/auth.go`).
pub const GATE_COOKIE: &str = "__Host-gate";
/// Session cookie set by `POST /__auth/login` (`relay/auth.go`).
pub const SESSION_COOKIE: &str = "__Host-bp-session";

const RETAINED: [&str; 2] = [GATE_COOKIE, SESSION_COOKIE];

/// Cookies to send on every socket upgrade. Values are secrets: `Debug` prints names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    values: BTreeMap<&'static str, String>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a jar from `Set-Cookie` header values. Unknown names are ignored, and a
    /// `Max-Age` of zero or less (or an empty value) removes the cookie.
    pub fn from_set_cookie<'a>(headers: impl IntoIterator<Item = &'a str>) -> Self {
        let mut jar = Self::new();
        jar.apply_set_cookie(headers);
        jar
    }

    /// Apply `Set-Cookie` header values to this jar (see [`CookieJar::from_set_cookie`]).
    pub fn apply_set_cookie<'a>(&mut self, headers: impl IntoIterator<Item = &'a str>) {
        for header in headers {
            let mut parts = header.split(';');
            let pair = parts.next().unwrap_or("");
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            let Some(key) = retained_name(name) else {
                continue;
            };
            let expired = parts.any(|attribute| {
                let attribute = attribute.trim();
                attribute
                    .split_once('=')
                    .filter(|(attr, _)| attr.trim().eq_ignore_ascii_case("max-age"))
                    .and_then(|(_, age)| age.trim().parse::<i64>().ok())
                    .is_some_and(|age| age <= 0)
            });
            if expired || value.is_empty() {
                self.values.remove(key);
            } else {
                self.values.insert(key, value.to_owned());
            }
        }
    }

    /// Store one cookie. Returns `false` (and stores nothing) for names the jar does not keep.
    pub fn insert(&mut self, name: &str, value: &str) -> bool {
        let Some(key) = retained_name(name) else {
            return false;
        };
        if value.is_empty() {
            self.values.remove(key);
        } else {
            self.values.insert(key, value.to_owned());
        }
        true
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        retained_name(name)
            .and_then(|key| self.values.get(key))
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The `Cookie` header value, gate cookie first. `None` when the jar is empty.
    pub fn cookie_header(&self) -> Option<String> {
        let pairs: Vec<String> = RETAINED
            .iter()
            .filter_map(|name| self.values.get(name).map(|value| format!("{name}={value}")))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }
}

fn retained_name(name: &str) -> Option<&'static str> {
    RETAINED.iter().copied().find(|retained| *retained == name)
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.values.keys().copied().collect();
        f.debug_struct("CookieJar").field("names", &names).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_both_relay_cookies_and_builds_header_in_fixed_order() {
        let jar = CookieJar::from_set_cookie([
            "__Host-bp-session=sess-token; Path=/; Secure; HttpOnly; SameSite=Lax",
            "__Host-gate=gate-token; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=3600",
        ]);
        assert_eq!(jar.get(GATE_COOKIE), Some("gate-token"));
        assert_eq!(jar.get(SESSION_COOKIE), Some("sess-token"));
        assert_eq!(
            jar.cookie_header().as_deref(),
            Some("__Host-gate=gate-token; __Host-bp-session=sess-token")
        );
    }

    #[test]
    fn ignores_unknown_cookie_names() {
        let jar = CookieJar::from_set_cookie(["theme=dark; Path=/", "__Host-other=x"]);
        assert!(jar.is_empty());
        assert_eq!(jar.cookie_header(), None);
    }

    #[test]
    fn max_age_zero_or_negative_removes_a_cookie() {
        let mut jar = CookieJar::new();
        jar.insert(GATE_COOKIE, "old");
        jar.insert(SESSION_COOKIE, "old");
        jar.apply_set_cookie(["__Host-gate=x; Path=/; Max-Age=0"]);
        assert_eq!(jar.get(GATE_COOKIE), None);
        assert_eq!(jar.get(SESSION_COOKIE), Some("old"));
        jar.apply_set_cookie(["__Host-bp-session=gone; Max-Age=-1"]);
        assert!(jar.is_empty(), "expired cookies must not remain: {jar:?}");
    }

    #[test]
    fn empty_value_and_attribute_case_and_spacing_are_handled() {
        let jar = CookieJar::from_set_cookie(["__Host-gate=abc; max-age = 0"]);
        assert!(jar.is_empty());
        let jar = CookieJar::from_set_cookie(["__Host-gate=; Path=/"]);
        assert!(jar.is_empty());
    }

    #[test]
    fn value_containing_equals_keeps_everything_after_first_equals() {
        let jar = CookieJar::from_set_cookie(["__Host-gate=ab=cd==; Path=/"]);
        assert_eq!(jar.get(GATE_COOKIE), Some("ab=cd=="));
    }

    #[test]
    fn insert_rejects_unknown_names_and_empty_value_removes() {
        let mut jar = CookieJar::new();
        assert!(!jar.insert("other", "x"));
        assert!(jar.insert(GATE_COOKIE, "g"));
        assert_eq!(jar.get(GATE_COOKIE), Some("g"));
        assert!(jar.insert(GATE_COOKIE, ""));
        assert_eq!(jar.get(GATE_COOKIE), None);
    }

    #[test]
    fn debug_never_prints_values() {
        let jar = CookieJar::from_set_cookie(["__Host-gate=super-secret-value"]);
        let printed = format!("{jar:?}");
        assert!(printed.contains(GATE_COOKIE));
        assert!(!printed.contains("super-secret-value"));
    }

    #[test]
    fn single_cookie_header_has_no_separator() {
        let jar = CookieJar::from_set_cookie(["__Host-bp-session=only"]);
        assert_eq!(
            jar.cookie_header().as_deref(),
            Some("__Host-bp-session=only")
        );
    }
}
