//! Randomness comes into the sans-IO core through this trait. The adapter
//! supplies an OS or platform source; tests supply a deterministic one.

/// A source of random bytes.
pub trait RelayRng {
    /// Fill `buf` with random bytes.
    fn fill(&mut self, buf: &mut [u8]);
}
