//! WebFetch + WebSearch tools — Rust ports of `src/tools/web-fetch.ts` and
//! `web-search.ts`. HTML→markdown via a lightweight converter (turndown
//! replacement), with the same TS fallback text-stripping path preserved.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::core::types::{ToolContext, ToolResult};
use crate::tools::ToolDef;

const FETCH_TIMEOUT_MS: u64 = 30_000;
const SEARCH_TIMEOUT_MS: u64 = 15_000;
const MAX_RESULT_SIZE_CHARS: usize = 30_000;
const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;
const USER_AGENT: &str = "nanocode/1.0 (CLI Agent)";

// ---------------------------------------------------------------------------
// HTML → markdown (turndown replacement; strip fallback mirrors TS stripHtml)
// ---------------------------------------------------------------------------

pub fn html_to_markdown(html: &str) -> String {
    // Remove script/style/nav blocks
    let mut text = regex::Regex::new(r"(?is)<script[\s\S]*?</script>")
        .unwrap()
        .replace_all(html, "")
        .to_string();
    text = regex::Regex::new(r"(?is)<style[\s\S]*?</style>")
        .unwrap()
        .replace_all(&text, "")
        .to_string();
    text = regex::Regex::new(r"(?is)<nav[\s\S]*?</nav>")
        .unwrap()
        .replace_all(&text, "")
        .to_string();

    // Common element conversions
    text = regex::Regex::new(r"(?i)<br\s*/?>").unwrap().replace_all(&text, "\n").to_string();
    text = regex::Regex::new(r"(?i)</p>").unwrap().replace_all(&text, "\n\n").to_string();
    text = regex::Regex::new(r"(?i)</div>").unwrap().replace_all(&text, "\n").to_string();
    text = regex::Regex::new(r"(?i)</h[1-6]>").unwrap().replace_all(&text, "\n\n").to_string();
    text = regex::Regex::new(r#"(?is)<h([1-6])[^>]*>"#)
        .unwrap()
        .replace_all(&text, |caps: &regex::Captures| {
            "#".repeat(caps[1].parse::<usize>().unwrap_or(1)) + " "
        })
        .to_string();
    text = regex::Regex::new(r"(?i)<li[^>]*>").unwrap().replace_all(&text, "- ").to_string();
    text = regex::Regex::new(r"(?i)</li>").unwrap().replace_all(&text, "\n").to_string();

    // Strip remaining tags
    text = regex::Regex::new(r"<[^>]+>").unwrap().replace_all(&text, "").to_string();

    // Entities
    text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Whitespace cleanup
    text = regex::Regex::new(r"\n{3,}").unwrap().replace_all(&text, "\n\n").to_string();
    text = regex::Regex::new(r"[ \t]+").unwrap().replace_all(&text, " ").to_string();
    text = regex::Regex::new(r"(?m)^ +").unwrap().replace_all(&text, "").to_string();

    text.trim().to_string()
}

fn is_html(content_type: Option<&str>, body: &str) -> bool {
    if let Some(ct) = content_type {
        if ct.contains("text/html") {
            return true;
        }
    }
    let head: String = body.trim_start().chars().take(100).collect::<String>().to_lowercase();
    head.starts_with("<!doctype") || head.starts_with("<html")
}

async fn fetch_url(
    url: &str,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<(u16, Option<String>, String), String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;

    let fut = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.7",
        )
        .timeout(Duration::from_millis(timeout_ms))
        .send();

    let response = tokio::select! {
        r = fut => r.map_err(|e| e.to_string())?,
        _ = cancel.cancelled() => return Err("Aborted".into()),
    };

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = response.text().await.map_err(|e| e.to_string())?;
    Ok((status, content_type, body))
}

// ---------------------------------------------------------------------------
// WebFetch tool
// ---------------------------------------------------------------------------

pub struct WebFetchTool;

#[async_trait]
impl ToolDef for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Fetch a URL and return its content. HTML pages are converted to markdown for readability.".into()
    }

    fn input_schema(&self) -> Json {
        serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "The URL to fetch. Must be a valid HTTP or HTTPS URL."}},
            "required": ["url"]
        })
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let short: String = url.chars().take(60).collect();
        let dots = if url.chars().count() > 60 { "..." } else { "" };
        format!("WebFetch: {short}{dots}")
    }

    fn prompt(&self) -> String {
        [
            "Fetch content from a URL.",
            "",
            "Guidelines:",
            "- HTML pages are converted to markdown for readability.",
            "- Timeout: 30 seconds.",
            "- Max response size: 5 MB.",
            "- Only HTTP and HTTPS URLs are supported.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(url) = input.get("url").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: url is required.");
        };

        let parsed = reqwest::Url::parse(url);
        let is_http = parsed
            .as_ref()
            .map(|u| matches!(u.scheme(), "http" | "https"))
            .unwrap_or(false);
        if parsed.is_err() || !is_http {
            return ToolResult::err(format!("Error: invalid URL: {url}"));
        }

        match fetch_url(url, FETCH_TIMEOUT_MS, &ctx.cancel).await {
            Err(e) if e == "Aborted" => ToolResult::ok("(Aborted)"),
            Err(e) => ToolResult::err(format!("Error fetching {url}: {e}")),
            Ok((status, content_type, body)) => {
                if !(200..300).contains(&status) {
                    let status_text = match status {
                        404 => "Not Found",
                        403 => "Forbidden",
                        500 => "Internal Server Error",
                        _ => "",
                    };
                    return ToolResult::err(format!(
                        "Error: HTTP {status} {status_text} fetching {url}"
                    ));
                }

                let truncated_body: String = if body.len() > MAX_RESPONSE_SIZE {
                    body.chars().take(MAX_RESPONSE_SIZE).collect()
                } else {
                    body
                };

                let mut content = if is_html(content_type.as_deref(), &truncated_body) {
                    html_to_markdown(&truncated_body)
                } else {
                    truncated_body
                };

                if content.len() > MAX_RESULT_SIZE_CHARS {
                    content.truncate(content.char_indices().take(MAX_RESULT_SIZE_CHARS).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0));
                    content.push_str(&format!("\n\n[Content truncated at {MAX_RESULT_SIZE_CHARS} chars]"));
                }

                let header = format!(
                    "URL: {url}\nStatus: {status}\nContent-Type: {}\n---\n\n",
                    content_type.as_deref().unwrap_or("unknown")
                );
                ToolResult::ok(header + &content)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebSearch tool (DuckDuckGo HTML backend)
// ---------------------------------------------------------------------------

pub struct WebSearchTool;

fn strip_tags(html: &str) -> String {
    let tag = regex::Regex::new(r"<[^>]+>").unwrap();
    html.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .pipe(|s| tag.replace_all(&s, "").to_string())
}

trait Pipe: Sized {
    fn pipe<F: FnOnce(Self) -> Self>(self, f: F) -> Self {
        f(self)
    }
}
impl<T> Pipe for T {}

pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub fn parse_search_results(html: &str, query: &str) -> String {
    let mut results: Vec<SearchResult> = Vec::new();

    let result_block = regex::Regex::new(r#"(?is)<div[^>]*class="[^"]*result[^"]*"[^>]*>([\s\S]*?)</div>\s*</div>"#).unwrap();
    let link_re = regex::Regex::new(r#"(?i)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]*)"[^>]*>([\s\S]*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"(?i)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>([\s\S]*?)</a>"#).unwrap();
    let uddg_re = regex::Regex::new(r"uddg=([^&]+)").unwrap();

    for cap in result_block.captures_iter(html) {
        if results.len() >= 10 {
            break;
        }
        let block = &cap[1];
        let Some(link_cap) = link_re.captures(block) else { continue };
        let mut url = link_cap[1].to_string();
        if let Some(uddg) = uddg_re.captures(&url) {
            url = urldecode(&uddg[1]);
        }
        let title = strip_tags(&link_cap[2]).trim().to_string();
        let snippet = snippet_re
            .captures(block)
            .map(|c| strip_tags(&c[1]).trim().to_string())
            .unwrap_or_default();
        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult { title, url, snippet });
        }
    }

    if results.is_empty() {
        // Fallback: any external link
        let simple = regex::Regex::new(r#"(?is)<a[^>]*href="(https?://[^"]+)"[^>]*>([\s\S]*?)</a>"#).unwrap();
        let mut seen = std::collections::HashSet::new();
        for cap in simple.captures_iter(html) {
            if results.len() >= 10 {
                break;
            }
            let url = cap[1].to_string();
            let title = strip_tags(&cap[2]).trim().to_string();
            if url.contains("duckduckgo.com") || title.is_empty() || seen.contains(&url) {
                continue;
            }
            seen.insert(url.clone());
            results.push(SearchResult { title, url, snippet: String::new() });
        }
    }

    if results.is_empty() {
        return format!(
            "No search results found for: {query}\n\nTip: Try different search terms or use WebFetch to access a specific URL directly."
        );
    }

    let mut out = format!("Search results for: {query}\n\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        out.push('\n');
    }
    out.push_str(&format!("({} results)", results.len()));
    out
}

fn urldecode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 => {
                if i + 2 < bytes.len() {
                    if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[async_trait]
impl ToolDef for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Search the web for information. Returns a list of search results with titles, URLs, and snippets.".into()
    }

    fn input_schema(&self) -> Json {
        serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string", "description": "The search query. Be specific for better results."}},
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        20_000
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let short: String = q.chars().take(50).collect();
        let dots = if q.chars().count() > 50 { "..." } else { "" };
        format!("WebSearch: {short}{dots}")
    }

    fn prompt(&self) -> String {
        [
            "Search the web for information.",
            "",
            "Guidelines:",
            "- Be specific in your search queries.",
            "- Use WebFetch to read full content from search result URLs.",
            "- Returns up to 10 results.",
        ]
        .join("\n")
    }

    async fn call(&self, input: Json, ctx: &ToolContext) -> ToolResult {
        let Some(query) = input.get("query").and_then(|v| v.as_str()) else {
            return ToolResult::err("Error: search query cannot be empty.");
        };
        if query.trim().is_empty() {
            return ToolResult::err("Error: search query cannot be empty.");
        }

        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencode(query)
        );

        match fetch_url(&url, SEARCH_TIMEOUT_MS, &ctx.cancel).await {
            Err(e) if e == "Aborted" => ToolResult::ok("(Aborted)"),
            Err(e) => ToolResult::err(format!("Error performing search: {e}")),
            Ok((status, _, body)) => {
                if !(200..300).contains(&status) {
                    return ToolResult::err(format!("Error performing search: Search returned HTTP {status}"));
                }
                ToolResult::ok(parse_search_results(&body, query))
            }
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::test_ctx;

    #[test]
    fn html_to_markdown_conversions() {
        let html = "<html><head><style>x{}</style></head><body><h1>Title</h1><p>Para one</p><ul><li>item</li></ul><script>evil()</script></body></html>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Para one"));
        assert!(md.contains("- item"));
        assert!(!md.contains("evil()"));
    }

    #[test]
    fn entity_and_whitespace_cleanup() {
        let md = html_to_markdown("<p>a &amp; b</p>\n\n\n\n<p>c</p>");
        assert!(md.contains("a & b"));
        assert!(!md.contains("\n\n\n"));
    }

    fn ctx() -> ToolContext {
        test_ctx(std::path::Path::new("/tmp"))
    }

    #[tokio::test]
    async fn webfetch_validates_url() {
        let r = WebFetchTool
            .call(serde_json::json!({"url": "ftp://example.com"}), &ctx())
            .await;
        assert!(r.is_error());
        assert!(r.result.contains("invalid URL"));

        let r = WebFetchTool
            .call(serde_json::json!({"url": "not a url"}), &ctx())
            .await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn search_results_parsing() {
        let html = r#"
        <div class="result results_links">
          <div><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">Example Title</a>
          <a class="result__snippet">Example snippet text</a></div>
        </div>"#;
        let out = parse_search_results(html, "test query");
        assert!(out.contains("1. Example Title"));
        assert!(out.contains("https://example.com/page"));
        assert!(out.contains("Example snippet text"));
        assert!(out.contains("(1 results)"));
    }

    #[tokio::test]
    async fn search_no_results_message() {
        let out = parse_search_results("<div>nothing</div>", "obscure");
        assert!(out.contains("No search results found for: obscure"));
    }
}
