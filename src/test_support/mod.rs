/// Test support utilities available to library unit tests.
#[cfg(test)]
pub mod mock_bunker;
#[cfg(test)]
pub mod mock_relay;

/// Create a temp directory under `target/test-temp` instead of `/tmp`.
///
/// Daemon config validation rejects MLS database paths under `/tmp` and
/// `/dev/shm`, so unit tests that exercise MLS config must use this helper.
#[cfg(test)]
pub fn test_tempdir() -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir should be available")
        .join("target")
        .join("test-temp");
    std::fs::create_dir_all(&base).expect("test temp base should be creatable");
    tempfile::Builder::new()
        .tempdir_in(base)
        .expect("temp dir should be creatable")
}
