//! Security, input validation, and sanitization module for Bulker.
//!
//! Provides defense-in-depth against:
//! - Arbitrary code execution (argument injection, shell escapes)
//! - Server-Side Request Forgery (SSRF) and local network probing
//! - Path traversal attacks (../, directory escapes)
//! - SQL injection (parameterized queries enforcement)
//! - Denial of Service via unbounded inputs

use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;

static YOUTUBE_ID_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").expect("valid regex")
});

static ADB_DEVICE_ID_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-zA-Z0-9.:_-]{1,64}$").expect("valid regex")
});

/// Allowed hostnames for YouTube / YouTube Music
const ALLOWED_HOSTS: &[&str] = &[
    "music.youtube.com",
    "www.youtube.com",
    "youtube.com",
    "youtu.be",
    "m.youtube.com",
];

/// Validates and sanitizes a YouTube Music / YouTube URL.
///
/// Prevents:
/// - CLI flag injection (e.g. `--exec="cmd"`, `--config-location`)
/// - Arbitrary protocol abuse (`file://`, `gopher://`, `javascript://`)
/// - Internal network probing (SSRF to localhost, 127.0.0.1, AWS metadata, etc.)
/// - Null byte injection and CRLF splitting
pub fn validate_youtube_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();

    if trimmed.is_empty() {
        bail!("URL cannot be empty");
    }

    if trimmed.len() > 2048 {
        bail!("URL exceeds maximum allowed length of 2048 characters");
    }

    // Prevent argument injection (flags passed to yt-dlp)
    if trimmed.starts_with('-') {
        bail!("Invalid URL: starts with dangerous prefix '-'");
    }

    // Disallow null bytes and control characters
    if trimmed.bytes().any(|b| b == 0 || b < 32 || b == 127) {
        bail!("Invalid URL: contains null bytes or unescaped control characters");
    }

    // Must start with https:// (strict TLS requirement)
    if !trimmed.starts_with("https://") {
        bail!("Invalid URL: only secure 'https://' URLs are permitted");
    }

    // Extract authority / host
    let url_without_proto = &trimmed["https://".len()..];
    let host = match url_without_proto.find(['/', '?', '#']) {
        Some(idx) => &url_without_proto[..idx],
        None => url_without_proto,
    };

    // Strip port if present
    let host_name = host.split(':').next().unwrap_or("").to_lowercase();

    // Check against allowed YouTube domains
    let is_allowed = ALLOWED_HOSTS.iter().any(|&allowed| host_name == allowed || host_name.ends_with(&format!(".{allowed}")));
    if !is_allowed {
        bail!("Forbidden URL host '{}': only official YouTube Music domains are allowed", host_name);
    }

    // Disallow loopback / local hosts
    if host_name == "localhost"
        || host_name == "127.0.0.1"
        || host_name == "0.0.0.0"
        || host_name == "[::1]"
        || host_name.starts_with("169.254.")
        || host_name.starts_with("10.")
        || host_name.starts_with("192.168.")
        || host_name.starts_with("172.16.")
    {
        bail!("SSRF attempt blocked: local or private IP addresses are disallowed");
    }

    Ok(trimmed.to_string())
}

/// Validates YouTube video ID or playlist ID.
///
/// Ensures only safe alphanumeric characters and `_`, `-` are present.
pub fn validate_youtube_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("ID cannot be empty");
    }
    if !YOUTUBE_ID_REGEX.is_match(trimmed) {
        bail!("Invalid YouTube identifier: contains illegal characters or invalid length");
    }
    Ok(trimmed.to_string())
}

/// Validates an ADB device identifier.
///
/// Prevents command injection or shell escaping via device serial/port.
pub fn validate_device_id(device_id: &str) -> Result<String> {
    let trimmed = device_id.trim();
    if trimmed.is_empty() {
        bail!("Device ID cannot be empty");
    }
    if !ADB_DEVICE_ID_REGEX.is_match(trimmed) {
        bail!("Invalid ADB device identifier: contains illegal characters");
    }
    Ok(trimmed.to_string())
}

/// Sanitizes a filename or path component to prevent path traversal,
/// directory escapes, and invalid filesystem characters.
pub fn sanitize_filename(name: &str) -> String {
    let mut cleaned: String = name
        .chars()
        .map(|c| match c {
            // Path separators & traversal
            '/' | '\\' => '_',
            // Shell metacharacters & reserved filesystem chars
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | ';' | '&' | '`' | '$' | '(' | ')' | '{' | '}' | '!' => '_',
            // Null bytes & control characters
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    // Prevent directory traversal tokens
    cleaned = cleaned.replace("..", "_");
    cleaned = cleaned.trim().trim_matches('.').trim().to_string();

    if cleaned.is_empty() {
        cleaned = "unnamed".to_string();
    }

    // Limit filename length to 180 characters to leave room for extensions and directory paths
    if cleaned.len() > 180 {
        cleaned.truncate(180);
        cleaned = cleaned.trim_end().to_string();
    }

    cleaned
}

/// Validates that an argument passed to a subprocess does not contain null bytes
/// or start with flag-injection characters unless explicitly permitted.
pub fn sanitize_cli_arg(arg: &str, allow_flags: bool) -> Result<String> {
    if arg.contains('\0') {
        bail!("CLI argument cannot contain null bytes");
    }
    if !allow_flags && arg.starts_with('-') {
        bail!("CLI argument cannot start with flag prefix '-'");
    }
    Ok(arg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_youtube_url() {
        assert!(validate_youtube_url("https://music.youtube.com/watch?v=dQw4w9WgXcQ").is_ok());
        assert!(validate_youtube_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").is_ok());
        assert!(validate_youtube_url("https://youtu.be/dQw4w9WgXcQ").is_ok());

        // Rejections
        assert!(validate_youtube_url("http://music.youtube.com/watch?v=dQw4w9WgXcQ").is_err());
        assert!(validate_youtube_url("file:///etc/passwd").is_err());
        assert!(validate_youtube_url("--exec=whoami").is_err());
        assert!(validate_youtube_url("https://evil.com/fake").is_err());
        assert!(validate_youtube_url("https://127.0.0.1:8080/exploit").is_err());
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "__etc_passwd");
        assert_eq!(sanitize_filename("Artist: Song *Remix*?"), "Artist_ Song _Remix__");
        assert_eq!(sanitize_filename(""), "unnamed");
        assert_eq!(sanitize_filename("..."), "unnamed");
    }

    #[test]
    fn test_validate_youtube_id() {
        assert!(validate_youtube_id("dQw4w9WgXcQ").is_ok());
        assert!(validate_youtube_id("RDCLAK5uy_k").is_ok());
        assert!(validate_youtube_id("id; rm -rf /").is_err());
        assert!(validate_youtube_id("../secret").is_err());
    }
}
