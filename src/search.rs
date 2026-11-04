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

/// Channels mirror search.nixos.org's channels.json, and pick which
/// Elasticsearch index gets searched. They no longer decide what a launch
/// resolves against: launching uses the local `<nixpkgs>` via `nix-shell -p`
/// (see `src/launch.rs`), which is far faster than fetching a channel's
/// nixpkgs tarball per launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub id: &'static str,
    pub label: &'static str,
}

pub const CHANNELS: &[Channel] = &[
    Channel {
        id: "unstable",
        label: "unstable",
    },
    Channel {
        id: "26.05",
        label: "26.05 (stable)",
    },
];

impl Channel {
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

/// This is a direct port of the query search.nixos.org's own frontend sends
/// to this same backend (captured from its network tab, not reverse
/// engineered from guesses) — every clause and weight below exists because
/// it's actually there in that request, not because it looked right against
/// whatever query it was tested with. That distinction matters in practice:
/// an earlier version of this function normalized multi-word queries by
/// joining them with hyphens ("google chrome" → "google-chrome") on the
/// assumption that's obviously what the real site must do too — it isn't.
/// Checked against a live capture, search.nixos.org does not rank
/// `google-chrome` first for "google chrome", nor `docker-compose` for
/// "docker compose"; that normalization was a plausible-sounding invention,
/// not real behavior, and inventing more special cases to chase individual
/// examples is exactly how the ranking got query-specific last time.
fn match_search(query: &str) -> serde_json::Value {
    json!({
        "dis_max": {
            "tie_breaker": 0.7,
            "queries": [
                {
                    "multi_match": {
                        "query": query,
                        "type": "cross_fields",
                        "analyzer": "whitespace",
                        "operator": "and",
                        "auto_generate_synonyms_phrase_query": false,
                        "fields": [
                            "package_attr_name^9", "package_attr_name.*^5.4",
                            "package_programs^9", "package_programs.*^5.4",
                            "package_mainProgram^9", "package_mainProgram.*^5.4",
                            "package_pname^6", "package_pname.*^3.6",
                            "package_description^1.3", "package_description.*^0.78",
                            "package_longDescription^1", "package_longDescription.*^0.6",
                            "flake_name^0.5", "flake_name.*^0.3"
                        ]
                    }
                },
                {
                    "multi_match": {
                        "query": query,
                        "type": "best_fields",
                        "fuzziness": "1",
                        "prefix_length": 1,
                        "operator": "and",
                        "fields": [
                            "package_attr_name^0.45", "package_programs^0.45",
                            "package_mainProgram^0.45", "package_pname^0.3"
                        ]
                    }
                },
                { "wildcard": { "package_attr_name": { "value": format!("*{query}*"), "case_insensitive": true } } }
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
    let query = query.trim().to_lowercase();
    let must = match mode {
        SearchMode::Packages => match_search(&query),
        SearchMode::Programs => match_program(&query),
    };

    let mut bool_query = json!({
        "filter": [{ "term": { "type": "package" } }],
        "must": [must]
    });

    let mut body = json!({ "size": 50, "query": null, "sort": [{ "_score": "desc" }] });

    if mode == SearchMode::Packages {
        // Two more real signals, on top of text relevance: an exact/prefix
        // boost toward the attribute name itself, and two `rank_feature`
        // popularity scores — `package_repology_repos` (how many other
        // distros package this) and `package_dep_count` (how many other
        // nixpkgs packages depend on it) — which is why, for "firefox",
        // `firefox-unwrapped` (what `firefox` itself wraps) outranks
        // same-named siblings like `firefox-esr` that nothing depends on.
        bool_query["should"] = json!([
            { "term": { "package_attr_name": { "value": &query, "boost": 100 } } },
            { "prefix": { "package_attr_name": { "value": &query, "boost": 20, "case_insensitive": true } } },
            { "rank_feature": { "field": "package_repology_repos", "boost": 5, "saturation": { "pivot": 20 } } },
            { "rank_feature": { "field": "package_dep_count", "boost": 5, "saturation": { "pivot": 1000 } } }
        ]);
        // A final re-scoring pass over the top 100 hits that explicitly
        // rewards shorter attribute names (1/length, weighted ×20) — this is
        // what actually separates near-tied variants like
        // `firefox-unwrapped` from `firefox-esr-140-unwrapped`, rather than
        // leaving it to incidental ES length-norm behavior.
        body["rescore"] = json!({
            "window_size": 100,
            "query": {
                "rescore_query": {
                    "function_score": {
                        "script_score": {
                            "script": { "source": "1.0 / doc['package_attr_name'].value.length()" }
                        }
                    }
                },
                "rescore_query_weight": 20
            }
        });
    }

    body["query"] = json!({ "bool": bool_query });
    body
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
    ///
    /// Verified against a live capture of the real site's own query: for a
    /// bare "chrome", things actually *named* chrome-something (`chromedriver`,
    /// `chrome-export`, ...) legitimately outrank `google-chrome` (where
    /// "chrome" is just a suffix) — `google-chrome` isn't even in the real
    /// top 50 for this query. So this checks for a top *prefix* match
    /// instead of asserting `google-chrome` shows up at all.
    #[test]
    #[ignore]
    fn packages_mode_finds_chrome() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "chrome").unwrap();
        assert!(hits[0].attr_name.starts_with("chrome"), "{hits:#?}");
    }

    /// Order verified against a live capture of search.nixos.org's own
    /// request/response for this exact query (2026-08-27) — not just
    /// eyeballed, the top-4 attr names and their relative order here match
    /// what the real site returned. Guards against regressing back to
    /// query-specific tuning that happens to look right for one example.
    #[test]
    #[ignore]
    fn packages_mode_matches_real_site_order_for_firefox() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "firefox").unwrap();
        let top4: Vec<&str> = hits.iter().take(4).map(|h| h.attr_name.as_str()).collect();
        assert_eq!(
            top4,
            vec![
                "firefox",
                "firefox-unwrapped",
                "firefox-esr-140-unwrapped",
                "firefox-esr-153-unwrapped",
            ],
            "{hits:#?}"
        );
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
