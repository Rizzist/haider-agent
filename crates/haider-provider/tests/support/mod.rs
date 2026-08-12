use std::fs;
use std::path::Path;

use haider_protocol::provider::StreamEvent;
use haider_provider::{ProviderError, ProviderStreamItem};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub(crate) enum ExpectedItem {
    Ok(StreamEvent),
    Err(ProviderError),
}

impl ExpectedItem {
    pub(crate) fn into_result(self) -> ProviderStreamItem {
        match self {
            Self::Ok(event) => Ok(event),
            Self::Err(error) => Err(error),
        }
    }
}

pub(crate) fn reanchor_events(path: &Path, actual: &[ProviderStreamItem]) {
    if std::env::var_os("UPDATE_FIXTURES").is_none() {
        return;
    }
    let tagged = actual
        .iter()
        .map(|item| match item {
            Ok(event) => serde_json::json!({"result": "ok", "value": event}),
            Err(error) => serde_json::json!({"result": "err", "value": error}),
        })
        .collect::<Vec<_>>();
    fs::write(
        path,
        serde_json::to_string_pretty(&tagged).expect("serialize event golden"),
    )
    .expect("write event golden");
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let bytes = fs::read(path).expect("reads JSON fixture");
    serde_json::from_slice(&bytes).expect("parses JSON fixture")
}
