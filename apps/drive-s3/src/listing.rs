use drive_s3_store::ObjectMeta;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use s3s::dto::{CommonPrefix, ETag, Object};
use std::collections::BTreeMap;

const ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

pub fn encoded(s: &str, encode: bool) -> String {
    if encode {
        utf8_percent_encode(s, ENCODE).to_string()
    } else {
        s.to_owned()
    }
}

pub struct Page {
    pub objects: Vec<Object>,
    pub prefixes: Vec<CommonPrefix>,
    pub next: Option<String>,
}

pub fn page(
    rows: Vec<ObjectMeta>,
    prefix: &str,
    delimiter: &str,
    after: &str,
    max: usize,
    url: bool,
) -> Page {
    let mut entries = BTreeMap::new();
    for row in rows {
        let Some(suffix) = row.key.strip_prefix(prefix) else {
            continue;
        };
        if !delimiter.is_empty()
            && let Some(i) = suffix.find(delimiter)
        {
            let key = &row.key[..prefix.len() + i + delimiter.len()];
            if key > after {
                entries.insert(key.to_owned(), None);
            }
        } else if row.key.as_str() > after {
            entries.insert(row.key.clone(), Some(row));
        }
    }
    let truncated = entries.len() > max;
    let mut output = Page {
        objects: vec![],
        prefixes: vec![],
        next: None,
    };
    for (key, item) in entries.into_iter().take(max) {
        let name = encoded(&key, url);
        match item {
            Some(row) => output.objects.push(Object {
                key: Some(name),
                size: Some(row.size as i64),
                e_tag: Some(ETag::Strong(row.etag)),
                last_modified: Some(super::s3::timestamp(row.modified_ms)),
                ..Default::default()
            }),
            None => output.prefixes.push(CommonPrefix { prefix: Some(name) }),
        }
        if truncated {
            output.next = Some(key);
        }
    }
    output
}
