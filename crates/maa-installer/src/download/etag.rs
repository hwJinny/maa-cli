//! Manifest cache implementation using ETag for conditional requests.
//!
//! This module provides caching functionality to avoid re-downloading manifests
//! when they haven't changed, using HTTP ETag headers.
//!
//! Note: The cache does not use file locking for simplicity and performance.
//! In rare concurrent write scenarios, some ETag updates may be lost, which is
//! acceptable as the cache will be refreshed on the next check.

use std::{
    fs,
    path::{Path, PathBuf},
    time,
};

use ureq::http::StatusCode;

use crate::error::{Error, ErrorKind, Result, WithDesc};

fn etag_path(dest: &Path) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(".etag");
    path.into()
}

pub fn download_with_etag(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    check_interval: Option<time::Duration>,
) -> Result<()> {
    let etag_file = etag_path(dest);

    let etag = if dest.exists() && etag_file.exists() {
        let modified = etag_file.metadata().ok().and_then(|m| m.modified().ok());
        if let Some(check_interval) = check_interval
            && let Some(modified) = modified
            && let Ok(duration) = time::SystemTime::now().duration_since(modified)
            && duration < check_interval
        {
            log::trace!("File {} is fresh", dest.display());
            return Ok(());
        }

        fs::read_to_string(&etag_file).ok()
    } else {
        None
    };

    let mut request = agent.get(url);
    if let Some(etag) = etag {
        request = request.header("If-None-Match", &etag);
    }
    let response = request.call()?;

    match response.status() {
        StatusCode::OK => {
            log::trace!("Downloaded file {}", dest.display());
            let etag = response.headers().get("ETag").and_then(|v| v.to_str().ok());
            if let Some(etag) = etag {
                log::trace!("Updated ETag {}", etag_file.display());
                fs::write(&etag_file, etag).then_with_desc(|| {
                    format!("Failed to update ETag at {}", etag_file.display())
                })?;
            }
            let mut file = fs::File::create(dest)?;
            std::io::copy(&mut response.into_body().as_reader(), &mut file)?;

            Ok(())
        }
        StatusCode::NOT_MODIFIED => {
            log::trace!("File {} is up to date", dest.display());
            if let Ok(file) = fs::File::open(&etag_file) {
                log::trace!("Touched {}", dest.display());
                let _ = file.set_modified(time::SystemTime::now());
            }
            Ok(())
        }
        s => Err(Error::new(ErrorKind::Network).with_desc(format!("unexpected status code {s}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::etag_path;

    #[test]
    fn etag_path_appends_to_name_without_extension() {
        assert_eq!(
            etag_path(PathBuf::from("manifest").as_path()),
            PathBuf::from("manifest.etag")
        );
    }

    #[test]
    fn etag_path_preserves_existing_extension() {
        assert_eq!(
            etag_path(PathBuf::from("manifest.json").as_path()),
            PathBuf::from("manifest.json.etag")
        );
    }

    #[test]
    fn etag_path_preserves_unicode() {
        assert_eq!(
            etag_path(PathBuf::from("명일방주.json").as_path()),
            PathBuf::from("명일방주.json.etag")
        );
    }

    #[cfg(unix)]
    #[test]
    fn etag_path_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(OsString::from_vec(vec![b'm', 0x80]));
        let result = etag_path(&path);

        assert_eq!(result.as_os_str().as_bytes(), b"m\x80.etag");
    }

    #[cfg(windows)]
    #[test]
    fn etag_path_preserves_unpaired_utf16() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(OsString::from_wide(&[b'm' as u16, 0xd800]));
        let result: Vec<_> = etag_path(&path).as_os_str().encode_wide().collect();

        assert_eq!(
            result,
            [b'm' as u16, 0xd800]
                .into_iter()
                .chain(".etag".encode_utf16())
                .collect::<Vec<_>>()
        );
    }
}
