use async_trait::async_trait;
use legion_runtime::{Tool, ToolContext, ToolError, ToolKind, ToolNamespace, ToolResult};
use serde::Deserialize;
use serde_json::json;

use super::web::strip_html;
use crate::policy::Policy;

/// Helper to stamp `kind()` and `namespace()` on a built-in Legion tool.
macro_rules! legion_tool_taxonomy {
    ($kind:expr) => {
        fn kind(&self) -> ToolKind {
            $kind
        }
        fn namespace(&self) -> ToolNamespace {
            ToolNamespace::Legion
        }
    };
}

const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const DEFAULT_DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const DEFAULT_BING_ENDPOINT: &str = "https://www.bing.com/search";

/// A search engine backend. Only free, API-key-less engines are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Engine {
    Duckduckgo,
    Bing,
    Searxng,
}

impl Engine {
    fn label(self) -> &'static str {
        match self {
            Engine::Duckduckgo => "duckduckgo",
            Engine::Bing => "bing",
            Engine::Searxng => "searxng",
        }
    }
}

fn default_engine() -> Engine {
    Engine::Duckduckgo
}

fn default_fallback_engines() -> Vec<Engine> {
    vec![Engine::Bing]
}

/// Tool-specific configuration, parsed from the opaque `ToolConfig.extra`
/// (`tools.web_search.*` in the config file, camelCase keys).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebSearchConfig {
    /// Primary engine.
    #[serde(default = "default_engine")]
    engine: Engine,
    /// Engines tried in order after the primary one fails or returns nothing.
    #[serde(default = "default_fallback_engines")]
    fallback_engines: Vec<Engine>,
    /// Base URL of a SearXNG instance (e.g. `https://searx.example.org`).
    /// Required for the `searxng` engine; without it that engine is skipped.
    #[serde(default)]
    searxng_url: Option<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            engine: default_engine(),
            fallback_engines: default_fallback_engines(),
            searxng_url: None,
        }
    }
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Search the web across multiple engines with serial fallback.
pub struct WebSearchTool {
    pub policy: Policy,
    client: reqwest::Client,
    config: WebSearchConfig,
    ddg_endpoint: String,
    bing_endpoint: String,
}

impl WebSearchTool {
    pub fn new(policy: Policy, tool_config: Option<&legion_core::config::ToolConfig>) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
            config: parse_search_config(tool_config),
            ddg_endpoint: DEFAULT_DDG_ENDPOINT.to_string(),
            bing_endpoint: DEFAULT_BING_ENDPOINT.to_string(),
        }
    }

    /// Test-only constructor pointing the scrape endpoints at a mock server.
    #[cfg(test)]
    fn with_endpoints(
        policy: Policy,
        config: WebSearchConfig,
        ddg_endpoint: String,
        bing_endpoint: String,
    ) -> Self {
        Self {
            policy,
            client: reqwest::Client::new(),
            config,
            ddg_endpoint,
            bing_endpoint,
        }
    }

    /// Primary engine followed by deduplicated fallbacks.
    fn engine_chain(&self) -> Vec<Engine> {
        let mut chain = vec![self.config.engine];
        chain.extend(self.config.fallback_engines.iter().copied());
        chain.dedup();
        chain
    }

    async fn search_engine(
        &self,
        engine: Engine,
        query: &str,
        count: usize,
    ) -> Result<Vec<SearchResult>, String> {
        match engine {
            Engine::Duckduckgo => self.search_duckduckgo(query, count).await,
            Engine::Bing => self.search_bing(query, count).await,
            Engine::Searxng => self.search_searxng(query, count).await,
        }
    }

    /// DuckDuckGo serves an anti-bot "anomaly" challenge (HTTP 202, no
    /// results) for plain GET requests. Submitting the query as a POST form,
    /// the same way the real HTML page does, returns the standard results
    /// markup.
    async fn search_duckduckgo(
        &self,
        query: &str,
        count: usize,
    ) -> Result<Vec<SearchResult>, String> {
        let response = self
            .client
            .post(&self.ddg_endpoint)
            .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .form(&[("q", query)])
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }

        let body = response
            .text()
            .await
            .map_err(|e| format!("failed to read response: {e}"))?;

        let results = parse_ddg_results(&body, count);
        if results.is_empty()
            && let Some(reason) = detect_anti_bot_page(&body)
        {
            return Err(format!("blocked by anti-bot challenge ({reason})"));
        }
        Ok(results)
    }

    async fn search_bing(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
        let response = self
            .client
            .get(&self.bing_endpoint)
            .query(&[("q", query)])
            .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }

        let body = response
            .text()
            .await
            .map_err(|e| format!("failed to read response: {e}"))?;

        let results = parse_bing_results(&body, count);
        if results.is_empty()
            && let Some(reason) = detect_anti_bot_page(&body)
        {
            return Err(format!("blocked by anti-bot challenge ({reason})"));
        }
        Ok(results)
    }

    /// Query a user-configured SearXNG instance via its JSON API. SearXNG is a
    /// self-hostable metasearch engine; because the request goes to an
    /// instance the user controls (or a public one they trust), it sidesteps
    /// the bot-detection blocks DuckDuckGo and Bing apply to scraped requests.
    async fn search_searxng(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
        let base = self
            .config
            .searxng_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                "no searxngUrl configured; set tools.web_search.searxngUrl to a SearXNG \
                 instance base URL (e.g. https://searx.example.org)"
                    .to_string()
            })?;

        let endpoint = format!("{}/search", base.trim_end_matches('/'));
        let response = self
            .client
            .get(&endpoint)
            .query(&[("q", query), ("format", "json")])
            .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "HTTP {status} (endpoint: {endpoint}); ensure the instance has the JSON \
                 format enabled in its settings"
            ));
        }

        let parsed: SearxngResponse = response.json().await.map_err(|e| {
            format!("non-JSON response ({e}); the instance may have the JSON format disabled")
        })?;

        Ok(parsed
            .results
            .into_iter()
            .filter(|r| !r.url.trim().is_empty())
            .take(count)
            .map(|r| SearchResult {
                title: if r.title.trim().is_empty() {
                    r.url.clone()
                } else {
                    r.title
                },
                url: r.url,
                snippet: r.content.unwrap_or_default(),
            })
            .collect())
    }
}

/// Parse the web_search section of `ToolConfig.extra`, falling back to
/// defaults when absent or malformed.
fn parse_search_config(tool_config: Option<&legion_core::config::ToolConfig>) -> WebSearchConfig {
    let Some(extra) = tool_config.map(|c| &c.extra) else {
        return WebSearchConfig::default();
    };
    if !extra.is_object() {
        return WebSearchConfig::default();
    }
    match serde_json::from_value(extra.clone()) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!("invalid tools.web_search config ({e}); using defaults");
            WebSearchConfig::default()
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web (DuckDuckGo, Bing, or a configured SearXNG instance) and return a list of result snippets."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search query" },
                "count": { "type": "integer", "description": "maximum number of results (default 5)" }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    legion_tool_taxonomy!(ToolKind::WebSearch);

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("missing 'query' parameter".to_string()))?;
        let count = params["count"].as_u64().unwrap_or(5).min(10) as usize;

        let mut failures: Vec<String> = Vec::new();
        let mut empty_engines: Vec<&'static str> = Vec::new();
        for engine in self.engine_chain() {
            match self.search_engine(engine, query, count).await {
                Ok(results) if !results.is_empty() => {
                    return Ok(ToolResult::ok(format_results(&results)));
                }
                Ok(_) => empty_engines.push(engine.label()),
                Err(e) => failures.push(format!("{}: {}", engine.label(), e)),
            }
        }

        // Every engine failed: aggregate the per-engine reasons into one error.
        if !failures.is_empty() {
            let mut msg = format!("web_search: all engines failed for query \"{query}\":");
            for failure in &failures {
                msg.push_str(&format!("\n- {failure}"));
            }
            for label in &empty_engines {
                msg.push_str(&format!("\n- {label}: returned no results"));
            }
            return Err(ToolError::Execution(msg));
        }

        // Every engine returned an empty (but valid) page: give the model a
        // self-help hint instead of a bare failure.
        Ok(ToolResult::ok(format!(
            "No results found for \"{query}\" on any configured engine ({}). \
             Try rephrasing the query. If results are consistently empty, the default \
             engines may be rate-limiting this host; configure a SearXNG instance via \
             tools.web_search.searxngUrl and add \"searxng\" to \
             tools.web_search.fallbackEngines for a reliable fallback.",
            empty_engines.join(", ")
        )))
    }
}

fn format_results(results: &[SearchResult]) -> String {
    let mut out = String::new();
    for (i, result) in results.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n{}\n{}\n\n",
            i + 1,
            result.title,
            result.url,
            result.snippet
        ));
    }
    out.trim().to_string()
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: Option<String>,
}

mod search_regex {
    use regex::Regex;
    use std::sync::LazyLock;

    macro_rules! static_regex {
        ($name:ident, $pat:expr) => {
            pub fn $name() -> &'static Regex {
                static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
                &RE
            }
        };
    }

    // Anchor tags carrying class "result__a" (attrs captured separately so
    // attribute order does not matter).
    static_regex!(
        ddg_link,
        r#"(?s)<a\s+([^>]*\bclass="result__a"[^>]*)>(.*?)</a>"#
    );
    static_regex!(href, r#"href="([^"]*)""#);
    static_regex!(tag, r"<[^>]+>");
    static_regex!(
        ddg_snippet,
        r#"(?s)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#
    );
    static_regex!(
        bing_block,
        r#"(?s)<li[^>]*class="[^"]*\bb_algo\b[^"]*"[^>]*>(.*?)</li>"#
    );
    static_regex!(
        bing_link,
        r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>\s*</h2>"#
    );
    static_regex!(
        bing_caption,
        r#"(?s)<div[^>]*class="[^"]*\bb_caption\b[^"]*"[^>]*>.*?<p[^>]*>(.*?)</p>"#
    );
}

fn parse_ddg_results(html: &str, count: usize) -> Vec<SearchResult> {
    let link_re = search_regex::ddg_link();
    let href_re = search_regex::href();
    let snippet_re = search_regex::ddg_snippet();

    let links: Vec<_> = link_re.captures_iter(html).collect();
    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|cap| strip_inline(cap.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    let mut results = Vec::new();
    for (i, link) in links.iter().enumerate() {
        if results.len() >= count {
            break;
        }
        let Some(href) = href_re.captures(&link[1]) else {
            continue;
        };
        let url = decode_ddg_url(&href[1]);
        if !url.starts_with("http") || url.contains("duckduckgo.com") {
            continue;
        }
        let title = strip_inline(&link[2]);
        let snippet = snippets.get(i).cloned().unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

fn parse_bing_results(html: &str, count: usize) -> Vec<SearchResult> {
    let block_re = search_regex::bing_block();
    let link_re = search_regex::bing_link();
    let caption_re = search_regex::bing_caption();

    let mut results = Vec::new();
    for block in block_re.captures_iter(html) {
        if results.len() >= count {
            break;
        }
        let Some(link) = link_re.captures(&block[1]) else {
            continue;
        };
        let url = strip_html(&link[1]);
        if !url.starts_with("http") || url.contains("bing.com") {
            continue;
        }
        let title = strip_inline(&link[2]);
        let snippet = caption_re
            .captures(&block[1])
            .map(|cap| strip_inline(&cap[1]))
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// Strip inline markup (e.g. `<b>` highlight tags) without inserting spaces:
/// in search-result snippets tags sit flush against punctuation
/// (`<b>entry</b>.`), where `strip_html`'s tag-to-space replacement would
/// leave stray gaps. Entities are still decoded and whitespace collapsed.
fn strip_inline(html: &str) -> String {
    strip_html(&search_regex::tag().replace_all(html, ""))
}

/// DuckDuckGo wraps result URLs as `//duckduckgo.com/l/?uddg=<url-encoded>`;
/// unwrap the redirect parameter to get the real URL.
fn decode_ddg_url(url: &str) -> String {
    let Some(uddg_start) = url.find("uddg=") else {
        return url.to_string();
    };
    let start = uddg_start + 5;
    let end = url[start..]
        .find('&')
        .map(|i| start + i)
        .unwrap_or(url.len());
    percent_decode(&url[start..end])
}

/// Minimal percent-decoder for query-string values (`%XX`, `+` as space).
fn percent_decode(encoded: &str) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Detect whether an HTML body is an anti-bot/captcha challenge rather than a
/// real results page. DuckDuckGo (and similar) serve these with HTTP 200, so
/// a successful status plus zero parsed results is ambiguous without this
/// check. Returns a short human-readable reason when detected.
fn detect_anti_bot_page(html: &str) -> Option<&'static str> {
    let lowered = html.to_ascii_lowercase();
    const MARKERS: &[(&str, &str)] = &[
        ("anomaly-modal", "anomaly challenge"),
        ("anomaly.js", "anomaly challenge"),
        ("dpn=1", "anomaly challenge"),
        ("g-recaptcha", "recaptcha"),
        ("captcha", "captcha"),
        ("are you a robot", "bot check"),
        ("unusual traffic", "bot check"),
        ("verify you are human", "human verification"),
        ("challenge-platform", "cloudflare challenge"),
        ("cf-challenge", "cloudflare challenge"),
    ];
    for (needle, reason) in MARKERS {
        if lowered.contains(needle) {
            return Some(reason);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{MockServer, ResponseTemplate};

    fn ctx(dir: &TempDir, sender: Option<&str>) -> ToolContext {
        ToolContext {
            workspace: dir.path().to_path_buf(),
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            sender: sender.map(|s| s.to_string()),
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: None,
            todo_store: None,
            background_tasks: None,
            plan_mode_tracker: None,
        }
    }

    fn open_policy() -> Policy {
        Policy {
            approval: crate::policy::Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        }
    }

    /// Build a search tool whose scrape endpoints point at the given mock
    /// servers (DDG at `/html/`, Bing at `/search`).
    fn search_tool(config: WebSearchConfig, ddg: &MockServer, bing: &MockServer) -> WebSearchTool {
        WebSearchTool::with_endpoints(
            open_policy(),
            config,
            format!("{}/html/", ddg.uri()),
            format!("{}/search", bing.uri()),
        )
    }

    fn ddg_only_tool(ddg: &MockServer, bing: &MockServer) -> WebSearchTool {
        let config = WebSearchConfig {
            engine: Engine::Duckduckgo,
            fallback_engines: vec![],
            searxng_url: None,
        };
        search_tool(config, ddg, bing)
    }

    const DDG_RESULTS_HTML: &str = r#"
        <div class="result results_links results_links_deep web-result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc123"><b>Rust</b> Language</a>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">A <b>systems</b> programming language.</a>
        </div>
        <div class="result results_links results_links_deep web-result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust">Rust on Wikipedia</a>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust">Encyclopedia <b>entry</b>.</a>
        </div>
        <div class="result results_links results_links_deep web-result">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/feedback">Feedback</a>
          <a class="result__snippet" href="https://duckduckgo.com/feedback">Send feedback.</a>
        </div>
    "#;

    const DDG_ANOMALY_HTML: &str = r#"<!DOCTYPE html><html><head>
        <script src="/dist/anomaly.js"></script></head>
        <body><div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>
        </body></html>"#;

    const BING_RESULTS_HTML: &str = r#"
        <ol id="b_results">
          <li class="b_algo">
            <h2><a href="https://example.com/rust">Rust &amp; Cargo</a></h2>
            <div class="b_caption"><p>A <strong>systems</strong> language.</p></div>
          </li>
          <li class="b_algo"><h2><a href="https://www.bing.com/aclk">ad</a></h2></li>
          <li class="b_algo">
            <h2><a href="https://example.org/legion">Legion</a></h2>
            <div class="b_caption"><p>Agentic coding.</p></div>
          </li>
        </ol>
    "#;

    #[test]
    fn web_search_is_read_only_and_concurrency_safe() {
        let search = WebSearchTool::new(open_policy(), None);
        assert!(search.is_read_only(&json!({"query": "x"})));
        assert!(search.is_concurrency_safe(&json!({"query": "x"})));
    }

    // -----------------------------------------------------------------------
    // Pure parser / helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn parses_ddg_html_and_unwraps_uddg_urls() {
        let results = parse_ddg_results(DDG_RESULTS_HTML, 10);
        assert_eq!(
            results.len(),
            2,
            "duckduckgo.com self-link must be filtered"
        );
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[0].snippet, "A systems programming language.");
        assert_eq!(results[1].url, "https://en.wikipedia.org/wiki/Rust");
        assert_eq!(results[1].snippet, "Encyclopedia entry.");
    }

    #[test]
    fn parses_bing_html_results() {
        let results = parse_bing_results(BING_RESULTS_HTML, 10);
        assert_eq!(results.len(), 2, "bing.com self-link must be filtered");
        assert_eq!(results[0].title, "Rust & Cargo");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].snippet, "A systems language.");
        assert_eq!(results[1].title, "Legion");
        assert_eq!(results[1].snippet, "Agentic coding.");
    }

    #[test]
    fn percent_decode_handles_hex_and_plus() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa+b%3Fx%3D1"),
            "https://example.com/a b?x=1"
        );
        // Truncated/invalid escapes are passed through untouched.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn detects_anti_bot_markers() {
        assert_eq!(
            detect_anti_bot_page(DDG_ANOMALY_HTML),
            Some("anomaly challenge")
        );
        assert_eq!(
            detect_anti_bot_page("<div class=\"g-recaptcha\"></div>"),
            Some("recaptcha")
        );
        assert_eq!(
            detect_anti_bot_page("<p>Please verify you are human.</p>"),
            Some("human verification")
        );
        assert_eq!(detect_anti_bot_page(DDG_RESULTS_HTML), None);
        assert_eq!(detect_anti_bot_page(BING_RESULTS_HTML), None);
    }

    #[test]
    fn engine_chain_defaults_and_dedup() {
        let tool = WebSearchTool::with_endpoints(
            open_policy(),
            WebSearchConfig::default(),
            "http://x".into(),
            "http://y".into(),
        );
        assert_eq!(tool.engine_chain(), vec![Engine::Duckduckgo, Engine::Bing]);

        let config = WebSearchConfig {
            engine: Engine::Bing,
            fallback_engines: vec![Engine::Bing, Engine::Searxng],
            searxng_url: Some("http://z".into()),
        };
        let tool = WebSearchTool::with_endpoints(
            open_policy(),
            config,
            "http://x".into(),
            "http://y".into(),
        );
        assert_eq!(tool.engine_chain(), vec![Engine::Bing, Engine::Searxng]);
    }

    // -----------------------------------------------------------------------
    // Engine end-to-end tests against wiremock
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ddg_search_returns_formatted_results() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        ddg.register(
            wiremock::Mock::given(method("POST"))
                .and(path("/html/"))
                .respond_with(ResponseTemplate::new(200).set_body_string(DDG_RESULTS_HTML)),
        )
        .await;

        let dir = TempDir::new().unwrap();
        let tool = ddg_only_tool(&ddg, &bing);
        let res = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("1. Rust Language"));
        assert!(res.content.contains("https://www.rust-lang.org/"));
        assert!(res.content.contains("2. Rust on Wikipedia"));
        assert!(!res.content.contains("duckduckgo.com/feedback"));
    }

    #[tokio::test]
    async fn bing_search_returns_formatted_results() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        bing.register(
            wiremock::Mock::given(method("GET"))
                .and(path("/search"))
                .respond_with(ResponseTemplate::new(200).set_body_string(BING_RESULTS_HTML)),
        )
        .await;

        let dir = TempDir::new().unwrap();
        let config = WebSearchConfig {
            engine: Engine::Bing,
            fallback_engines: vec![],
            searxng_url: None,
        };
        let tool = search_tool(config, &ddg, &bing);
        let res = tool
            .execute(json!({"query": "rust", "count": 5}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("1. Rust & Cargo"));
        assert!(res.content.contains("https://example.com/rust"));
        assert!(res.content.contains("2. Legion"));
        assert!(!res.content.contains("bing.com/aclk"));
    }

    #[tokio::test]
    async fn searxng_search_parses_json_results() {
        let ddg = MockServer::start().await;
        let searxng = MockServer::start().await;
        searxng
            .register(
                wiremock::Mock::given(method("GET"))
                    .and(path("/search"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "query": "rust",
                        "results": [
                            {
                                "url": "https://www.rust-lang.org/",
                                "title": "Rust Programming Language",
                                "content": "A language empowering everyone."
                            },
                            { "url": "", "title": "junk" },
                            { "url": "https://crates.io", "title": "" }
                        ]
                    }))),
            )
            .await;

        let dir = TempDir::new().unwrap();
        let config = WebSearchConfig {
            engine: Engine::Searxng,
            fallback_engines: vec![],
            searxng_url: Some(searxng.uri()),
        };
        let tool = search_tool(config, &ddg, &searxng);
        let res = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("1. Rust Programming Language"));
        assert!(res.content.contains("A language empowering everyone."));
        // Empty-URL entry dropped; empty title falls back to the URL.
        assert!(!res.content.contains("junk"));
        assert!(res.content.contains("2. https://crates.io"));
    }

    // -----------------------------------------------------------------------
    // Failure handling and fallback chain
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn non_2xx_status_fails_engine_with_status_in_error() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        ddg.register(
            wiremock::Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable")),
        )
        .await;

        let dir = TempDir::new().unwrap();
        let tool = ddg_only_tool(&ddg, &bing);
        let err = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("duckduckgo"), "engine named in: {msg}");
        assert!(msg.contains("503"), "status code in: {msg}");
    }

    #[tokio::test]
    async fn anti_bot_page_fails_engine_and_falls_back_to_bing() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        // DDG answers 200 with an anomaly challenge page (no results).
        ddg.register(
            wiremock::Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(DDG_ANOMALY_HTML)),
        )
        .await;
        bing.register(
            wiremock::Mock::given(method("GET"))
                .and(path("/search"))
                .respond_with(ResponseTemplate::new(200).set_body_string(BING_RESULTS_HTML)),
        )
        .await;

        let dir = TempDir::new().unwrap();
        // Default chain: duckduckgo -> bing.
        let tool = search_tool(WebSearchConfig::default(), &ddg, &bing);
        let res = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(
            res.content.contains("1. Rust & Cargo"),
            "expected bing fallback results, got: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn all_engines_fail_returns_aggregated_error() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        ddg.register(
            wiremock::Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(202).set_body_string(DDG_ANOMALY_HTML)),
        )
        .await;
        bing.register(
            wiremock::Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(500).set_body_string("boom")),
        )
        .await;

        let dir = TempDir::new().unwrap();
        let tool = search_tool(WebSearchConfig::default(), &ddg, &bing);
        let err = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("duckduckgo"), "ddg failure listed in: {msg}");
        assert!(
            msg.contains("anti-bot challenge"),
            "ddg anomaly page reported in: {msg}"
        );
        assert!(msg.contains("bing"), "bing failure listed in: {msg}");
        assert!(msg.contains("500"), "bing status listed in: {msg}");
    }

    #[tokio::test]
    async fn all_engines_empty_returns_self_help_hint() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        // Valid 200 pages with no results and no anti-bot markers.
        ddg.register(
            wiremock::Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>")),
        )
        .await;
        bing.register(wiremock::Mock::given(method("GET")).respond_with(
            ResponseTemplate::new(200).set_body_string("<ol id=\"b_results\"></ol>"),
        ))
        .await;

        let dir = TempDir::new().unwrap();
        let tool = search_tool(WebSearchConfig::default(), &ddg, &bing);
        let res = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(res.content.contains("No results found"));
        assert!(res.content.contains("duckduckgo"));
        assert!(res.content.contains("bing"));
        assert!(
            res.content.contains("searxngUrl"),
            "hint should suggest SearXNG: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn searxng_without_url_is_skipped_and_next_engine_used() {
        let ddg = MockServer::start().await;
        let bing = MockServer::start().await;
        bing.register(
            wiremock::Mock::given(method("GET"))
                .and(path("/search"))
                .respond_with(ResponseTemplate::new(200).set_body_string(BING_RESULTS_HTML)),
        )
        .await;

        let dir = TempDir::new().unwrap();
        let config = WebSearchConfig {
            engine: Engine::Searxng,
            fallback_engines: vec![Engine::Bing],
            searxng_url: None,
        };
        let tool = search_tool(config, &ddg, &bing);
        let res = tool
            .execute(json!({"query": "rust"}), ctx(&dir, None))
            .await
            .unwrap();

        assert!(
            res.content.contains("1. Rust & Cargo"),
            "expected bing results after searxng was skipped, got: {}",
            res.content
        );
    }
}
