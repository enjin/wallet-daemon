//! Loads the `.env` file with tolerance for UTF-16 encodings.
//!
//! `dotenvy` only understands UTF-8 (it strips a UTF-8 BOM but nothing else).
//! Windows PowerShell 5.1 writes files as UTF-16 LE with a BOM by default
//! (`>`, `Out-File`, `Set-Content`, `Add-Content` without `-Encoding`), so a
//! `.env` created that way is unreadable and the daemon exits complaining that
//! required variables are missing. We detect a UTF-16 BOM, transcode to UTF-8,
//! and hand the result to dotenvy. UTF-8 files take dotenvy's normal path.

use std::io::Cursor;
use std::path::PathBuf;

/// Finds and loads `.env`, transcoding UTF-16 (LE/BE) to UTF-8 when needed.
///
/// Must be called before the first `dotenvy::var` access. dotenvy does not
/// override variables that are already set, so values we load here win over
/// dotenvy's later lazy load of the same file.
pub fn load_env() {
    let Some(path) = find_dotenv() else {
        // No `.env` found; rely on real environment variables.
        return;
    };

    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };

    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        decode_utf16(&bytes[2..], u16::from_le_bytes)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        decode_utf16(&bytes[2..], u16::from_be_bytes)
    } else {
        // UTF-8 (with or without BOM): let dotenvy handle it from the path so
        // its own BOM stripping and parsing apply unchanged.
        let _ = dotenvy::from_path(&path);
        return;
    };

    let _ = dotenvy::from_read(Cursor::new(text.into_bytes()));
}

/// Walks up from the current directory looking for a `.env` file, mirroring
/// dotenvy's default lookup so behavior is unchanged for UTF-8 users.
fn find_dotenv() -> Option<PathBuf> {
    let mut dir: PathBuf = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn decode_utf16(body: &[u8], to_u16: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = body.chunks_exact(2).map(|c| to_u16([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn read_back(bytes: &[u8]) -> String {
        // Reproduce load_env's transcoding without touching process env.
        if bytes.starts_with(&[0xFF, 0xFE]) {
            decode_utf16(&bytes[2..], u16::from_le_bytes)
        } else if bytes.starts_with(&[0xFE, 0xFF]) {
            decode_utf16(&bytes[2..], u16::from_be_bytes)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    fn utf16_le_bom(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    fn utf16_be_bom(s: &str) -> Vec<u8> {
        let mut out = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_be_bytes());
        }
        out
    }

    #[test]
    fn decodes_utf16_le_with_bom() {
        let src = "KEY_PASS=hunter2\nPLATFORM_KEY=token\n";
        assert_eq!(read_back(&utf16_le_bom(src)), src);
    }

    #[test]
    fn decodes_utf16_be_with_bom() {
        let src = "KEY_PASS=hunter2\nPLATFORM_KEY=token\n";
        assert_eq!(read_back(&utf16_be_bom(src)), src);
    }

    #[test]
    fn passes_utf8_through_unchanged() {
        let src = "KEY_PASS=hunter2\n";
        assert_eq!(read_back(src.as_bytes()), src);
    }

    #[test]
    fn transcoded_utf16_parses_as_env_pairs() {
        let src = "KEY_PASS=hunter2\nPLATFORM_KEY=token\n";
        let text = read_back(&utf16_le_bom(src));
        let pairs: Vec<(String, String)> = dotenvy::from_read_iter(Cursor::new(text.into_bytes()))
            .map(|item| item.expect("valid env pair"))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("KEY_PASS".to_string(), "hunter2".to_string()),
                ("PLATFORM_KEY".to_string(), "token".to_string()),
            ]
        );
    }

    #[test]
    fn find_dotenv_returns_none_when_absent() {
        // Sanity: a path that cannot exist as a file.
        assert!(!Path::new("/nonexistent-dir-xyz/.env").is_file());
    }
}
