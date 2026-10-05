//! Replay metadata is retained as data; annotations grant no URL/file or tool authority.
use super::{ContentError, Value, fields, invalid, present, string};

pub(super) fn validate(value: &Value) -> Result<(), ContentError> {
    match value["type"].as_str() {
        Some("url_citation") => {
            fields(
                value,
                &["type", "start_index", "end_index", "url", "title"],
                &[],
            )?;
            if !present(value, "url") || !string(value, "title") {
                return Err(invalid());
            }
            span(value)
        }
        Some("file_citation") => {
            fields(value, &["type", "file_id", "filename", "index"], &[])?;
            if !present(value, "file_id")
                || !string(value, "filename")
                || value["index"].as_u64().is_none()
            {
                return Err(invalid());
            }
            Ok(())
        }
        Some("file_path") => {
            fields(value, &["type", "file_id", "index"], &[])?;
            if !present(value, "file_id") || value["index"].as_u64().is_none() {
                return Err(invalid());
            }
            Ok(())
        }
        Some("container_file_citation") => {
            fields(
                value,
                &[
                    "type",
                    "container_id",
                    "file_id",
                    "filename",
                    "start_index",
                    "end_index",
                ],
                &[],
            )?;
            if !present(value, "container_id")
                || !present(value, "file_id")
                || !string(value, "filename")
            {
                return Err(invalid());
            }
            span(value)
        }
        _ => Err(invalid()),
    }
}
fn span(value: &Value) -> Result<(), ContentError> {
    match (value["start_index"].as_u64(), value["end_index"].as_u64()) {
        (Some(start), Some(end)) if start <= end => Ok(()),
        _ => Err(invalid()),
    }
}
