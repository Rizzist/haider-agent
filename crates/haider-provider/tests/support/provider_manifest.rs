use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) schema: String,
    pub(crate) provisional: bool,
    pub(crate) provenance: String,
    pub(crate) fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Fixture {
    pub(crate) name: String,
    pub(crate) transport: String,
    pub(crate) status: u16,
    pub(crate) retry_after: Option<String>,
    pub(crate) wire: String,
    pub(crate) golden: String,
}
