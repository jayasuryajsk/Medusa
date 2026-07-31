//! Model-facing web tools: `web_fetch` (fetch a page as readable text) and
//! `web_search` (DuckDuckGo HTML results). Both are side-effect-free on the
//! workspace, but they are outbound network egress and are therefore routed
//! through the permission gate (see `ToolRuntime::web_fetch`/`web_search`):
//! auto-allowed in Open mode, approval-gated in Guarded/Ask/Readonly.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use regex::Regex;
use url::{Host, Url};

/// Default character cap applied to `web_fetch` output.
pub const WEB_FETCH_DEFAULT_MAX_CHARS: usize = 12_000;
/// Default result count for `web_search`.
pub const WEB_SEARCH_DEFAULT_COUNT: usize = 8;
/// Hard result cap for `web_search`.
pub const WEB_SEARCH_MAX_COUNT: usize = 10;

/// Raw response bodies are never read past this many bytes.
const BODY_READ_CAP: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "medusa-tui/0.1.0";

#[derive(Debug, Clone)]
pub struct WebFetchRequest {
    pub url: String,
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct WebSearchRequest {
    pub query: String,
    pub count: Option<usize>,
}

/// Fetch a public http(s) URL and return `url:`/`status:` header lines
/// followed by readable text (HTML is reduced to text).
pub fn web_fetch(request: &WebFetchRequest) -> Result<String> {
    let url =
        Url::parse(request.url.trim()).wrap_err_with(|| format!("invalid URL: {}", request.url))?;
    check_url_allowed(&url).map_err(|reason| eyre!(reason))?;

    let response = web_client()?
        .get(url)
        .send()
        .wrap_err_with(|| format!("failed to fetch {}", request.url))?;

    let final_url = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (body, body_truncated) = read_body_capped(response)?;
    let is_html = content_type.contains("text/html") || content_type.contains("application/xhtml");
    // Plain text / JSON pass through untouched; only HTML gets the readability pass.
    let mut content = if is_html { html_to_text(&body) } else { body };
    if body_truncated {
        content.push_str("\n… body truncated at 2 MB");
    }

    let max_chars = request
        .max_chars
        .unwrap_or(WEB_FETCH_DEFAULT_MAX_CHARS)
        .max(1);
    Ok(format!(
        "url: {final_url}\nstatus: {status}\n\n{}",
        cap_chars(&content, max_chars)
    ))
}

/// Search DuckDuckGo's HTML endpoint and return "title — url" entries with
/// snippets. Zero parsed results on a 200 response is reported honestly
/// rather than treated as an error.
pub fn web_search(request: &WebSearchRequest) -> Result<String> {
    let query = request.query.trim();
    if query.is_empty() {
        bail!("web_search.query cannot be empty");
    }
    let count = request
        .count
        .unwrap_or(WEB_SEARCH_DEFAULT_COUNT)
        .clamp(1, WEB_SEARCH_MAX_COUNT);

    let response = web_client()?
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .wrap_err("failed to reach DuckDuckGo")?;

    let status = response.status();
    let (body, _) = read_body_capped(response)?;
    if !status.is_success() {
        bail!("web search failed: HTTP {status}");
    }

    Ok(format_search_results(
        query,
        &parse_ddg_results(&body, count),
    ))
}

pub(crate) fn format_search_results(query: &str, hits: &[SearchHit]) -> String {
    let mut output = format!("query: {query}\nresults: {}\n", hits.len());
    if hits.is_empty() {
        output.push_str(
            "\nno results parsed — DDG layout may have changed; try web_fetch on a likely page instead.\n",
        );
        return output;
    }
    for (index, hit) in hits.iter().enumerate() {
        output.push_str(&format!("\n{}. {} — {}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            output.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    output
}

fn web_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            // Every hop is re-resolved and re-checked: a public URL that
            // 302-redirects to an internal name/IP is blocked here.
            match redirect_decision(attempt.previous().len(), attempt.url()) {
                RedirectDecision::Follow => attempt.follow(),
                RedirectDecision::Stop(reason) | RedirectDecision::Block(reason) => {
                    attempt.error(std::io::Error::other(reason))
                }
            }
        }))
        .build()
        .wrap_err("failed to build web client")
}

fn read_body_capped(response: reqwest::blocking::Response) -> Result<(String, bool)> {
    let mut buffer = Vec::new();
    response
        .take(BODY_READ_CAP as u64 + 1)
        .read_to_end(&mut buffer)
        .wrap_err("failed reading response body")?;
    let truncated = buffer.len() > BODY_READ_CAP;
    buffer.truncate(BODY_READ_CAP);
    Ok((String::from_utf8_lossy(&buffer).into_owned(), truncated))
}

/// Guard against fetching local/internal services. Three layers, in order:
///  1. reject non-http(s) schemes and embedded credentials;
///  2. reject statically-blockable internal *names* (localhost, `*.local`,
///     `metadata.google.internal`, `*.internal`/`.corp`/`.lan`/`.home.arpa`,
///     and bare single-label hosts like `jenkins`) — a fixed internal name is
///     not a DNS rebind, it is a trivially name-blockable SSRF target;
///  3. resolve the host and reject if ANY resolved address is internal
///     (loopback/private/link-local/unique-local/CGNAT/unspecified), so a
///     public-looking name that resolves to 169.254.169.254 is blocked.
///
/// Residual: DNS rebinding *between* this check and the actual connect (TOCTOU)
/// is not closed — a name that resolves public here could resolve private at
/// connect time microseconds later. Static internal names and private
/// resolutions are now blocked; the rebind race is a known residual.
pub(crate) fn check_url_allowed(url: &Url) -> std::result::Result<(), String> {
    check_url_allowed_with(url, resolve_host)
}

/// Core of [`check_url_allowed`] with an injectable resolver so the
/// resolve-and-block logic is unit-testable without live DNS.
fn check_url_allowed_with(
    url: &Url,
    resolve: impl Fn(&str, u16) -> std::io::Result<Vec<IpAddr>>,
) -> std::result::Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme `{other}`: only http and https are allowed"
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with embedded credentials are not allowed".to_string());
    }
    let Some(host) = url.host() else {
        return Err("URL has no host".to_string());
    };
    match host {
        Host::Domain(domain) => {
            if let Some(reason) = blocked_host_name(domain) {
                return Err(reason);
            }
            // Resolve and reject if the name maps to any internal address.
            let port = url.port_or_known_default().unwrap_or(0);
            if let Ok(addresses) = resolve(domain, port) {
                for address in addresses {
                    if ip_is_blocked(&address) {
                        return Err(format!(
                            "refusing to fetch `{domain}`: it resolves to internal address `{address}`"
                        ));
                    }
                }
            }
            // A resolution failure is left for the fetch itself to surface —
            // the connect simply fails rather than reaching anything.
        }
        Host::Ipv4(address) => {
            if ip_is_blocked(&IpAddr::V4(address)) {
                return Err(format!(
                    "refusing to fetch loopback/private/link-local address `{address}`"
                ));
            }
        }
        Host::Ipv6(address) => {
            if ip_is_blocked(&IpAddr::V6(address)) {
                return Err(format!(
                    "refusing to fetch loopback/private/link-local address `{address}`"
                ));
            }
        }
    }
    Ok(())
}

/// Reject statically-known internal/loopback host *names* before any DNS
/// lookup. Returns the rejection reason, or `None` if the name is not
/// obviously internal (it is still resolve-checked afterwards).
fn blocked_host_name(domain: &str) -> Option<String> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return Some("URL has an empty host".to_string());
    }
    if domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".local") {
        return Some(format!("refusing to fetch local host `{domain}`"));
    }
    const INTERNAL_SUFFIXES: [&str; 4] = [".internal", ".corp", ".lan", ".home.arpa"];
    let internal_suffix = INTERNAL_SUFFIXES
        .iter()
        .any(|suffix| domain.ends_with(suffix));
    // A bare single-label host (no dot) is never a public FQDN — `jenkins`,
    // `gitlab`, `router` all resolve only inside a private network.
    if domain == "metadata.google.internal"
        || domain == "home.arpa"
        || internal_suffix
        || !domain.contains('.')
    {
        return Some(format!("refusing to fetch internal host `{domain}`"));
    }
    None
}

fn resolve_host(host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
    Ok((host, port)
        .to_socket_addrs()?
        .map(|address| address.ip())
        .collect())
}

/// True for any address the web tools must never connect to: loopback,
/// RFC1918 private, link-local (incl. cloud metadata 169.254.169.254),
/// unique-local (`fc00::/7`), carrier-grade NAT (`100.64/10`), the
/// `0.0.0.0/8` "this network" block, broadcast, unspecified, and the
/// IPv4-mapped / IPv4-compatible IPv6 forms of all of the above.
pub(crate) fn ip_is_blocked(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => ipv4_is_blocked(*v4),
        IpAddr::V6(v6) => ipv6_is_blocked(*v6),
    }
}

fn ipv4_is_blocked(address: Ipv4Addr) -> bool {
    address.is_loopback() // 127.0.0.0/8
        || address.is_private() // 10/8, 172.16/12, 192.168/16
        || address.is_link_local() // 169.254.0.0/16 (cloud metadata)
        || address.is_unspecified() // 0.0.0.0
        || address.is_broadcast() // 255.255.255.255
        || address.octets()[0] == 0 // 0.0.0.0/8 "this network"
        || is_cgnat_ipv4(address) // 100.64.0.0/10
}

/// RFC 6598 shared address space (carrier-grade NAT), used by some cloud
/// link-local proxies and never routable on the public internet.
fn is_cgnat_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, ..] = address.octets();
    a == 100 && (64..=127).contains(&b)
}

fn ipv6_is_blocked(address: Ipv6Addr) -> bool {
    address.is_loopback() // ::1
        || address.is_unspecified() // ::
        || (address.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (address.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || address.to_ipv4_mapped().is_some_and(ipv4_is_blocked) // ::ffff:a.b.c.d
        || address.to_ipv4().is_some_and(ipv4_is_blocked) // ::a.b.c.d (deprecated)
        || nat64_embedded_ipv4(address).is_some_and(ipv4_is_blocked)
}

/// The IPv4 carried in a NAT64 address. In a DNS64/NAT64 network these are
/// static, race-free routes to the embedded IPv4 (e.g. `64:ff9b::169.254.169.254`
/// reaches the cloud metadata service), so the embedded address must face the
/// same block-list. Covers the well-known prefix `64:ff9b::/96` and the
/// local-use prefix `64:ff9b:1::/48` (RFC 8215); the low 32 bits are the IPv4.
fn nat64_embedded_ipv4(address: Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let s = address.segments();
    let well_known = s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0;
    let local_use = s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001;
    if well_known || local_use {
        Some(std::net::Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ))
    } else {
        None
    }
}

/// Per-hop decision for the redirect policy.
enum RedirectDecision {
    Follow,
    Stop(String),
    Block(String),
}

/// Production redirect decision: caps hops and re-runs the full (resolving)
/// URL guard on every hop.
fn redirect_decision(previous_hops: usize, next: &Url) -> RedirectDecision {
    redirect_decision_with(previous_hops, next, resolve_host)
}

/// Core of [`redirect_decision`] with an injectable resolver. Extracted from
/// the reqwest closure so the redirect-time SSRF guard is unit-testable —
/// reqwest's `Attempt` cannot be constructed outside a live client.
fn redirect_decision_with(
    previous_hops: usize,
    next: &Url,
    resolve: impl Fn(&str, u16) -> std::io::Result<Vec<IpAddr>>,
) -> RedirectDecision {
    if previous_hops > MAX_REDIRECTS {
        return RedirectDecision::Stop(format!("stopped after {MAX_REDIRECTS} redirects"));
    }
    if let Err(reason) = check_url_allowed_with(next, resolve) {
        return RedirectDecision::Block(format!("redirect blocked: {reason}"));
    }
    RedirectDecision::Follow
}

fn cap_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let capped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{capped}\n… truncated at {max_chars} chars")
    } else {
        capped
    }
}

// ---------------------------------------------------------------------------
// HTML → readable text
// ---------------------------------------------------------------------------

/// Containers whose entire content is dropped.
const DROPPED_CONTAINERS: [&str; 8] = [
    "script", "style", "head", "nav", "footer", "noscript", "svg", "iframe",
];

/// Tags that become a line break in the output.
const BLOCK_TAGS: [&str; 24] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "tr",
    "table",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "header",
    "aside",
    "blockquote",
    "hr",
    "form",
];

enum Segment {
    Text(String),
    Pre(String),
}

/// Hand-rolled readability pass: drop script/style/head/nav/footer content,
/// keep `<pre>` verbatim, render links as "text (url)", turn block tags into
/// newlines, unescape entities, and collapse blank runs.
pub(crate) fn html_to_text(html: &str) -> String {
    let html = strip_html_comments(html);
    // ASCII lowering preserves byte offsets, so structure is searched in
    // `lower` while content is sliced from `html`.
    let lower = html.to_ascii_lowercase();

    let mut segments = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < html.len() {
        let Some(open_rel) = html[i..].find('<') else {
            current.push_str(&unescape_entities(&html[i..]));
            break;
        };
        current.push_str(&unescape_entities(&html[i..i + open_rel]));
        i += open_rel;

        let Some(tag_end_rel) = html[i..].find('>') else {
            break; // malformed trailing tag: drop the remainder
        };
        let tag_inner = &html[i + 1..i + tag_end_rel];
        let after_tag = i + tag_end_rel + 1;

        let closing = tag_inner.starts_with('/');
        let name: String = tag_inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        if !closing && DROPPED_CONTAINERS.contains(&name.as_str()) {
            match find_close_tag(&lower, after_tag, &name) {
                Some((_, after_close)) => {
                    i = after_close;
                    continue;
                }
                None => break, // unclosed dropped container: drop the rest
            }
        }

        if !closing
            && name == "pre"
            && let Some((close_start, after_close)) = find_close_tag(&lower, after_tag, "pre")
        {
            segments.push(Segment::Text(std::mem::take(&mut current)));
            segments.push(Segment::Pre(strip_tags_verbatim(
                &html[after_tag..close_start],
            )));
            i = after_close;
            continue;
        }

        if !closing
            && name == "a"
            && let Some((close_start, after_close)) = find_close_tag(&lower, after_tag, "a")
        {
            let text = inline_text(&html[after_tag..close_start]);
            let href = attr_value(tag_inner, "href")
                .map(|value| unescape_entities(&value))
                .filter(|value| {
                    !value.is_empty()
                        && !value.starts_with('#')
                        && !value.starts_with("javascript:")
                });
            match href {
                Some(href) if text.is_empty() => current.push_str(&href),
                Some(href) if text != href => {
                    current.push_str(&format!("{text} ({href})"));
                }
                _ => current.push_str(&text),
            }
            i = after_close;
            continue;
        }

        if BLOCK_TAGS.contains(&name.as_str()) {
            current.push('\n');
        }
        i = after_tag;
    }

    segments.push(Segment::Text(current));

    let mut output = String::new();
    for segment in segments {
        match segment {
            Segment::Text(text) => {
                let normalized = normalize_text(&text);
                if !normalized.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&normalized);
                }
            }
            Segment::Pre(pre) => {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(pre.trim_matches('\n'));
            }
        }
    }
    output.trim_matches('\n').to_string()
}

fn strip_html_comments(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        output.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end_rel) => rest = &rest[start + end_rel + 3..],
            None => return output, // unterminated comment: drop the remainder
        }
    }
    output.push_str(rest);
    output
}

/// Find the first case-insensitive `</name>` close tag at or after `from`.
/// Returns (start of the close tag, index just past its `>`).
fn find_close_tag(lower: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let needle = format!("</{name}");
    let mut position = from;
    while let Some(rel) = lower.get(position..)?.find(&needle) {
        let start = position + rel;
        let after_name = start + needle.len();
        match lower[after_name..].chars().next() {
            Some('>') => return Some((start, after_name + 1)),
            Some(c) if c.is_ascii_whitespace() => {
                let gt = lower[after_name..].find('>')?;
                return Some((start, after_name + gt + 1));
            }
            None => return None,
            _ => position = after_name, // e.g. `</preface` while looking for `</pre`
        }
    }
    None
}

/// Extract an attribute value from the inside of a tag (`a href="..." ...`).
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search = 0;
    while let Some(rel) = lower.get(search..)?.find(name) {
        let at = search + rel;
        let boundary_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let mut cursor = at + name.len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if !boundary_ok || cursor >= bytes.len() || bytes[cursor] != b'=' {
            search = at + name.len();
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        let quote = bytes[cursor];
        return if quote == b'"' || quote == b'\'' {
            let start = cursor + 1;
            tag[start..]
                .find(quote as char)
                .map(|end| tag[start..start + end].to_string())
        } else {
            let end = tag[cursor..]
                .find(|c: char| c.is_ascii_whitespace())
                .map_or(tag.len(), |rel| cursor + rel);
            Some(tag[cursor..end].to_string())
        };
    }
    None
}

/// Strip tags and unescape entities, collapsing whitespace to single spaces.
fn inline_text(html: &str) -> String {
    unescape_entities(&strip_tags_verbatim(html))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove tags but keep the text (whitespace intact) and unescape entities.
fn strip_tags_verbatim(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        output.push_str(&rest[..start]);
        match rest[start..].find('>') {
            Some(end_rel) => rest = &rest[start + end_rel + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    output.push_str(rest);
    unescape_entities(&output)
}

/// Collapse horizontal whitespace per line and drop blank lines entirely —
/// block tags already provide one newline of separation, and compact output
/// costs the model fewer tokens.
fn normalize_text(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn unescape_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut output = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'&'
            && let Some(semi_rel) = text[i..].find(';')
            && (2..=12).contains(&semi_rel)
            && let Some(decoded) = decode_entity(&text[i + 1..i + semi_rel])
        {
            output.push(decoded);
            i += semi_rel + 1;
            continue;
        }
        let ch = text[i..].chars().next().expect("index on char boundary");
        output.push(ch);
        i += ch.len_utf8();
    }
    output
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('–'),
        "mdash" => Some('—'),
        "hellip" => Some('…'),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "copy" => Some('©'),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                digits.parse::<u32>().ok()?
            };
            char::from_u32(code)
        }
    }
}

// ---------------------------------------------------------------------------
// DuckDuckGo HTML results parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
}

fn ddg_link_regex() -> &'static Regex {
    static LINK: OnceLock<Regex> = OnceLock::new();
    LINK.get_or_init(|| {
        Regex::new(r#"(?is)<a\s([^>]*class="result__a"[^>]*)>(.*?)</a>"#)
            .expect("DDG link regex compiles")
    })
}

fn ddg_snippet_regex() -> &'static Regex {
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    SNIPPET.get_or_init(|| {
        Regex::new(r#"(?is)<(?:a|td|div|span)\s[^>]*class="result__snippet"[^>]*>(.*?)</(?:a|td|div|span)>"#)
            .expect("DDG snippet regex compiles")
    })
}

/// Parse DuckDuckGo's html.duckduckgo.com results page with resilient
/// string/regex matching. Ads and undecodable links are skipped.
pub(crate) fn parse_ddg_results(html: &str, count: usize) -> Vec<SearchHit> {
    let links: Vec<(usize, usize, String, String)> = ddg_link_regex()
        .captures_iter(html)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let attrs = captures.get(1)?.as_str();
            let title = inline_text(captures.get(2)?.as_str());
            let url = decode_ddg_href(&attr_value(attrs, "href")?)?;
            if title.is_empty() {
                return None;
            }
            Some((whole.start(), whole.end(), title, url))
        })
        .collect();

    let snippets: Vec<(usize, String)> = ddg_snippet_regex()
        .captures_iter(html)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let text = inline_text(captures.get(1)?.as_str());
            (!text.is_empty()).then_some((whole.start(), text))
        })
        .collect();

    links
        .iter()
        .take(count)
        .enumerate()
        .map(|(index, (_, end, title, url))| {
            let next_start = links.get(index + 1).map(|(start, ..)| *start);
            let snippet = snippets
                .iter()
                .find(|(position, _)| {
                    *position >= *end && next_start.is_none_or(|next| *position < next)
                })
                .map(|(_, text)| text.clone())
                .unwrap_or_default();
            SearchHit {
                title: title.clone(),
                url: url.clone(),
                snippet,
            }
        })
        .collect()
}

/// Decode a DDG result href: resolve the `uddg=` redirect parameter when
/// present, otherwise accept direct/protocol-relative http links. Ad links
/// are rejected.
fn decode_ddg_href(href: &str) -> Option<String> {
    let href = unescape_entities(href);
    if href.contains("y.js") || href.contains("ad_domain=") {
        return None;
    }
    if let Some(position) = href.find("uddg=") {
        let raw = href[position + 5..].split('&').next().unwrap_or("");
        let decoded = percent_decode(raw);
        return (!decoded.is_empty()).then_some(decoded);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    href.starts_with("http").then_some(href)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            output.push(high * 16 + low);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            output.push(b' ');
        } else {
            output.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
