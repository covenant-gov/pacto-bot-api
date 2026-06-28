use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use parking_lot::Mutex;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

/// A set of unique synthetic secret markers used to detect leaks.
///
/// Each marker is generated once per fixture so that tests do not accidentally
/// match real config values or example strings.
pub struct SensitiveFixture {
    /// Synthetic `nsec` value. Kept as a 64-character hex string so it is valid
    /// for `LocalKey::parse` while still being easy to search for.
    pub nsec_marker: String,
    /// Synthetic bunker URI substring.
    pub bunker_uri_marker: String,
    /// Synthetic HTTP secret token.
    pub http_token_marker: String,
}

impl SensitiveFixture {
    /// Create a new fixture with fresh markers.
    pub fn new() -> Self {
        let first = Uuid::new_v4().as_simple().to_string();
        let second = Uuid::new_v4().as_simple().to_string();
        Self {
            nsec_marker: format!("{first}{second}"),
            bunker_uri_marker: format!("pacto-test-bunker-{}", Uuid::new_v4()),
            http_token_marker: format!("pacto-test-token-{}", Uuid::new_v4()),
        }
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
    let hay = haystack.as_ref();
    let mut leaked = Vec::new();
    if hay.contains(&fixture.nsec_marker) {
        leaked.push("nsec");
    }
    if hay.contains(&fixture.bunker_uri_marker) {
        leaked.push("bunker_uri");
    }
    if hay.contains(&fixture.http_token_marker) {
        leaked.push("http_token");
    }
    assert!(
        leaked.is_empty(),
        "secret markers leaked in haystack: {leaked:?}"
    );
}

/// Panic if any synthetic secret marker appears in `haystack`.
pub fn assert_no_leak_bytes(haystack: &[u8], fixture: &SensitiveFixture) {
    let mut leaked = Vec::new();
    if contains_subsequence(haystack, fixture.nsec_marker.as_bytes()) {
        leaked.push("nsec");
    }
    if contains_subsequence(haystack, fixture.bunker_uri_marker.as_bytes()) {
        leaked.push("bunker_uri");
    }
    if contains_subsequence(haystack, fixture.http_token_marker.as_bytes()) {
        leaked.push("http_token");
    }
    assert!(
        leaked.is_empty(),
        "secret markers leaked in binary haystack: {leaked:?}"
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
    }

    Ok(path)
}

impl SensitiveFixture {
    /// Simulate a core-dump memory scan of the current process.
    ///
    /// On Linux this reads the writable regions of `/proc/self/mem`, scrubs the
    /// exact heap locations of the live fixture markers, and returns the rest of
    /// the dump. On unsupported platforms it returns `None` so the caller can
    /// skip the test.
    pub fn scan_memory(&self) -> Option<Vec<u8>> {
        let exclusions = [
            (self.nsec_marker.as_ptr() as usize, self.nsec_marker.len()),
            (
                self.bunker_uri_marker.as_ptr() as usize,
                self.bunker_uri_marker.len(),
            ),
            (
                self.http_token_marker.as_ptr() as usize,
                self.http_token_marker.len(),
            ),
        ];
        read_proc_mem_writable(&exclusions)
    }
}

#[cfg(target_os = "linux")]
fn read_proc_mem_writable(exclusions: &[(usize, usize)]) -> Option<Vec<u8>> {
    use std::os::unix::fs::FileExt;

    let maps = fs::read_to_string("/proc/self/maps").ok()?;
    let mem = fs::File::open("/proc/self/mem").ok()?;
    let mut dump = Vec::new();

    for line in maps.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let perms = parts[1];
        if !perms.starts_with('r') || !perms.contains('w') {
            continue;
        }
        let mut range = parts[0].split('-');
        let start = usize::from_str_radix(range.next()?, 16).ok()?;
        let end = usize::from_str_radix(range.next()?, 16).ok()?;
        let len = end.saturating_sub(start);
        if len == 0 {
            continue;
        }

        let mut buf = vec![0u8; len];
        if mem.read_at(&mut buf, start as u64).is_err() {
            continue;
        }

        for &(addr, exc_len) in exclusions {
            if addr >= start && addr + exc_len <= end {
                let offset = addr - start;
                for byte in &mut buf[offset..offset + exc_len] {
                    *byte = 0;
                }
            }
        }

        dump.extend_from_slice(&buf);
    }

    Some(dump)
}

#[cfg(not(target_os = "linux"))]
fn read_proc_mem_writable(_exclusions: &[(usize, usize)]) -> Option<Vec<u8>> {
    None
}
