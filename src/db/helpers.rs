use anyhow::Result;

pub(crate) fn json_or_null(v: &Option<Vec<String>>) -> Result<Option<String>> {
    Ok(match v {
        None => None,
        Some(list) => Some(serde_json::to_string(list)?),
    })
}

pub(crate) fn parse_json_array(raw: Option<String>) -> Option<Vec<String>> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
}

pub(crate) fn json_or_null_owned(v: Option<&Vec<String>>) -> Result<Option<String>> {
    Ok(match v {
        None => None,
        Some(list) => Some(serde_json::to_string(list)?),
    })
}
