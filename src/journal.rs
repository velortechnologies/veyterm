use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// One append-only journal for this client invocation, across reconnects/session switches.
pub struct Journal {
    file: RefCell<File>,
    error: RefCell<Option<String>>,
}

impl Journal {
    pub fn open(path: &Path, host: &str, session: Option<&str>) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let journal = Self {
            file: RefCell::new(options.open(path)?),
            error: RefCell::new(None),
        };
        let server = url::Url::parse(host)
            .ok()
            .and_then(|url| {
                Some(format!(
                    "{}:{}",
                    url.host_str()?,
                    url.port_or_known_default()?
                ))
            })
            .unwrap_or_else(|| host.into());
        journal.write(
            "session",
            json!({"host": host, "server":server, "name": session}),
        )?;
        Ok(journal)
    }

    fn write(&self, kind: &str, data: Value) -> io::Result<()> {
        self.write_entry(&entry(kind, data))
    }

    fn write_entry(&self, record: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&without_binary(record))?;
        line.push(b'\n');
        let mut file = self.file.borrow_mut();
        file.write_all(&line)?;
        file.flush()
    }

    pub fn record(&self, kind: &str, data: Value) {
        self.record_entry(&entry(kind, data));
    }

    pub fn record_entry(&self, record: &Value) {
        if let Err(error) = self.write_entry(record) {
            *self.error.borrow_mut() = Some(format!("Log write failed: {error}"));
        }
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.borrow_mut().take()
    }
}

/// Keep text, tool arguments and attachment metadata, never embedded file bytes.
fn without_binary(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let attachment = object.contains_key("mimeType")
                || object.contains_key("mime_type")
                || object.contains_key("media_type")
                || object.contains_key("fileName")
                || matches!(
                    object.get("type").and_then(Value::as_str),
                    Some(
                        "image"
                            | "image_url"
                            | "audio"
                            | "input_audio"
                            | "file"
                            | "video"
                            | "base64"
                    )
                );
            Value::Object(
                object
                    .iter()
                    .map(|(key, value)| {
                        let binary_field = matches!(key.as_str(), "base64" | "b64_json")
                            || (attachment && matches!(key.as_str(), "content" | "data" | "bytes"));
                        (
                            key.clone(),
                            if binary_field {
                                json!("[binary omitted]")
                            } else {
                                without_binary(value)
                            },
                        )
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(without_binary).collect()),
        Value::String(text) if text.starts_with("data:") => json!("[binary omitted]"),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_attachment_bytes_but_keeps_text_and_tool_data() {
        let input = json!({
            "attachments": [{"fileName":"x.png", "mimeType":"image/png", "content":"SECRET"}],
            "source":{"type":"base64","media_type":"image/png","data":"SECRET"},
            "content":[{"type":"text","text":"hello"}, {"type":"image","source":{"data":"data:image/png;base64,SECRET"}}],
            "tool":{"data":{"content":"ordinary text"}},
        });
        let result = without_binary(&input);
        assert!(!result.to_string().contains("SECRET"));
        assert_eq!(result["tool"]["data"]["content"], "ordinary text");
        assert_eq!(result["content"][0]["text"], "hello");
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn journal_appends_valid_ndjson_with_metadata_first() {
        let path = std::env::temp_dir().join(format!("veyterm-log-{}.ndjson", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let log = Journal::open(&path, "wss://host", Some("session")).unwrap();
            log.record("input", json!({"text":"hello\nworld"}));
            log.record("connection_error", json!({"error":"offline"}));
            assert!(log.take_error().is_none());
        }
        let log = Journal::open(&path, "wss://host", None).unwrap();
        log.record("connection", json!({"status":"CONNECTED"}));
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 5);
        assert_eq!(records[0]["data"]["name"], "session");
        assert_eq!(records[0]["data"]["host"], "wss://host");
        assert_eq!(records[1]["data"]["text"], "hello\nworld");
        std::fs::remove_file(path).unwrap();
    }
}

pub fn entry(kind: &str, data: Value) -> Value {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    json!({"timestamp":timestamp,"type":kind,"data":data})
}

pub fn save(path: &Path, records: &[Value]) -> io::Result<()> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &without_binary(record))?;
        bytes.push(b'\n');
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&bytes)
}
