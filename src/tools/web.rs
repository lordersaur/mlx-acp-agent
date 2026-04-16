use std::env;
use std::process::Command;

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::Client;

// ---------------------------------------------------------------------------
// Constants (mirror web.py)
// ---------------------------------------------------------------------------

const MAX_FETCH_BYTES: usize = 750_000;
const MAX_FETCH_OUTPUT_CHARS: usize = 8000;
const MAX_SEARCH_RESULTS: usize = 10;
const MAX_SNIPPET_CHARS: usize = 320;
const USER_AGENT: &str = "mlx-acp-agent/0.1";
const DUCKDUCKGO_HTML_URL: &str = "https://html.duckduckgo.com/html/";
const DEFAULT_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn fetch_url(url: &str) -> Result<String> {
    let normalized = normalize_url(url)?;
    let (final_url, status, content_type, body, truncated) = http_get(&normalized).await?;
    let rendered = render_response_body(&final_url, &content_type, &body);
    let rendered = rendered.trim();

    let mut lines = vec![
        format!("URL: {final_url}"),
        format!("Status: {status}"),
        format!(
            "Content-Type: {}",
            if content_type.is_empty() {
                "unknown"
            } else {
                &content_type
            }
        ),
    ];
    if truncated {
        lines.push(format!(
            "Note: response body truncated after {MAX_FETCH_BYTES} bytes."
        ));
    }
    lines.push(String::new());
    lines.push(if rendered.is_empty() {
        "No readable text found.".to_owned()
    } else {
        rendered.to_owned()
    });

    let result = lines.join("\n");
    Ok(truncate_str(result.trim(), MAX_FETCH_OUTPUT_CHARS))
}

pub async fn search_web(query: &str, max_results: usize) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("Query is required");
    }
    let limit = max_results.clamp(1, MAX_SEARCH_RESULTS);
    let mut errors = Vec::new();

    match search_duckduckgo(query, limit).await {
        Ok((backend, results)) => {
            return Ok(format_search_results(query, &backend, &results));
        }
        Err(e) => errors.push(e.to_string()),
    }

    let searxng_url = env::var("SEARXNG_BASE_URL")
        .or_else(|_| env::var("SEARXNG_URL"))
        .unwrap_or_default();
    if !searxng_url.trim().is_empty() {
        match search_searxng(query, limit, searxng_url.trim()).await {
            Ok((backend, results)) => {
                return Ok(format_search_results(query, &backend, &results));
            }
            Err(e) => errors.push(e.to_string()),
        }
    } else {
        errors.push("SearXNG is not configured".to_owned());
    }

    let api_key = env::var("GOOGLE_CUSTOM_SEARCH_API_KEY").unwrap_or_default();
    let cx = env::var("GOOGLE_CUSTOM_SEARCH_CX").unwrap_or_default();
    if !api_key.trim().is_empty() && !cx.trim().is_empty() {
        match search_google(query, limit, api_key.trim(), cx.trim()).await {
            Ok((backend, results)) => {
                return Ok(format_search_results(query, &backend, &results));
            }
            Err(e) => errors.push(e.to_string()),
        }
    } else {
        errors.push("Google Custom Search is not configured".to_owned());
    }

    bail!("{}", errors.join("; "))
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

fn build_client() -> Result<Client> {
    Client::builder()
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("failed to build reqwest client")
}

/// Returns (final_url, status_code, content_type, body_bytes, truncated).
async fn http_get(url: &str) -> Result<(String, u16, String, Vec<u8>, bool)> {
    let client = build_client()?;
    let accept = "text/html,application/xhtml+xml,application/json,text/plain,text/markdown,\
                  application/xml,text/xml;q=0.9,*/*;q=0.5";

    let response = client
        .get(url)
        .header("Accept", accept)
        .send()
        .await
        .or_else(|_| {
            // Return the error; curl fallback happens below
            Err(anyhow::anyhow!("reqwest failed"))
        });

    match response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if resp.status().is_client_error() || resp.status().is_server_error() {
                bail!("HTTP {status} for {url}");
            }
            let final_url = resp.url().to_string();
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();

            let bytes = resp.bytes().await.context("failed to read response body")?;
            let (body, truncated) = if bytes.len() > MAX_FETCH_BYTES {
                (bytes[..MAX_FETCH_BYTES].to_vec(), true)
            } else {
                (bytes.to_vec(), false)
            };

            Ok((final_url, status, content_type, body, truncated))
        }
        Err(_) => curl_get(url, accept),
    }
}

fn curl_get(url: &str, accept: &str) -> Result<(String, u16, String, Vec<u8>, bool)> {
    let marker = "\n__MLX_ACP_META__\n";
    let output = Command::new("curl")
        .args([
            "-L",
            "--compressed",
            "--max-time",
            &DEFAULT_TIMEOUT_SECS.to_string(),
            "-A",
            USER_AGENT,
            "-H",
            &format!("Accept: {accept}"),
            "-sS",
            url,
            "-w",
            &format!("{marker}%{{http_code}}\n%{{content_type}}\n%{{url_effective}}\n"),
        ])
        .output()
        .context("curl is not installed or failed to run")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!("{}", stderr);
    }

    let marker_bytes = marker.as_bytes();
    let pos = output
        .stdout
        .windows(marker_bytes.len())
        .rposition(|w| w == marker_bytes)
        .context("curl did not return response metadata")?;

    let body = output.stdout[..pos].to_vec();
    let meta = String::from_utf8_lossy(&output.stdout[pos + marker_bytes.len()..]);
    let mut meta_lines = meta.lines();
    let status: u16 = meta_lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let content_type = meta_lines.next().unwrap_or("").trim().to_owned();
    let final_url = meta_lines
        .next()
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| url.to_owned());

    if status >= 400 || status == 0 {
        bail!("HTTP {status} for {final_url}");
    }

    let (body, truncated) = if body.len() > MAX_FETCH_BYTES {
        (body[..MAX_FETCH_BYTES].to_vec(), true)
    } else {
        (body, false)
    };

    Ok((final_url, status, content_type, body, truncated))
}

// ---------------------------------------------------------------------------
// URL normalisation
// ---------------------------------------------------------------------------

fn normalize_url(url: &str) -> Result<String> {
    let candidate = url.trim().to_owned();
    if candidate.is_empty() {
        bail!("URL is required");
    }
    let candidate = if !candidate.contains("://") {
        format!("https://{candidate}")
    } else {
        candidate
    };
    if !candidate.starts_with("http://") && !candidate.starts_with("https://") {
        bail!("Only http and https URLs are supported");
    }
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// Response body rendering
// ---------------------------------------------------------------------------

fn render_response_body(final_url: &str, content_type: &str, body: &[u8]) -> String {
    let lowered = content_type.to_lowercase();
    let url_lower = final_url.to_lowercase();

    if lowered.contains("html") || url_lower.ends_with(".html") || url_lower.ends_with(".htm") {
        let text = decode_bytes(body, content_type);
        return extract_html_text(&text);
    }
    if lowered.contains("json") || url_lower.ends_with(".json") {
        let text = decode_bytes(body, content_type);
        return format_json(&text);
    }
    if lowered.starts_with("text/")
        || lowered.contains("xml")
        || lowered.contains("markdown")
        || content_type.is_empty()
        || looks_like_text(body)
    {
        return decode_bytes(body, content_type);
    }
    format!(
        "Unsupported content type: {} ({} bytes).",
        if content_type.is_empty() {
            "unknown"
        } else {
            content_type
        },
        body.len()
    )
}

fn decode_bytes(body: &[u8], content_type: &str) -> String {
    let charset = charset_from_content_type(content_type);
    // Attempt the declared charset; fall back to utf-8 lossy.
    let _ = charset; // We always use utf-8 lossy for simplicity
    String::from_utf8_lossy(body).into_owned()
}

fn charset_from_content_type(content_type: &str) -> String {
    let re = Regex::new(r"(?i)charset=([^\s;]+)").unwrap();
    re.captures(content_type)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim_matches('"').trim_matches('\'').to_owned())
        .unwrap_or_else(|| "utf-8".to_owned())
}

fn looks_like_text(body: &[u8]) -> bool {
    if body.is_empty() {
        return true;
    }
    let sample = &body[..body.len().min(2048)];
    if sample.contains(&0u8) {
        return false;
    }
    let control = sample
        .iter()
        .filter(|&&b| b < 9 || (13 < b && b < 32 && b != 27))
        .count();
    (control as f64) / (sample.len() as f64) < 0.05
}

fn format_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_owned())
}

// ---------------------------------------------------------------------------
// HTML text extraction (mirrors ReadableHTMLParser in web.py)
// ---------------------------------------------------------------------------

const HTML_SKIP_TAGS: &[&str] = &["canvas", "noscript", "script", "style", "svg", "template"];

const HTML_BLOCK_TAGS: &[&str] = &[
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "tr",
    "ul",
];

fn extract_html_text(html: &str) -> String {
    // --- Extract <title> ---
    let title_re = Regex::new(r"(?si)<title[^>]*>(.*?)</title>").unwrap();
    let title = title_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| normalize_whitespace(&decode_entities(m.as_str())))
        .unwrap_or_default();

    // --- Extract meta description ---
    let desc_re = Regex::new(
        r#"(?si)<meta[^>]+(?:name=["']description["'][^>]+content=["']([^"']+)["']|content=["']([^"']+)["'][^>]+name=["']description["'])[^>]*>"#,
    )
    .unwrap();
    let og_desc_re = Regex::new(
        r#"(?si)<meta[^>]+(?:property=["']og:description["'][^>]+content=["']([^"']+)["']|content=["']([^"']+)["'][^>]+property=["']og:description["'])[^>]*>"#,
    )
    .unwrap();
    let description = desc_re
        .captures(html)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .or_else(|| {
            og_desc_re
                .captures(html)
                .and_then(|c| c.get(1).or_else(|| c.get(2)))
        })
        .map(|m| normalize_whitespace(&decode_entities(m.as_str())))
        .unwrap_or_default();

    // --- Remove skip-tag content ---
    let mut body = html.to_owned();
    for tag in HTML_SKIP_TAGS {
        let re = Regex::new(&format!(r"(?si)<{tag}(?:\s[^>]*)?>.*?</{tag}>")).unwrap();
        body = re.replace_all(&body, " ").into_owned();
    }

    // --- Add newlines around block tags ---
    let block_alt = HTML_BLOCK_TAGS.join("|");
    let open_block_re = Regex::new(&format!(r"(?i)<({block_alt})(?:\s[^>]*)?>")).unwrap();
    let close_block_re = Regex::new(&format!(r"(?i)</({block_alt})>")).unwrap();
    body = open_block_re.replace_all(&body, "\n").into_owned();
    body = close_block_re.replace_all(&body, "\n").into_owned();

    // --- Strip remaining tags ---
    let tag_re = Regex::new(r"<[^>]+>").unwrap();
    body = tag_re.replace_all(&body, "").into_owned();

    // --- Decode entities and normalise whitespace ---
    body = normalize_whitespace(&decode_entities(&body));

    // --- Assemble sections ---
    let mut sections: Vec<String> = Vec::new();
    if !title.is_empty() {
        sections.push(format!("Title: {title}"));
    }
    if !description.is_empty() && !body.to_lowercase().contains(&description.to_lowercase()) {
        sections.push(format!("Description: {description}"));
    }
    if !body.is_empty() {
        sections.push(body);
    }
    sections.join("\n\n")
}

fn normalize_whitespace(text: &str) -> String {
    let text = decode_entities(text);
    // Replace nbsp
    let text = text.replace('\u{00a0}', " ");
    // Collapse horizontal whitespace (keep newlines)
    let re_hws = Regex::new(r"[ \t\r\x0c\x0b]+").unwrap();
    let text = re_hws.replace_all(&text, " ").into_owned();
    // Trim spaces around newlines
    let re_sp_nl = Regex::new(r" *\n *").unwrap();
    let text = re_sp_nl.replace_all(&text, "\n").into_owned();
    // Collapse 3+ consecutive newlines
    let re_multi_nl = Regex::new(r"\n{3,}").unwrap();
    let text = re_multi_nl.replace_all(&text, "\n\n").into_owned();
    text.trim().to_owned()
}

fn decode_entities(text: &str) -> String {
    // Named entities
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&hellip;", "…");
    // Decimal numeric entities &#123;
    let dec_re = Regex::new(r"&#(\d+);").unwrap();
    let text = dec_re
        .replace_all(&text, |caps: &regex::Captures<'_>| {
            let n: u32 = caps[1].parse().unwrap_or(0);
            char::from_u32(n).map(|c| c.to_string()).unwrap_or_default()
        })
        .into_owned();
    // Hex numeric entities &#x1F;
    let hex_re = Regex::new(r"&#x([0-9a-fA-F]+);").unwrap();
    hex_re
        .replace_all(&text, |caps: &regex::Captures<'_>| {
            let n = u32::from_str_radix(&caps[1], 16).unwrap_or(0);
            char::from_u32(n).map(|c| c.to_string()).unwrap_or_default()
        })
        .into_owned()
}

fn strip_html_tags(fragment: &str) -> String {
    let re = Regex::new(r"<[^>]+>").unwrap();
    normalize_whitespace(&decode_entities(&re.replace_all(fragment, " ")))
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML search (mirrors _search_duckduckgo in web.py)
// ---------------------------------------------------------------------------

async fn search_duckduckgo(query: &str, max_results: usize) -> Result<(String, Vec<SearchResult>)> {
    let client = build_client()?;
    let url = format!("{DUCKDUCKGO_HTML_URL}?q={}", urlencoded(query));

    let html = client
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .context("DuckDuckGo search request failed")?
        .text()
        .await
        .context("DuckDuckGo response read failed")?;

    let results = parse_duckduckgo_results(&html);
    if results.is_empty() {
        bail!("DuckDuckGo search returned no parseable results");
    }
    Ok((
        "DuckDuckGo HTML".to_owned(),
        results.into_iter().take(max_results).collect(),
    ))
}

fn parse_duckduckgo_results(html: &str) -> Vec<SearchResult> {
    // Primary: find <a class="result__a" href="...">title</a>
    let anchor_re =
        Regex::new(r#"(?si)<a[^>]+class="[^"]*result__a[^"]*"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#)
            .unwrap();
    let snippet_re = Regex::new(
        r#"(?si)<(?:a|div|span)[^>]+class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</(?:a|div|span)>"#,
    )
    .unwrap();

    let anchors: Vec<(String, String)> = anchor_re
        .captures_iter(html)
        .map(|c| {
            (
                resolve_ddg_url(c.get(1).map(|m| m.as_str()).unwrap_or("")),
                strip_html_tags(c.get(2).map(|m| m.as_str()).unwrap_or("")),
            )
        })
        .filter(|(url, title)| !url.is_empty() && !title.is_empty())
        .collect();

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|c| strip_html_tags(c.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    anchors
        .into_iter()
        .enumerate()
        .map(|(i, (url, title))| SearchResult {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

fn resolve_ddg_url(href: &str) -> String {
    if href.is_empty() {
        return String::new();
    }
    // DuckDuckGo redirect URLs look like /l/?uddg=<encoded_url>
    if href.contains("duckduckgo.com/l/") || href.starts_with("/l/") {
        if let Some(pos) = href.find("uddg=") {
            let encoded = &href[pos + 5..];
            let end = encoded.find('&').unwrap_or(encoded.len());
            if let Ok(decoded) = urlencoding_decode(&encoded[..end]) {
                return decoded;
            }
        }
    }
    if href.starts_with('/') {
        format!("https://duckduckgo.com{href}")
    } else {
        href.to_owned()
    }
}

fn urlencoding_decode(s: &str) -> Result<String> {
    let s = s.replace('+', " ");
    let re = Regex::new(r"%([0-9a-fA-F]{2})").unwrap();
    let result = re.replace_all(&s, |caps: &regex::Captures<'_>| {
        u8::from_str_radix(&caps[1], 16)
            .ok()
            .map(|b| (b as char).to_string())
            .unwrap_or_default()
    });
    Ok(result.into_owned())
}

fn urlencoded(s: &str) -> String {
    // Simple percent-encode for query strings
    s.chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || "-_.~".contains(c) {
                vec![c.to_string()]
            } else if c == ' ' {
                vec!["+".to_owned()]
            } else {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                encoded
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect::<Vec<_>>()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SearXNG search (mirrors _searxng_search in web.py)
// ---------------------------------------------------------------------------

async fn search_searxng(
    query: &str,
    max_results: usize,
    base_url: &str,
) -> Result<(String, Vec<SearchResult>)> {
    let endpoint = {
        let trimmed = base_url.trim_end_matches('/');
        if trimmed.ends_with("/search") {
            trimmed.to_owned()
        } else {
            format!("{trimmed}/search")
        }
    };

    let client = build_client()?;
    let url = format!("{endpoint}?q={}&format=json", urlencoded(query));
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("SearXNG search request failed")?;

    if !response.status().is_success() {
        bail!(
            "SearXNG search failed with HTTP {}",
            response.status().as_u16()
        );
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .context("SearXNG search returned invalid JSON")?;

    let results: Vec<SearchResult> = payload
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let title = item.get("title")?.as_str()?.trim().to_owned();
                    let url = item.get("url")?.as_str()?.trim().to_owned();
                    if title.is_empty() || url.is_empty() {
                        return None;
                    }
                    let snippet = item
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    Some(SearchResult {
                        title,
                        url,
                        snippet,
                    })
                })
                .take(max_results)
                .collect()
        })
        .unwrap_or_default();

    if results.is_empty() {
        bail!("SearXNG search returned no results");
    }
    Ok(("SearXNG".to_owned(), results))
}

// ---------------------------------------------------------------------------
// Google Custom Search (mirrors _google_custom_search in web.py)
// ---------------------------------------------------------------------------

async fn search_google(
    query: &str,
    max_results: usize,
    api_key: &str,
    cx: &str,
) -> Result<(String, Vec<SearchResult>)> {
    let client = build_client()?;
    let url = format!(
        "https://www.googleapis.com/customsearch/v1?key={api_key}&cx={cx}&q={}&num={max_results}",
        urlencoded(query),
    );
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Google Custom Search request failed")?;

    if !response.status().is_success() {
        bail!(
            "Google Custom Search failed with HTTP {}",
            response.status().as_u16()
        );
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .context("Google Custom Search returned invalid JSON")?;

    let results: Vec<SearchResult> = payload
        .get("items")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let title = item.get("title")?.as_str()?.trim().to_owned();
                    let url = item.get("link")?.as_str()?.trim().to_owned();
                    if title.is_empty() || url.is_empty() {
                        return None;
                    }
                    let snippet = item
                        .get("snippet")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    Some(SearchResult {
                        title,
                        url,
                        snippet,
                    })
                })
                .take(max_results)
                .collect()
        })
        .unwrap_or_default();

    if results.is_empty() {
        bail!("Google Custom Search returned no results");
    }
    Ok(("Google Custom Search".to_owned(), results))
}

// ---------------------------------------------------------------------------
// Result formatting (mirrors _format_search_results in web.py)
// ---------------------------------------------------------------------------

fn format_search_results(query: &str, backend: &str, results: &[SearchResult]) -> String {
    let mut lines = vec![
        format!("Web search results for: {query}"),
        format!("Backend: {backend}"),
        String::new(),
    ];
    for (i, result) in results.iter().enumerate() {
        lines.push(format!("{}. {}", i + 1, result.title));
        lines.push(format!("URL: {}", result.url));
        if !result.snippet.is_empty() {
            lines.push(format!(
                "Snippet: {}",
                truncate_str(&result.snippet, MAX_SNIPPET_CHARS)
            ));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_owned()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn truncate_str(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let omitted = text.len() - limit;
    // Find a char boundary
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [truncated {omitted} chars]", &text[..end])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{decode_entities, extract_html_text, normalize_whitespace, resolve_ddg_url};

    #[test]
    fn decode_entities_handles_common_cases() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&#65;"), "A");
        assert_eq!(decode_entities("&#x41;"), "A");
    }

    #[test]
    fn normalize_whitespace_collapses_space_and_newlines() {
        let input = "  hello   world  \n\n\n  foo  ";
        let result = normalize_whitespace(input);
        assert_eq!(result, "hello world\n\nfoo");
    }

    #[test]
    fn extract_html_text_picks_up_title_and_body() {
        let html = r#"
            <html>
            <head>
                <title>My Page</title>
                <meta name="description" content="A test page.">
                <script>alert('x')</script>
                <style>body { color: red }</style>
            </head>
            <body>
                <h1>Hello</h1>
                <p>World</p>
            </body>
            </html>
        "#;
        let result = extract_html_text(html);
        assert!(result.contains("Title: My Page"), "missing title: {result}");
        assert!(result.contains("Hello"), "missing h1: {result}");
        assert!(result.contains("World"), "missing p: {result}");
        assert!(!result.contains("alert"), "script not stripped: {result}");
        assert!(!result.contains("color:"), "style not stripped: {result}");
    }

    #[test]
    fn resolve_ddg_url_handles_redirect_links() {
        let href = "/l/?uddg=https%3A%2F%2Fexample.com&rut=abc";
        let result = resolve_ddg_url(href);
        assert_eq!(result, "https://example.com");
    }
}
