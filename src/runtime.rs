//! Environmental values used by mounted-filesystem mutations.
//!
//! A provider can be injected for deterministic tests or an embedding runtime.
//! Formatting has its own UUID policy and is not changed by this interface;
//! its random bytes come from [`fill_random`] here.
//!
//! THIS IS THE ONE PLACE `src/` READS THE CLOCK, THE PROCESS ID OR THE
//! PLATFORM'S RANDOMNESS. On the browser build (wasm32-unknown-unknown) `std`
//! compiles `SystemTime::now`, `Instant::now` and `std::process::id` and
//! panics when they run, so each use here has a JavaScript path beside it.
//! `tests/scripts/test-clock-calls.sh` refuses those calls anywhere else.
use std::sync::atomic::{AtomicU32, Ordering};

pub trait Runtime: Send + Sync {
    /// Seconds since the Unix epoch, matching ext4's legacy timestamp fields.
    fn now_unix_seconds(&self) -> u32;
    /// New inode generation; implementations must avoid immediate reuse.
    fn next_inode_generation(&self) -> u32;
}

/// Native defaults retain the process-ID/counter and SystemTime behavior.
/// Browser builds use JavaScript wall time and a random process-equivalent seed.
pub struct SystemRuntime;
static COUNTER: AtomicU32 = AtomicU32::new(1);

impl Runtime for SystemRuntime {
    fn now_unix_seconds(&self) -> u32 {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            (js_sys::Date::now() / 1000.0) as u32
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0)
        }
    }
    fn next_inode_generation(&self) -> u32 {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        let seed = {
            // Randomize once for this WASM instance, then preserve the native
            // monotonic wrapping counter semantics within the instance.
            static SEED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
            *SEED.get_or_init(|| (js_sys::Math::random() * 4294967296.0) as u32)
        };
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        let seed = std::process::id();
        seed.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Fill `out` with random bytes for identifiers such as a volume UUID. Not a
/// source of key material.
///
/// Native: `/dev/urandom`, falling back to a generator seeded from the time
/// and the process ID when that cannot be read. Browser build: the host's
/// `crypto.getRandomValues`, which every browser and Node 19 or later has,
/// falling back to `Math.random` for a host without it.
pub(crate) fn fill_random(out: &mut [u8]) {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        if !fill_from_web_crypto(out) {
            for b in out.iter_mut() {
                *b = (js_sys::Math::random() * 256.0) as u8;
            }
        }
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            if f.read_exact(out).is_ok() {
                return;
            }
        }
        // Not cryptographic, and it does not need to be: this names a volume.
        let mut state = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xDEADBEEF)
            ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
        for b in out.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 56) as u8;
        }
    }
}

/// `globalThis.crypto.getRandomValues(out)`, or `false` when the host has no
/// such function or it throws.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn fill_from_web_crypto(out: &mut [u8]) -> bool {
    use js_sys::wasm_bindgen::{JsCast, JsValue};
    use js_sys::{Function, Reflect, Uint8Array};

    let Ok(crypto) = Reflect::get(&js_sys::global(), &JsValue::from_str("crypto")) else {
        return false;
    };
    if !crypto.is_object() {
        return false;
    }
    let Ok(get) = Reflect::get(&crypto, &JsValue::from_str("getRandomValues")) else {
        return false;
    };
    let Ok(get) = get.dyn_into::<Function>() else {
        return false;
    };
    let bytes = Uint8Array::new_with_length(out.len() as u32);
    let arg: &JsValue = bytes.as_ref();
    if get.call1(&crypto, arg).is_err() {
        return false;
    }
    bytes.copy_to(out);
    true
}

/// Time since [`Stopwatch::start`], for rate-limiting work such as progress
/// callbacks. Never a timestamp.
///
/// Native: `std::time::Instant`, monotonic. Browser build: the host's
/// `Date.now()`, because `Instant::now` panics there (#295); a wall clock can
/// step backwards, which reads as no time elapsed rather than as a panic.
pub(crate) struct Stopwatch {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    started_ms: f64,
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    started: std::time::Instant,
}

impl Stopwatch {
    pub(crate) fn start() -> Self {
        Self {
            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
            started_ms: js_sys::Date::now(),
            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn elapsed(&self) -> std::time::Duration {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            let ms = (js_sys::Date::now() - self.started_ms).max(0.0);
            std::time::Duration::from_secs_f64(ms / 1000.0)
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            self.started.elapsed()
        }
    }
}
