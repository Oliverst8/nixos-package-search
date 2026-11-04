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
const SCHEMA_VERSION: u32 = 52;

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

/// A port of `packagesRanking` + `encodeRequestBody` from
/// search.nixos.org's own frontend
/// (`frontend/src/Search/Query.elm` in NixOS/nixos-search), not a
/// reconstruction from a captured request body. Every clause, field, boost
/// and derivation below is the one that file builds, in the order it builds
/// them, because matching the real thing is the whole point: the site's
/// ranking is, in its own words, "the champion of an evolutionary search
/// over the clause structure, the fields and the boosts", so none of these
/// numbers can be reasoned about one at a time, and a plausible-looking
/// rewrite is how this ended up diverging from the site before.
///
/// The one thing deliberately left out is the `aggs` block, which only
/// feeds the site's facet sidebar. It affects neither which documents come
/// back nor their order, and this app has no facets.
mod rank {
    use serde_json::{Value, json};

    /// `whenTyped`: derivations that only make sense against text the user
    /// actually typed. `glued` is the exception — an empty attribute name is
    /// still a name to ask about — so it doesn't go through this.
    fn when_typed(words: &[String], derived: Vec<String>) -> Vec<String> {
        if words.iter().any(|w| !w.is_empty()) {
            derived
        } else {
            Vec::new()
        }
    }

    /// The whole query as a phrase, but only where there is more than one
    /// word: a phrase over a single word is that word, which the clauses
    /// naming it already cover.
    fn multi_word_whole(words: &[String]) -> Vec<String> {
        if words.len() > 1 {
            vec![words.join(" ")]
        } else {
            Vec::new()
        }
    }

    /// The words glued back into one token. A keyword field holds a name as
    /// a single token, so a multi-word query only reaches one glued up.
    fn glued(separator: &str, words: &[String]) -> Vec<String> {
        vec![words.join(separator)]
    }

    /// The words read as an attribute path: `python http server` →
    /// `python.http.server`.
    fn dotted(words: &[String]) -> Vec<String> {
        when_typed(words, vec![words.join(".")])
    }

    /// The last word of `nginx virtual hosts` — the one that names.
    fn last_word(words: &[String]) -> Vec<String> {
        when_typed(words, words.last().cloned().into_iter().collect())
    }

    /// Everything before the last word — the part that qualifies.
    fn all_but_last(words: &[String]) -> Vec<String> {
        let leading = words.split_last().map(|(_, rest)| rest).unwrap_or(&[]);
        if leading.is_empty() {
            Vec::new()
        } else {
            when_typed(words, vec![leading.join(" ")])
        }
    }

    /// `[w with _ as -, w with - as _, w]`, in that order — the site relies
    /// on the order for deduplication, so a word containing neither
    /// contributes exactly one spelling.
    pub fn dash_underscore_variants(word: &str) -> Vec<String> {
        vec![
            word.replace('_', "-"),
            word.replace('-', "_"),
            word.to_string(),
        ]
    }

    pub fn unique(words: Vec<String>) -> Vec<String> {
        let mut seen = Vec::new();
        for w in words {
            if !seen.contains(&w) {
                seen.push(w);
            }
        }
        seen
    }

    /// One spelling per word: optionally each dash/underscore spelling,
    /// optionally wrapped in `*` for a substring match.
    fn per_word(variants: bool, surround: bool, words: &[String]) -> Vec<String> {
        let expanded = if variants {
            words
                .iter()
                .flat_map(|w| dash_underscore_variants(w))
                .collect()
        } else {
            words.to_vec()
        };
        unique(expanded)
            .into_iter()
            .map(|w| if surround { format!("*{w}*") } else { w })
            .collect()
    }

    /// A `term`, `prefix` or `wildcard` against one keyword field.
    fn keyword_clauses(
        kind: &str,
        field: &str,
        boost: Option<f64>,
        case_insensitive: bool,
        texts: Vec<String>,
    ) -> Vec<Value> {
        texts
            .into_iter()
            .map(|text| {
                let mut inner = json!({ "value": text });
                if let Some(boost) = boost {
                    inner["boost"] = json!(boost);
                }
                if case_insensitive {
                    inner["case_insensitive"] = json!(true);
                }
                json!({ kind: { field: inner } })
            })
            .collect()
    }

    /// A `multi_match` over weighted fields. `between` carries whatever the
    /// clause sets between the query and the fields — an analyzer, a
    /// fuzziness, a name.
    fn multi_match_clauses(
        kind: &str,
        between: &[(&str, Value)],
        fields: &[&str],
        boost: Option<f64>,
        texts: Vec<String>,
    ) -> Vec<Value> {
        texts
            .into_iter()
            .map(|text| {
                let mut m = json!({ "type": kind, "query": text });
                for (k, v) in between {
                    m[*k] = v.clone();
                }
                m["fields"] = json!(fields);
                if let Some(boost) = boost {
                    m["boost"] = json!(boost);
                }
                json!({ "multi_match": m })
            })
            .collect()
    }

    /// Discards the score of what it wraps and returns a fixed boost, making
    /// the clause a yes/no rather than a how-well.
    fn constant_score(boost: f64, inner: Vec<Value>) -> Vec<Value> {
        inner
            .into_iter()
            .map(|one| json!({ "constant_score": { "filter": one, "boost": boost } }))
            .collect()
    }

    fn rank_feature(field: &str, boost: Option<f64>, name: Option<&str>, func: Value) -> Value {
        let mut rf = json!({ "field": field });
        if let Some(boost) = boost {
            rf["boost"] = json!(boost);
        }
        if let Some(name) = name {
            rf["_name"] = json!(name);
        }
        let obj = rf.as_object_mut().expect("rank_feature is an object");
        for (k, v) in func.as_object().expect("fn is an object") {
            obj.insert(k.clone(), v.clone());
        }
        json!({ "rank_feature": rf })
    }

    /// The best of several readings rather than the sum, so a document
    /// matching two of them isn't thereby twice as good a hit.
    fn dis_max(tie_breaker: f64, queries: Vec<Value>) -> Value {
        json!({ "dis_max": { "tie_breaker": tie_breaker, "queries": queries } })
    }

    /// `packagesRanking`, clause for clause. Returns `(must, should)`.
    pub fn packages(words: &[String]) -> (Vec<Value>, Vec<Value>) {
        // The whole query as a phrase over both description fields, so a
        // query that names no package still matches on description alone —
        // which is where the previous shape was losing.
        let description_phrase = constant_score(
            110.0,
            multi_match_clauses(
                "phrase",
                &[],
                &["package_description^7.42", "package_longDescription^1"],
                None,
                multi_word_whole(words),
            ),
        );

        // The last word only, spelled approximately: `fierfix` still finds
        // `firefox`.
        let fuzzy_last_word = multi_match_clauses(
            "best_fields",
            &[
                ("fuzziness", json!("1")),
                ("prefix_length", json!(1)),
                ("minimum_should_match", json!("20%")),
                ("_name", json!(format!("fuzzy_{}", words.join("_")))),
            ],
            &[
                "package_attr_name^0.378",
                "package_programs^0.441",
                "package_mainProgram^0.413",
                // Not 0.3: the site sends this float artifact verbatim.
                "package_pname^0.30000000000000004",
            ],
            None,
            last_word(words),
        );

        // Each word as a substring of the attribute name, on the plain
        // keyword rather than its edge-ngrams.
        let attr_name_substrings = keyword_clauses(
            "wildcard",
            "package_attr_name",
            None,
            true,
            per_word(true, true, words),
        );

        // The words glued back into one attribute name, exactly and as a
        // prefix — this is what puts `google-chrome` first for
        // "google chrome".
        let exact_name = keyword_clauses(
            "term",
            "package_attr_name",
            Some(127.0),
            false,
            glued("-", words),
        );
        let name_prefix = keyword_clauses(
            "prefix",
            "package_attr_name",
            Some(16.4),
            true,
            glued("", words),
        );
        let pname_prefix = keyword_clauses(
            "prefix",
            "package_pname",
            Some(10.1),
            true,
            glued("", words),
        );

        // Everything but the last word, which for `python http server` is
        // the part that qualifies rather than names.
        let leading_words = multi_match_clauses(
            "cross_fields",
            &[("analyzer", json!("keyword"))],
            &[
                "package_pname.edge^3.52",
                "package_description^1.17",
                "package_programs^117",
            ],
            None,
            all_but_last(words),
        );

        // Popularity, read twice from the same field. The `log` one looks
        // redundant next to the `saturation` one and is not: the site
        // measured removing it at -0.014 nDCG.
        let popularity = vec![
            rank_feature(
                "package_repology_repos",
                Some(328.0),
                None,
                json!({ "log": { "scaling_factor": 5140.0 } }),
            ),
            rank_feature(
                "package_repology_repos",
                Some(5.0),
                Some("popularity_package_repology_repos"),
                json!({ "saturation": { "pivot": 13.5 } }),
            ),
        ];

        // Dependency count beside a weak per-word match on the long
        // description, the attribute set and the attribute name's
        // edge-ngrams. Also measured rather than argued.
        let dep_count_and_text = vec![json!({
            "bool": {
                "must": [rank_feature(
                    "package_dep_count",
                    None,
                    None,
                    json!({ "sigmoid": { "pivot": 7580.0, "exponent": 0.434 } }),
                )],
                "should": multi_match_clauses(
                    "best_fields",
                    &[("analyzer", json!("lowercase"))],
                    &[
                        "package_longDescription.edge^12",
                        "package_attr_set^2.97",
                        "package_attr_name.edge^0.307",
                    ],
                    Some(0.128),
                    per_word(false, false, words),
                ),
                "boost": 0.364
            }
        })];

        // The query read as a program path: `python.http.server`.
        let program_path = keyword_clauses(
            "wildcard",
            "package_programs",
            Some(12.8),
            true,
            dotted(words),
        );

        let must = vec![dis_max(
            0.387,
            [description_phrase, fuzzy_last_word, attr_name_substrings].concat(),
        )];
        let should = [
            exact_name,
            name_prefix,
            leading_words,
            pname_prefix,
            popularity,
            dep_count_and_text,
            program_path,
        ]
        .concat();
        (must, should)
    }
}

/// Mirrors `nix-search-cli`'s `MatchProgram`: prefix + exact match against
/// the binaries a package provides. This is the app's own extra mode (the
/// site has no equivalent), so it isn't part of the port above.
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
    // `encodeRequestBody`: lowercase, split on whitespace, and peel off the
    // `-word` exclusions before the ranking ever sees the words.
    let lowered = query.trim().to_lowercase();
    let (negative, positive): (Vec<String>, Vec<String>) = lowered
        .split_whitespace()
        .map(str::to_string)
        .partition(|w| w.starts_with('-'));
    let negative: Vec<String> = negative.iter().map(|w| w[1..].to_string()).collect();
    let positive: Vec<String> = positive;

    // `-foo` excludes anything whose attribute name contains foo, in each of
    // its dash/underscore spellings.
    let must_not: Vec<serde_json::Value> = rank::unique(
        negative
            .iter()
            .flat_map(|w| rank::dash_underscore_variants(w))
            .collect(),
    )
    .into_iter()
    .map(|word| {
        json!({ "wildcard": { "package_attr_name": {
            "value": format!("*{word}*"),
            "case_insensitive": true
        } } })
    })
    .collect();

    let (must, should) = match mode {
        SearchMode::Packages => rank::packages(&positive),
        SearchMode::Programs => (vec![match_program(&positive.join(" "))], Vec::new()),
    };

    let mut body = json!({
        "from": 0,
        "size": 50,
        "sort": [{ "_score": "desc" }],
        "query": {
            "bool": {
                "filter": [{ "term": { "type": { "value": "package", "_name": "filter_packages" } } }],
                "must_not": must_not,
                "must": must,
                "should": should
            }
        }
    });

    if mode == SearchMode::Packages {
        // The final pass the site makes over its top 100: 1/length of the
        // attribute name, weighted ×30.3. This is what separates near-tied
        // variants like `firefox` from `firefox-esr-140-unwrapped`.
        body["rescore"] = json!({
            "window_size": 100,
            "query": {
                "rescore_query": {
                    "function_score": {
                        "script_score": {
                            "script": {
                                "source": "doc['package_attr_name'].size() == 0 ? 0 : 1.0 / doc['package_attr_name'].value.length()"
                            }
                        }
                    }
                },
                "rescore_query_weight": 30.3
            }
        });
    }

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

    /// The exact body search.nixos.org's own frontend sent for the query
    /// "google chrome", captured from its network tab. `build_query` is a
    /// port of the Elm that produced it (see `mod rank`), so the two must
    /// agree clause for clause — this test is what makes that claim
    /// checkable without the network, and what will fail loudly if someone
    /// "tidies up" a boost.
    ///
    /// The site also sends an `aggs` block for its facet sidebar, dropped
    /// here and in `build_query`: it changes neither the hits nor their
    /// order.
    const CAPTURED_GOOGLE_CHROME: &str = r##"{"from":0,"size":50,"sort":[{"_score":"desc"}],"query":{"bool":{"filter":[{"term":{"type":{"value":"package","_name":"filter_packages"}}}],"must_not":[],"must":[{"dis_max":{"tie_breaker":0.387,"queries":[{"constant_score":{"filter":{"multi_match":{"type":"phrase","query":"google chrome","fields":["package_description^7.42","package_longDescription^1"]}},"boost":110}},{"multi_match":{"type":"best_fields","query":"chrome","fuzziness":"1","prefix_length":1,"minimum_should_match":"20%","_name":"fuzzy_google_chrome","fields":["package_attr_name^0.378","package_programs^0.441","package_mainProgram^0.413","package_pname^0.30000000000000004"]}},{"wildcard":{"package_attr_name":{"value":"*google*","case_insensitive":true}}},{"wildcard":{"package_attr_name":{"value":"*chrome*","case_insensitive":true}}}]}}],"should":[{"term":{"package_attr_name":{"value":"google-chrome","boost":127}}},{"prefix":{"package_attr_name":{"value":"googlechrome","boost":16.4,"case_insensitive":true}}},{"multi_match":{"type":"cross_fields","query":"google","analyzer":"keyword","fields":["package_pname.edge^3.52","package_description^1.17","package_programs^117"]}},{"prefix":{"package_pname":{"value":"googlechrome","boost":10.1,"case_insensitive":true}}},{"rank_feature":{"field":"package_repology_repos","boost":328,"log":{"scaling_factor":5140}}},{"rank_feature":{"field":"package_repology_repos","boost":5,"_name":"popularity_package_repology_repos","saturation":{"pivot":13.5}}},{"bool":{"must":[{"rank_feature":{"field":"package_dep_count","sigmoid":{"pivot":7580,"exponent":0.434}}}],"should":[{"multi_match":{"type":"best_fields","query":"google","analyzer":"lowercase","fields":["package_longDescription.edge^12","package_attr_set^2.97","package_attr_name.edge^0.307"],"boost":0.128}},{"multi_match":{"type":"best_fields","query":"chrome","analyzer":"lowercase","fields":["package_longDescription.edge^12","package_attr_set^2.97","package_attr_name.edge^0.307"],"boost":0.128}}],"boost":0.364}},{"wildcard":{"package_programs":{"value":"google.chrome","boost":12.8,"case_insensitive":true}}}]}},"rescore":{"window_size":100,"query":{"rescore_query":{"function_score":{"script_score":{"script":{"source":"doc['package_attr_name'].size() == 0 ? 0 : 1.0 / doc['package_attr_name'].value.length()"}}}},"rescore_query_weight":30.3}}}"##;

    /// Elm prints an integral float as `110`, serde_json as `110.0`, and
    /// the two parse back as different `Value` variants despite being the
    /// same number to Elasticsearch. Collapse that distinction so the
    /// comparison is about the query and not about float formatting.
    fn normalize_numbers(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => {
                    serde_json::json!(f as i64)
                }
                _ => v.clone(),
            },
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(normalize_numbers).collect())
            }
            serde_json::Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), normalize_numbers(v)))
                    .collect(),
            ),
            _ => v.clone(),
        }
    }

    #[test]
    fn query_body_matches_the_real_site_request() {
        let ours = build_query(SearchMode::Packages, "google chrome");
        let theirs: serde_json::Value =
            serde_json::from_str(CAPTURED_GOOGLE_CHROME).expect("fixture parses");
        assert_eq!(
            normalize_numbers(&ours),
            normalize_numbers(&theirs),
            "generated query diverged from search.nixos.org's own request"
        );
    }

    /// `-word` is the site's exclusion syntax, peeled off before ranking.
    #[test]
    fn negated_words_become_must_not_wildcards() {
        let body = build_query(SearchMode::Packages, "python -wrapper");
        let must_not = &body["query"]["bool"]["must_not"];
        assert_eq!(must_not.as_array().map(Vec::len), Some(1), "{must_not:#?}");
        assert_eq!(
            must_not[0]["wildcard"]["package_attr_name"]["value"],
            serde_json::json!("*wrapper*")
        );
        // ...and the negated word never reaches the ranking clauses.
        let should = serde_json::to_string(&body["query"]["bool"]["should"]).unwrap();
        assert!(!should.contains("wrapper"), "{should}");
    }

    /// A word spelled with `_` is also searched with `-` and vice versa,
    /// while a word with neither contributes exactly one spelling.
    #[test]
    fn dash_underscore_variants_are_deduplicated() {
        let body = build_query(SearchMode::Packages, "ripgrep");
        let wildcards: Vec<_> = body["query"]["bool"]["must"][0]["dis_max"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|q| q.get("wildcard").is_some())
            .collect();
        assert_eq!(wildcards.len(), 1, "{wildcards:#?}");

        let body = build_query(SearchMode::Packages, "home_manager");
        let wildcards: Vec<_> = body["query"]["bool"]["must"][0]["dis_max"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|q| q["wildcard"]["package_attr_name"]["value"].as_str())
            .collect();
        assert_eq!(wildcards, vec!["*home-manager*", "*home_manager*"]);
    }

    /// Hits the real search.nixos.org backend. Not run by default since it
    /// needs network; run explicitly with `cargo test -- --ignored`.
    ///
    /// The top hits here are the ones the real site returned for this exact
    /// query, in this order, in the capture the fixture above came from —
    /// so this checks the port end to end (query *and* index) rather than
    /// just the request body.
    #[test]
    #[ignore]
    fn google_chrome_ranks_like_the_real_site() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "google chrome").unwrap();
        let top: Vec<&str> = hits.iter().take(4).map(|h| h.attr_name.as_str()).collect();
        assert_eq!(
            top,
            vec![
                "google-chrome",
                "python314Packages.google",
                "python313Packages.google",
                "chromium",
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

    /// The fuzzy clause runs on the last word, so a typo still lands —
    /// but only a single-edit one. `fuzziness: 1` is what the site sends,
    /// and the Elm's own comment advertises `fierfix` → `firefox`, which is
    /// two edits away and returns nothing from the real backend either.
    #[test]
    #[ignore]
    fn single_character_typo_still_finds_the_package() {
        let client = reqwest::blocking::Client::new();
        let hits = run_search(&client, CHANNELS[0], SearchMode::Packages, "firefx").unwrap();
        assert_eq!(hits[0].attr_name, "firefox", "{hits:#?}");
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

    /// Schema version guard: the index name carries it, and a bump on
    /// search.nixos.org's side turns every search into a 404.
    #[test]
    #[ignore]
    fn index_for_each_channel_exists() {
        let client = reqwest::blocking::Client::new();
        for channel in CHANNELS {
            let hits = run_search(&client, *channel, SearchMode::Packages, "hello").unwrap();
            assert!(!hits.is_empty(), "no results on channel {}", channel.id);
        }
    }
}
