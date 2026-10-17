//! Request parameters read the way the Bot API server reads them
//! (`td/tdnet/td/net/HttpReader.cpp`): the query string first, then a JSON,
//! URL-encoded or multipart body. A name's first value wins, so the query
//! string's beats the body's, as the server's `Query::arg` takes the first
//! match.

use std::{collections::BTreeMap, convert::Infallible};

use bytes::Bytes;

/// One multipart part that carried a file name.
#[derive(Clone, Debug)]
pub(crate) struct FilePart {
    pub name: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub data: Bytes,
}

/// Text arguments and file parts of one request.
#[derive(Clone, Debug, Default)]
pub(crate) struct Params {
    pub args: BTreeMap<String, String>,
    pub files: Vec<FilePart>,
}

/// Why a body could not be read.
#[derive(Debug)]
pub(crate) enum BodyError {
    NotAnObject,
    Json,
    Multipart,
}

impl BodyError {
    pub fn description(&self) -> &'static str {
        match self {
            Self::NotAnObject | Self::Json => "Bad Request: can't parse JSON object",
            Self::Multipart => "Bad Request: can't parse multipart/form-data",
        }
    }
}

impl Params {
    /// Read a URL-encoded query string or form body.
    pub fn add_form(&mut self, form: &[u8]) {
        for (key, value) in form_urlencoded::parse(form) {
            self.add(key.into_owned(), value.into_owned());
        }
    }

    /// Adds an argument unless the name already has one.
    fn add(&mut self, name: String, value: String) {
        self.args.entry(name).or_insert(value);
    }

    /// Read a body by its content type. Like the server, the match is a
    /// substring of the lower-cased header, and other types yield no
    /// arguments.
    pub async fn add_body(&mut self, content_type: &str, body: Bytes) -> Result<(), BodyError> {
        let lower = content_type.to_ascii_lowercase();
        if lower.contains("multipart/form-data") {
            // The boundary keeps its case.
            self.add_multipart(content_type, body).await
        } else if lower.contains("application/x-www-form-urlencoded") {
            self.add_form(&body);
            Ok(())
        } else if lower.contains("application/json") {
            self.add_json(&body)
        } else {
            Ok(())
        }
    }

    /// A top-level JSON object: strings as they are, other values as their
    /// JSON text, nulls skipped.
    fn add_json(&mut self, body: &[u8]) -> Result<(), BodyError> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| BodyError::Json)?;
        let serde_json::Value::Object(fields) = value else {
            return Err(BodyError::NotAnObject);
        };
        for (key, value) in fields {
            let text = match value {
                serde_json::Value::Null => continue,
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            };
            self.add(key, text);
        }
        Ok(())
    }

    /// Text fields become arguments; parts with a file name become files,
    /// in any order.
    async fn add_multipart(&mut self, content_type: &str, body: Bytes) -> Result<(), BodyError> {
        let boundary = multer::parse_boundary(content_type).map_err(|_| BodyError::Multipart)?;
        let stream = futures_util::stream::once(std::future::ready(Ok::<_, Infallible>(body)));
        let mut multipart = multer::Multipart::new(stream, boundary);
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|_| BodyError::Multipart)?
        {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let part_type = field.content_type().map(ToString::to_string);
            let data = field.bytes().await.map_err(|_| BodyError::Multipart)?;
            match filename {
                Some(filename) => self.files.push(FilePart {
                    name,
                    filename,
                    content_type: part_type,
                    data,
                }),
                None => self.add(name, String::from_utf8_lossy(&data).into_owned()),
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.args.get(name).map(String::as_str)
    }

    /// An integer argument, read like the server's `td::to_integer`: text
    /// that is not a number reads as 0; then clamped to `min..=max`.
    pub fn integer(&self, name: &str, default: i64, min: i64, max: i64) -> i64 {
        self.get(name)
            .map_or(default, |text| text.trim().parse().unwrap_or(0))
            .clamp(min, max)
    }

    /// A boolean argument: `true` (any case) or `1`.
    pub fn flag(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|text| text.eq_ignore_ascii_case("true") || text == "1")
    }

    /// The file a `field` refers to: a part with that name, or an
    /// `attach://<part>` argument.
    pub fn file(&self, field: &str) -> Option<&FilePart> {
        let by_name = |name: &str| self.files.iter().find(|part| part.name == name);
        by_name(field).or_else(|| {
            self.get(field)
                .and_then(|value| value.strip_prefix("attach://"))
                .and_then(by_name)
        })
    }
}
