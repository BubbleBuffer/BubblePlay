//! The relay's randomness, from the Workers runtime's Web Crypto source.

use bp_relay_core::RelayRng;
use worker::js_sys::{Function, Reflect, Uint8Array};
use worker::wasm_bindgen::{JsCast, JsValue};

/// Web Crypto refuses requests above 65536 bytes.
const MAX_CHUNK: usize = 65_536;

/// Fills buffers from `crypto.getRandomValues`. It panics if the runtime has no such source,
/// because the relay cannot run without randomness.
pub struct WebCrypto;

impl RelayRng for WebCrypto {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(MAX_CHUNK) {
            let array = Uint8Array::new_with_length(chunk.len() as u32);
            get_random_values(&array).expect("the Web Crypto random source is unavailable");
            array.copy_to(chunk);
        }
    }
}

fn get_random_values(array: &Uint8Array) -> Result<(), JsValue> {
    let global = worker::js_sys::global();
    let crypto = Reflect::get(&global, &JsValue::from_str("crypto"))?;
    let function: Function =
        Reflect::get(&crypto, &JsValue::from_str("getRandomValues"))?.dyn_into()?;
    let argument: &JsValue = array.as_ref();
    function.call1(&crypto, argument)?;
    Ok(())
}
