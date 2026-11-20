use serde_json::Value;
#[derive(Clone)]
pub struct File {
    pub url: String,
    pub name: String,
}
pub fn collect(v: &Value, out: &mut Vec<File>) {
    match v {
        Value::Object(m) => {
            let media =
                m.get("type").and_then(Value::as_str).is_some_and(|t| {
                    matches!(t, "image" | "audio" | "video" | "file" | "attachment")
                }) || m.contains_key("mimeType");
            if media && let Some(url) = m.get("url").and_then(Value::as_str) {
                let name = m
                    .get("fileName")
                    .or(m.get("label"))
                    .or(m.get("alt"))
                    .and_then(Value::as_str)
                    .unwrap_or("incoming-file");
                out.push(File {
                    url: url.into(),
                    name: name.into(),
                });
            }
            for (k, v) in m {
                if k == "mediaUrl" {
                    if let Some(url) = v.as_str() {
                        out.push(File {
                            url: url.into(),
                            name: "incoming-file".into(),
                        });
                    }
                } else if k == "mediaUrls" {
                    if let Some(a) = v.as_array() {
                        for u in a {
                            if let Some(url) = u.as_str() {
                                out.push(File {
                                    url: url.into(),
                                    name: "incoming-file".into(),
                                });
                            }
                        }
                    }
                } else {
                    collect(v, out);
                }
            }
        }
        Value::Array(a) => {
            for v in a {
                collect(v, out)
            }
        }
        _ => (),
    }
}
pub fn save(
    file: &File,
    endpoint: &str,
    insecure: bool,
    pin: Option<&str>,
) -> Result<String, String> {
    use std::io::Write;
    let mut base = url::Url::parse(endpoint).map_err(|e| e.to_string())?;
    base.set_scheme("https").map_err(|_| "invalid scheme")?;
    let url = base.join(&file.url).map_err(|e| e.to_string())?;
    if url.scheme() != "https" {
        return Err("Only HTTPS downloads are supported".into());
    }
    let same_origin = url.origin() == base.origin();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_preconfigured_tls(
            crate::gateway::tls_config(
                if same_origin { pin } else { None },
                insecure && same_origin,
            )
            .map_err(|e| e.to_string())?,
        )
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.get(url);
    if same_origin && let Ok(token) = std::env::var("VEYTERM_TOKEN") {
        request = request.bearer_auth(token);
    }
    let mut response = request
        .send()
        .map_err(|_| "Download request failed")?
        .error_for_status()
        .map_err(|_| "Download rejected by server")?;
    let name = std::path::Path::new(&file.name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("incoming-file");
    let mut chosen = None;
    for n in 0..10000 {
        let path = if n == 0 {
            name.to_owned()
        } else {
            format!("{name}.{n}")
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => {
                chosen = Some((path, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    let (path, mut output) = chosen.ok_or("No unused filename")?;
    if std::io::copy(&mut response, &mut output)
        .and_then(|_| output.flush())
        .is_err()
    {
        let _ = std::fs::remove_file(&path);
        return Err("Download failed".into());
    }
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_and_image_blocks() {
        let v = serde_json::json!({"content":[{"type":"attachment","attachment":{"url":"/api/chat/media/outgoing/s/id/full","label":"report.pdf","mimeType":"application/pdf"}},{"type":"image","url":"https://example.test/photo.png"},{"type":"text","text":"hello"}]});
        let mut files = vec![];
        collect(&v, &mut files);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "report.pdf");
    }
}
