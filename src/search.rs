use crate::types::{EsResponse, PackageHit};
use serde_json::json;

/// The public read-only Elasticsearch credentials used by search.nixos.org's
/// own web frontend (same backend `nix-search-cli` talks to). These are
/// intentionally client-visible: the site ships them to every browser tab.
const ES_URL: &str = "https://search.nixos.org/backend";
const ES_USER: &str = "aWVSALXpZv";
const ES_PASS: &str = "X8gPHnzL52wFEekuxsfQ9cSh";
/// search.nixos.org bumps this when the document schema changes. If searches
/// start returning "index_not_found_exception", check search.nixos.org's page
/// source for the current `elasticsearchMappingSchemaVersion` and update this.
const SCHEMA_VERSION: u32 = 51;

/// Channels mirror search.nixos.org's channels.json. `flake_ref` is the exact
/// nixpkgs revision-tracking ref used to launch packages, so results always
/// come from (and launch against) the same channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub id: &'static str,
    pub branch: &'static str,
    pub label: &'static str,
}

pub const CHANNELS: &[Channel] = &[
    Channel {
        id: "unstable",
        branch: "nixos-unstable",
        label: "unstable",
    },
    Channel {
        id: "26.05",
        branch: "nixos-26.05",
        label: "26.05 (stable)",
    },
];

impl Channel {
    pub fn flake_ref(&self) -> String {
        format!("github:NixOS/nixpkgs/{}", self.branch)
    }

    fn index_name(&self) -> String {
        format!("latest-{}-nixos-{}", SCHEMA_VERSION, self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Search package name + description (the common case).
    Packages,
    /// Search by the binary/program a package provides (what would end up on
    /// $PATH), e.g. typing "rg" finds `ripgrep`.
    Programs,
}

impl SearchMode {
    pub fn label(&self) -> &'static str {
        match self {
            SearchMode::Packages => "Packages",
            SearchMode::Programs => "Programs",
        }
    }
}

/// `package_attr_name`/`package_pname`/`package_programs` are ES `keyword`
/// fields with an `.edge` (edge-ngram-from-the-start) sub-field. Neither can
/// find a mid-string match on their own: the base field needs an exact
/// value, and `.edge` only ever ngrams a value from its first character, so
/// e.g. "chrome" never matches inside "google-chrome". So substring hits
/// have to come from an explicit `wildcard` query (heavily boosted, since
/// wildcard scores are a flat 1.0 and would otherwise be drowned out by the
/// prefix/description clauses below), and prefix-as-you-type ("goog...")
/// comes from `.edge` with the search analyzer pinned to `lowercase`
/// (single whole-string token) instead of the field's own `edge_ngram`
/// analyzer, which — if used at query time too — shreds the query itself
/// into ["c","ch","chr",...] and floods the results with noise.
fn match_search(query: &str) -> serde_json::Value {
    json!({
        "dis_max": {
            "tie_breaker": 0.3,
            "queries": [
                { "term": { "package_attr_name": { "value": query, "boost": 200 } } },
                { "term": { "package_pname": { "value": query, "boost": 180 } } },
                { "wildcard": { "package_attr_name": { "value": format!("*{query}*"), "case_insensitive": true, "boost": 50 } } },
                { "wildcard": { "package_pname": { "value": format!("*{query}*"), "case_insensitive": true, "boost": 45 } } },
                { "wildcard": { "package_programs": { "value": format!("*{query}*"), "case_insensitive": true, "boost": 40 } } },
                { "multi_match": {
                    "query": query,
                    "analyzer": "lowercase",
                    "type": "bool_prefix",
                    "fields": ["package_attr_name.edge^9", "package_pname.edge^7", "package_programs.edge^4"]
                }},
                { "multi_match": {
                    "query": query,
                    "fields": ["package_description^1.4", "package_longDescription^1"]
                }}
            ]
        }
    })
}

/// Mirrors `nix-search-cli`'s `MatchProgram`: prefix + exact match against
/// the binaries a package provides.
fn match_program(query: &str) -> serde_json::Value {
    json!({
        "dis_max": {
            "tie_breaker": 0.7,
            "queries": [
                { "wildcard": { "package_programs": { "value": format!("{query}*"), "case_insensitive": true } } },
                { "match": { "package_programs": query } }
            ]
        }
    })
}

fn build_query(mode: SearchMode, query: &str) -> serde_json::Value {
    let must = match mode {
        SearchMode::Packages => match_search(query),
        SearchMode::Programs => match_program(query),
    };

    json!({
        "size": 60,
        "query": {
            "bool": {
                "filter": [{ "term": { "type": "package" } }],
                "must": [must]
            }
        },
        "sort": [
            { "_score": "desc" },
            { "package_attr_name": "desc" },
            { "package_pversion": "desc" }
        ]
    })
}

pub fn run_search(
    client: &reqwest::blocking::Client,
    channel: Channel,
    mode: SearchMode,
    query: &str,
) -> Result<Vec<PackageHit>, String> {
    let url = format!("{ES_URL}/{}/_search", channel.index_name());
    let body = build_query(mode, query);

    let resp = client
        .post(&url)
        .basic_auth(ES_USER, Some(ES_PASS))
        .json(&body)
        .send()
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        return Err(format!("search backend returned {status}: {text}"));
    }

    let parsed: EsResponse = resp
        .json()
        .map_err(|e| format!("failed to parse response: {e}"))?;

    Ok(parsed.hits.hits.into_iter().map(|h| h.source).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the real search.nixos.org backend. Not run by default since it
    /// needs network; run explicitly with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn packages_mode_finds_chrome() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "chrome").unwrap();
        assert!(hits.iter().any(|h| h.attr_name == "google-chrome"), "{hits:#?}");
    }

    #[test]
    #[ignore]
    fn packages_mode_finds_exact_attr_name() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "ripgrep").unwrap();
        assert_eq!(hits[0].attr_name, "ripgrep", "{hits:#?}");
    }

    #[test]
    #[ignore]
    fn stable_channel_returns_results() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[1], SearchMode::Packages, "firefox").unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    #[ignore]
    fn programs_mode_finds_ripgrep_by_binary_name() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Programs, "rg").unwrap();
        assert!(hits.iter().any(|h| h.attr_name == "ripgrep"), "{hits:#?}");
    }
}
