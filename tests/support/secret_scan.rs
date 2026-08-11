#![allow(
    dead_code,
    reason = "support utilities used by future integration tests"
)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

/// Guards every live `SensitiveFixture` against the one test that scans the
/// whole process's writable memory. Every fixture holds the shared (read)
/// half for its entire lifetime; the memory-scan test instead takes the
/// exclusive (write) half around the scan itself, which blocks until every
/// other fixture in the process has been dropped -- and, per `Drop for
/// SensitiveFixture` below, zeroized. Without this, `cargo test`'s default
/// in-process, multi-threaded harness (unlike `cargo nextest`, which runs
/// each test in its own process) can let the scan observe another test's
/// marker while that test is still genuinely mid-flight, not just stale.
static SCAN_LOCK: RwLock<()> = RwLock::new(());

/// A set of unique synthetic secret markers used to detect leaks.
///
/// Each marker is generated once per fixture so that tests do not accidentally
/// match real config values or example strings.
pub struct SensitiveFixture {
    /// Synthetic `nsec` value. Kept as a 64-character hex string so it is valid
    /// for `LocalKey::parse` while still being easy to search for.
    pub nsec_marker: String,
    /// Raw 32-byte secret key bytes corresponding to `nsec_marker`.
    nsec_marker_bytes: [u8; 32],
    /// Synthetic bunker URI substring.
    pub bunker_uri_marker: String,
    /// Synthetic HTTP secret token.
    pub http_token_marker: String,
    /// Synthetic 32-byte attachment key encoded as 64 hex characters.
    pub attachment_key_marker: String,
    /// Synthetic 16-byte attachment nonce encoded as 32 hex characters.
    pub attachment_nonce_marker: String,
    /// Synthetic decrypted attachment plaintext marker.
    pub attachment_plaintext_marker: String,
    /// Held for the fixture's lifetime so `SCAN_LOCK`'s writer (the
    /// memory-scan test) can never observe this fixture concurrently.
    /// `None` only for the fixture built by that same test, which takes the
    /// write half itself instead (see `new_unguarded`).
    _scan_guard: Option<RwLockReadGuard<'static, ()>>,
}

impl SensitiveFixture {
    /// Create a new fixture with fresh markers.
    pub fn new() -> Self {
        Self::build(Some(SCAN_LOCK.read()))
    }

    /// Build a fixture without taking `SCAN_LOCK`'s read half. For use only
    /// by the memory-scan test itself, which takes the write half instead
    /// (via `acquire_exclusive_scan_lock`) around the scan.
    pub fn new_unguarded() -> Self {
        Self::build(None)
    }

    /// Block until every other live `SensitiveFixture` in the process has
    /// been dropped (and zeroized), then hold `SCAN_LOCK` exclusively so
    /// none can spring up mid-scan. Intended to be held only around the
    /// scan itself.
    pub fn acquire_exclusive_scan_lock() -> RwLockWriteGuard<'static, ()> {
        SCAN_LOCK.write()
    }

    #[allow(clippy::expect_used)]
    fn build(scan_guard: Option<RwLockReadGuard<'static, ()>>) -> Self {
        let first = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let second = Zeroizing::new(Uuid::new_v4().as_simple().to_string());

        // Build the 64-character nsec marker in a single heap allocation so the
        // only full copy is the live buffer.
        let mut nsec_marker = String::with_capacity(64);
        nsec_marker.push_str(&first);
        nsec_marker.push_str(&second);

        // Decode into a zeroizing temporary so the raw secret bytes are not
        // left behind in a freed `Vec`.
        let mut nsec_bytes_buf = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(&nsec_marker, nsec_bytes_buf.as_mut())
            .expect("UUID simple form is hex");
        let nsec_marker_bytes = *nsec_bytes_buf;

        // Build each concatenated marker into a single pre-allocated buffer:
        // growing a `String` reallocates and frees the old buffer without
        // zeroing it, which would leave a partial marker in freed heap. UUID
        // temporaries are wrapped in `Zeroizing` for the same reason.
        let bunker_uuid = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let mut bunker_uri_marker = String::with_capacity("pacto-test-bunker-".len() + 32);
        bunker_uri_marker.push_str("pacto-test-bunker-");
        bunker_uri_marker.push_str(&bunker_uuid);

        let token_uuid = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let mut http_token_marker = String::with_capacity("pacto-test-token-".len() + 32);
        http_token_marker.push_str("pacto-test-token-");
        http_token_marker.push_str(&token_uuid);

        let key_uuid1 = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let key_uuid2 = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let mut attachment_key_marker = String::with_capacity(64);
        attachment_key_marker.push_str(&key_uuid1);
        attachment_key_marker.push_str(&key_uuid2);

        let nonce_uuid = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let mut attachment_nonce_marker = String::with_capacity(32);
        attachment_nonce_marker.push_str(&nonce_uuid);

        let plaintext_uuid = Zeroizing::new(Uuid::new_v4().as_simple().to_string());
        let mut attachment_plaintext_marker =
            String::with_capacity("pacto-attachment-plaintext-".len() + 32);
        attachment_plaintext_marker.push_str("pacto-attachment-plaintext-");
        attachment_plaintext_marker.push_str(&plaintext_uuid);

        Self {
            nsec_marker,
            nsec_marker_bytes,
            bunker_uri_marker,
            http_token_marker,
            attachment_key_marker,
            attachment_nonce_marker,
            attachment_plaintext_marker,
            _scan_guard: scan_guard,
        }
    }
}

// SAFETY/hygiene: `cargo test`'s default harness runs every test in this
// binary in one process (unlike `cargo nextest`, which isolates each test in
// its own process). `simulated_core_dump_after_nsec_load_does_not_leak_marker`
// scans the *entire* process's writable memory, so a marker from a fixture
// that belonged to a different, already-finished test can still be sitting
// in freed-but-unzeroed heap and get flagged as a false-positive leak from
// *this* test. Zeroize every marker on drop so no fixture outlives its test.
impl Drop for SensitiveFixture {
    fn drop(&mut self) {
        self.nsec_marker.zeroize();
        self.nsec_marker_bytes.zeroize();
        self.bunker_uri_marker.zeroize();
        self.http_token_marker.zeroize();
        self.attachment_key_marker.zeroize();
        self.attachment_nonce_marker.zeroize();
        self.attachment_plaintext_marker.zeroize();
    }
}

impl Default for SensitiveFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Panic if any synthetic secret marker appears in `haystack`.
///
/// The panic message lists every marker that leaked so failures are actionable.
pub fn assert_no_leak(haystack: impl AsRef<str>, fixture: &SensitiveFixture) {
    let hay = haystack.as_ref().as_bytes();
    let leaked: Vec<&'static str> = fixture
        .needles()
        .into_iter()
        .filter(|(_, needle)| contains_subsequence(hay, needle))
        .map(|(label, _)| label)
        .collect();
    assert!(
        leaked.is_empty(),
        "secret markers leaked in haystack: {leaked:?}"
    );
}

fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Run `f` with a temporary tracing subscriber and return its result plus the
/// captured log output.
pub fn capture_logs_during<R>(f: impl FnOnce() -> R) -> (R, String) {
    let writer = TestWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    let guard = tracing::subscriber::set_default(subscriber);
    let result = f();
    drop(guard);

    let bytes = writer.0.lock().clone();
    let logs = String::from_utf8_lossy(&bytes).to_string();
    (result, logs)
}

#[derive(Clone, Default)]
struct TestWriter(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for TestWriter {
    type Writer = TestWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `strings(1)` on `binary_path` and return its output, or `None` if the
/// tool is unavailable.
pub fn strings_output(binary_path: &Path) -> Option<String> {
    let output = Command::new("strings").arg(binary_path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Write `content` to a temporary config file with owner-only permissions.
/// Also tightens the parent directory to `0o700` so config validation passes.
pub fn write_config_file(dir: &Path, content: &str) -> std::io::Result<PathBuf> {
    let path = dir.join("pacto-bot-api.toml");
    let mut file = fs::File::create(&path)?;
    file.write_all(content.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;

        let mut dir_perms = fs::metadata(dir)?.permissions();
        dir_perms.set_mode(0o700);
        fs::set_permissions(dir, dir_perms)?;
    }

    Ok(path)
}

impl SensitiveFixture {
    /// Every synthetic marker paired with the label reported when it leaks.
    fn needles(&self) -> [(&'static str, &[u8]); 7] {
        [
            ("nsec", self.nsec_marker.as_bytes()),
            ("nsec_bytes", &self.nsec_marker_bytes),
            ("bunker_uri", self.bunker_uri_marker.as_bytes()),
            ("http_token", self.http_token_marker.as_bytes()),
            ("attachment_key", self.attachment_key_marker.as_bytes()),
            ("attachment_nonce", self.attachment_nonce_marker.as_bytes()),
            (
                "attachment_plaintext",
                self.attachment_plaintext_marker.as_bytes(),
            ),
        ]
    }

    /// Address ranges of the live markers. These are the fixture's own
    /// reference copies, not residue, so they are masked out of the scan.
    fn marker_ranges(&self) -> [(usize, usize); 7] {
        self.needles()
            .map(|(_, bytes)| (bytes.as_ptr() as usize, bytes.len()))
    }

    /// Simulate a core-dump scan of the current process and return the label
    /// of every marker still reachable in writable memory.
    ///
    /// Returns `None` on platforms where the scan is not implemented, so the
    /// caller can skip the test.
    pub fn scan_memory_for_leaks(&self) -> Option<Vec<&'static str>> {
        scan_writable_memory(&self.marker_ranges(), &self.needles())
    }
}

/// Bytes pulled from `/proc/self/mem` per `pread(2)`.
#[cfg(target_os = "linux")]
const SCAN_CHUNK: usize = 1 << 20;

/// Scan every writable region of the current process for `needles`, returning
/// the labels that were found outside `exclusions`.
///
/// The scanner must not perturb what it observes. A scratch buffer obtained
/// from the allocator is itself writable process memory, and `pread(2)` on
/// `/proc/self/mem` will happily copy a region into a buffer that lives inside
/// that very region: the copy duplicates every byte of the region -- including
/// the fixture's own live markers -- at addresses the caller's exclusion list
/// does not cover, and reads back as a leak. (Worse, the copy self-overlaps,
/// so the kernel re-copies bytes it has already written and the duplicate
/// appears more than once.) So the scratch buffer is allocated once, never
/// resized, and its address range is masked out of every chunk alongside the
/// caller's exclusions.
#[cfg(target_os = "linux")]
fn scan_writable_memory(
    exclusions: &[(usize, usize)],
    needles: &[(&'static str, &[u8])],
) -> Option<Vec<&'static str>> {
    use std::os::unix::fs::FileExt;

    // Carried between chunks so a marker straddling a chunk boundary is found.
    let overlap = needles
        .iter()
        .map(|(_, needle)| needle.len())
        .max()
        .unwrap_or(0)
        .saturating_sub(1);

    // Allocated before the maps snapshot so its mapping is visible to the
    // scan, and never resized: a reallocation would move the masked range
    // mid-scan and leave an unscrubbed copy of the last chunk in freed heap.
    let mut scratch = vec![0u8; overlap + SCAN_CHUNK];
    let scratch_range = [(scratch.as_ptr() as usize, scratch.len())];

    let maps = fs::read_to_string("/proc/self/maps").ok()?;
    let mem = fs::File::open("/proc/self/mem").ok()?;

    let mut leaked: Vec<&'static str> = Vec::new();
    for (start, end) in writable_regions(&maps) {
        let len = end - start;
        let mut offset = 0;
        // Reset per region: distinct mappings never share one allocation.
        let mut carried = 0;
        while offset < len {
            let chunk_len = SCAN_CHUNK.min(len - offset);
            let chunk_addr = start + offset;
            let Some(chunk) = scratch.get_mut(overlap..overlap + chunk_len) else {
                break;
            };
            if mem.read_exact_at(chunk, chunk_addr as u64).is_err() {
                break;
            }
            mask(chunk, chunk_addr, exclusions);
            mask(chunk, chunk_addr, &scratch_range);

            let window = &scratch[overlap - carried..overlap + chunk_len];
            for &(label, needle) in needles {
                if !leaked.contains(&label) && contains_subsequence(window, needle) {
                    leaked.push(label);
                }
            }

            carried = overlap.min(chunk_len);
            scratch.copy_within(
                overlap + chunk_len - carried..overlap + chunk_len,
                overlap - carried,
            );
            offset += chunk_len;
        }
    }

    Some(leaked)
}

/// Zero every byte of `buf` (a snapshot of the memory at `buf_addr`) that
/// falls inside one of `exclusions`. Overlaps are clamped, so an exclusion
/// that straddles a chunk boundary is masked in both chunks.
#[cfg(target_os = "linux")]
fn mask(buf: &mut [u8], buf_addr: usize, exclusions: &[(usize, usize)]) {
    let buf_end = buf_addr + buf.len();
    for &(addr, len) in exclusions {
        let lo = addr.max(buf_addr);
        let hi = (addr + len).min(buf_end);
        if lo < hi {
            buf[lo - buf_addr..hi - buf_addr].fill(0);
        }
    }
}

/// Parse `/proc/self/maps` into the `(start, end)` bounds of every readable,
/// writable mapping.
#[cfg(target_os = "linux")]
fn writable_regions(maps: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    maps.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let perms = fields.next()?;
        if !perms.starts_with('r') || !perms.contains('w') {
            return None;
        }
        let (start, end) = range.split_once('-')?;
        let start = usize::from_str_radix(start, 16).ok()?;
        let end = usize::from_str_radix(end, 16).ok()?;
        (end > start).then_some((start, end))
    })
}

#[cfg(not(target_os = "linux"))]
fn scan_writable_memory(
    _exclusions: &[(usize, usize)],
    _needles: &[(&'static str, &[u8])],
) -> Option<Vec<&'static str>> {
    None
}
