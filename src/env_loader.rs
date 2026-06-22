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
    find_dotenv_from(std::env::current_dir().ok()?)
}

/// Walks up from `start` looking for a `.env` file. Pure — touches no process
/// state — so it can be tested directly without `set_current_dir`.
fn find_dotenv_from(start: PathBuf) -> Option<PathBuf> {
    let mut dir = start;
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
    let mut units: Vec<u16> = body.chunks_exact(2).map(|c| to_u16([c[0], c[1]])).collect();
    // An odd trailing byte means the file is truncated or not actually UTF-16.
    // Preserve it as U+FFFD rather than silently dropping data.
    if !body.len().is_multiple_of(2) {
        units.push(0xFFFD);
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn decodes_odd_length_utf16_with_replacement() {
        // "AB" as UTF-16 LE plus a dangling odd byte after the BOM.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("AB".encode_utf16().flat_map(u16::to_le_bytes));
        bytes.push(0x00); // truncated final code unit
        let out = read_back(&bytes);
        assert!(out.starts_with("AB"), "decoded prefix lost: {out:?}");
        assert!(
            out.ends_with('\u{FFFD}'),
            "dangling byte should become U+FFFD, got {out:?}"
        );
    }

    #[test]
    fn find_dotenv_walks_up_to_ancestor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".env"), b"KEY_PASS=x\n").unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_dotenv_from(nested), Some(root.path().join(".env")));
    }

    #[test]
    fn find_dotenv_prefers_nearest() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".env"), b"a=1\n").unwrap();
        let nearer = root.path().join("a");
        std::fs::create_dir_all(nearer.join("b")).unwrap();
        std::fs::write(nearer.join(".env"), b"a=2\n").unwrap();

        assert_eq!(
            find_dotenv_from(root.path().join("a/b")),
            Some(nearer.join(".env"))
        );
    }
}
