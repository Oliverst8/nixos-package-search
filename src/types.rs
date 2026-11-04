use serde::{Deserialize, Serialize};

/// `Serialize` as well as `Deserialize` because launch history stores whole
/// hits (see `src/history.rs`); the field renames below apply in both
/// directions, so a stored entry is shaped exactly like the Elasticsearch
/// document it came from and round-trips through the same code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageHit {
    #[serde(rename = "package_attr_name", default)]
    pub attr_name: String,
    #[serde(rename = "package_pname", default)]
    pub pname: String,
    #[serde(rename = "package_pversion", default)]
    pub version: String,
    #[serde(rename = "package_outputs", default)]
    pub outputs: Vec<String>,
    #[serde(rename = "package_description", default)]
    pub description: Option<String>,
    #[serde(rename = "package_longDescription", default)]
    pub long_description: Option<String>,
    #[serde(rename = "package_homepage", default)]
    pub homepage: Vec<String>,
    #[serde(rename = "package_license_set", default)]
    pub licenses: Vec<String>,
    #[serde(rename = "package_maintainers_set", default)]
    pub maintainers: Vec<String>,
    #[serde(rename = "package_mainProgram", default)]
    pub main_program: Option<String>,
    #[serde(rename = "package_programs", default)]
    pub programs: Vec<String>,
    #[serde(rename = "package_platforms", default)]
    pub platforms: Vec<String>,
    #[serde(rename = "package_position", default)]
    pub position: Option<String>,
}

impl PackageHit {
    /// Best-guess binary to run: the declared mainProgram, falling back to the
    /// package name (which is what `nix run`/`nix shell -c` would try anyway).
    pub fn runnable(&self) -> String {
        self.main_program
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.pname.clone())
    }
}

#[derive(Debug, Deserialize)]
pub struct EsResponse {
    pub hits: EsHits,
}

#[derive(Debug, Deserialize)]
pub struct EsHits {
    pub hits: Vec<EsHit>,
}

#[derive(Debug, Deserialize)]
pub struct EsHit {
    #[serde(rename = "_source")]
    pub source: PackageHit,
}
