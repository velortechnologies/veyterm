use base64::Engine;
use serde_json::{Value, json};
use std::{io, path::PathBuf};
pub fn expand(s: &str) -> PathBuf {
    if s == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    } else if let Some(s) = s.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(s)
    } else {
        PathBuf::from(s)
    }
}
/// Split at the first unescaped space, decoding escaped spaces/backslashes in the path.
pub fn split_attachment(raw: &str) -> (String, &str) {
    let mut path = String::new();
    let mut chars = raw.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == ' ' {
            return (path, &raw[i + 1..]);
        }
        if c == '\\'
            && let Some(&(_, next)) = chars.peek()
            && (next == ' ' || next == '\\')
        {
            path.push(next);
            chars.next();
            continue;
        }
        path.push(c);
    }
    (path, "")
}
fn escape_path(path: &str) -> String {
    path.replace('\\', "\\\\").replace(' ', "\\ ")
}
pub fn complete(raw: &str) -> Vec<String> {
    let (decoded, _) = split_attachment(raw);
    let raw = decoded.as_str();
    if raw == "~" {
        return vec!["/attach ~/".into()];
    }
    let (dir, stem) = raw
        .rsplit_once('/')
        .map_or(("", raw), |(d, s)| (&raw[..d.len() + 1], s));
    let path = if dir.is_empty() {
        PathBuf::from(".")
    } else {
        expand(dir)
    };
    let Ok(entries) = std::fs::read_dir(path) else {
        return vec![];
    };
    let mut result: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            if !name.starts_with(stem) {
                return None;
            }
            let tail = if e.path().is_dir() { "/" } else { "" };
            Some(format!(
                "/attach {}",
                escape_path(&format!("{dir}{name}{tail}"))
            ))
        })
        .collect();
    result.sort();
    result
}
pub fn load(raw: &str) -> io::Result<Value> {
    let p = expand(raw);
    let meta = std::fs::metadata(&p)?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let data = std::fs::read(&p)?;
    let mime = mime_guess::from_path(&p)
        .first_or_octet_stream()
        .to_string();
    Ok(
        json!({"fileName":p.file_name().and_then(|v|v.to_str()).unwrap_or("file"),"mimeType":mime,"content":base64::engine::general_purpose::STANDARD.encode(&data),"sizeBytes":data.len(),"origin":"file"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn home_completion() {
        assert_eq!(complete("~"), vec!["/attach ~/"]);
        assert_eq!(expand("~/x"), expand("~").join("x"));
    }
    #[test]
    fn payload_roundtrip() {
        let p = std::env::temp_dir().join(format!(
            "veyterm-attachment-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&p, "Привіт").unwrap();
        let v = load(p.to_str().unwrap()).unwrap();
        assert_eq!(v["mimeType"], "text/plain");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(v["content"].as_str().unwrap())
                .unwrap(),
            "Привіт".as_bytes()
        );
        std::fs::remove_file(p).unwrap();
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;
    #[test]
    fn escaped_path_and_caption() {
        assert_eq!(
            split_attachment(r"~/my\ file.png hello world"),
            ("~/my file.png".into(), "hello world")
        );
        assert_eq!(
            split_attachment(r"~/my\ file.png"),
            ("~/my file.png".into(), "")
        );
        assert_eq!(split_attachment("file.png"), ("file.png".into(), ""));
        let path = r"dir\name with spaces.png";
        assert_eq!(split_attachment(&escape_path(path)).0, path);
    }
}
