//! The randomness the core needs, supplied by the adapter.
//!
//! The core never reads OS randomness itself. A native adapter passes a
//! getrandom-backed source; the Durable Object adapter passes crypto.getRandomValues.

/// A source of random bytes.
pub trait RelayRng {
    /// Fills `buf` completely with random bytes.
    fn fill(&mut self, buf: &mut [u8]);
}
