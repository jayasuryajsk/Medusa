use super::*;

fn assert_rejected(raw: &str, fragment: &str) {
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(_) => return, // unparseable is rejected upstream, also fine
    };
    let error = check_url_allowed(&url).expect_err(&format!("{raw} should be rejected"));
    assert!(
        error.contains(fragment),
        "error for {raw} should mention {fragment:?}, got: {error}"
    );
}

#[test]
fn url_guard_rejects_non_http_schemes() {
    assert_rejected("file:///etc/passwd", "scheme");
    assert_rejected("ftp://example.com/", "scheme");
}

#[test]
fn url_guard_rejects_local_and_private_hosts() {
    assert_rejected("http://localhost:8080/admin", "local host");
    assert_rejected("http://sub.localhost/", "local host");
    assert_rejected("http://printer.local/", "local host");
    assert_rejected("http://127.0.0.1/", "refusing");
    assert_rejected("http://127.9.8.7/", "refusing");
    assert_rejected("http://10.0.0.5/", "refusing");
    assert_rejected("http://172.16.0.1/", "refusing");
    assert_rejected("http://172.31.255.255/", "refusing");
    assert_rejected("http://192.168.1.1/", "refusing");
    assert_rejected("http://169.254.169.254/", "refusing");
    assert_rejected("http://0.0.0.0/", "refusing");
    assert_rejected("http://[::1]/", "refusing");
    assert_rejected("http://[fe80::1]/", "refusing");
    assert_rejected("http://[fd00::1]/", "refusing");
    assert_rejected("http://[::ffff:127.0.0.1]/", "refusing");
}

#[test]
fn url_guard_rejects_embedded_credentials() {
    assert_rejected("https://user:secret@example.com/", "credentials");
    assert_rejected("https://user@example.com/", "credentials");
}

#[test]
fn url_guard_allows_public_hosts() {
    // IP-literal hosts never hit DNS, so these use the real guard offline.
    for raw in [
        "https://172.32.0.1/",
        "https://8.8.8.8/",
        "https://[2606:4700:4700::1111]/",
    ] {
        let url = Url::parse(raw).unwrap();
        assert!(check_url_allowed(&url).is_ok(), "{raw} should be allowed");
    }
    // Domain hosts go through the resolver seam so the suite stays offline.
    let public = |_host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))])
    };
    for raw in ["https://docs.rs/reqwest", "http://example.com/path?x=1"] {
        let url = Url::parse(raw).unwrap();
        assert!(
            check_url_allowed_with(&url, public).is_ok(),
            "{raw} should be allowed"
        );
    }
}

#[test]
fn ip_is_blocked_covers_internal_ranges() {
    let blocked = [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.5.5",
        "192.168.0.1",
        "169.254.169.254", // GCP/AWS/Azure metadata
        "100.64.1.1",      // carrier-grade NAT
        "100.127.255.255",
        "0.0.0.0",
        "0.1.2.3",
        "255.255.255.255",
    ];
    for raw in blocked {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(ip_is_blocked(&ip), "{raw} must be blocked");
    }
    for raw in [
        "8.8.8.8",
        "93.184.216.34",
        "100.63.0.1",
        "100.128.0.1",
        "1.1.1.1",
    ] {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(!ip_is_blocked(&ip), "{raw} must be allowed");
    }
    for raw in [
        "::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::",
    ] {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(ip_is_blocked(&ip), "{raw} must be blocked");
    }
    for raw in ["2606:4700:4700::1111", "2001:4860:4860::8888"] {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(!ip_is_blocked(&ip), "{raw} must be allowed");
    }
    // NAT64-embedded internal IPv4 (well-known 64:ff9b::/96 and local-use
    // 64:ff9b:1::/48) must be blocked — in a DNS64/NAT64 network these are
    // static, race-free routes to the embedded IPv4 (e.g. metadata).
    for raw in [
        "64:ff9b::a9fe:a9fe",     // 169.254.169.254 metadata
        "64:ff9b::0a00:0001",     // 10.0.0.1
        "64:ff9b:1::a9fe:a9fe",   // local-use prefix
        "64:ff9b:1:0:0:0:7f00:1", // 127.0.0.1 via local-use
    ] {
        let ip: IpAddr = raw.parse().unwrap();
        assert!(ip_is_blocked(&ip), "NAT64 {raw} must be blocked");
    }
    // NAT64-embedded PUBLIC IPv4 stays allowed (8.8.8.8 = 0808:0808).
    let public_nat64: IpAddr = "64:ff9b::808:808".parse().unwrap();
    assert!(!ip_is_blocked(&public_nat64), "NAT64 public target allowed");
}

#[test]
fn host_name_classifier_blocks_internal_names() {
    for name in [
        "metadata.google.internal",
        "foo.internal",
        "svc.corp",
        "printer.lan",
        "host.home.arpa",
        "home.arpa",
        "jenkins", // bare single-label
        "gitlab",
        "localhost",
        "api.localhost",
        "printer.local",
    ] {
        assert!(
            blocked_host_name(name).is_some(),
            "{name} must be name-blocked"
        );
    }
    for name in [
        "docs.rs",
        "example.com",
        "sub.example.com",
        "api.github.com",
    ] {
        assert!(
            blocked_host_name(name).is_none(),
            "{name} must not be name-blocked"
        );
    }
}

#[test]
fn resolve_check_rejects_private_resolving_host() {
    // A public-looking name that DNS maps to a private/metadata address is
    // rejected even though its NAME passes the static classifier.
    let url = Url::parse("http://totally-public.example/").unwrap();
    assert!(blocked_host_name("totally-public.example").is_none());

    let to_metadata = |_host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))])
    };
    let error = check_url_allowed_with(&url, to_metadata)
        .expect_err("private-resolving host must be rejected");
    assert!(error.contains("internal address"), "got: {error}");

    // Mixed answer: one public, one private → still rejected.
    let mixed = |_host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
        Ok(vec![
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        ])
    };
    assert!(check_url_allowed_with(&url, mixed).is_err());
}

#[test]
fn resolve_check_allows_public_resolving_host() {
    let url = Url::parse("https://totally-public.example/").unwrap();
    let to_public = |_host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))])
    };
    assert!(check_url_allowed_with(&url, to_public).is_ok());
}

#[test]
fn redirect_guard_blocks_internal_hops_and_caps_count() {
    let real = resolve_host;
    // Literal internal IPs and names are blocked on redirect (offline).
    for raw in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:8080/admin",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://[::1]/",
    ] {
        let url = Url::parse(raw).unwrap();
        assert!(
            matches!(
                redirect_decision_with(1, &url, real),
                RedirectDecision::Block(_)
            ),
            "{raw} must be blocked on redirect"
        );
    }

    // A public IP within the hop budget is followed.
    let public_ip = Url::parse("https://8.8.8.8/").unwrap();
    assert!(matches!(
        redirect_decision_with(1, &public_ip, real),
        RedirectDecision::Follow
    ));

    // Exceeding the hop cap stops even for a public target.
    assert!(matches!(
        redirect_decision_with(MAX_REDIRECTS + 1, &public_ip, real),
        RedirectDecision::Stop(_)
    ));

    // A public-looking name that resolves to a private IP is blocked
    // per-hop via the injected resolver (no live DNS).
    let rebind = |_host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))])
    };
    let redirect_target = Url::parse("http://totally-public.example/").unwrap();
    assert!(matches!(
        redirect_decision_with(1, &redirect_target, rebind),
        RedirectDecision::Block(_)
    ));
}

#[test]
fn html_extraction_strips_scripts_styles_and_chrome() {
    let html = r#"<html><head><title>ignored</title><style>body { color: red }</style></head>
<body><nav>Home | About</nav>
<h1>Real Title</h1>
<script>alert("evil");</script>
<p>Visible paragraph.</p>
<footer>copyright</footer></body></html>"#;
    let text = html_to_text(html);
    assert!(text.contains("Real Title"));
    assert!(text.contains("Visible paragraph."));
    assert!(!text.contains("ignored"));
    assert!(!text.contains("color: red"));
    assert!(!text.contains("alert"));
    assert!(!text.contains("Home | About"));
    assert!(!text.contains("copyright"));
}

#[test]
fn html_extraction_preserves_pre_blocks_verbatim() {
    let html = "<p>Intro   text</p><pre><code>fn main() {\n    let x = 1;   // spaces kept\n}</code></pre><p>After</p>";
    let text = html_to_text(html);
    assert!(text.contains("Intro text"), "non-pre whitespace collapses");
    assert!(
        text.contains("fn main() {\n    let x = 1;   // spaces kept\n}"),
        "pre content must keep indentation and internal spaces: {text}"
    );
    assert!(text.contains("After"));
}

#[test]
fn html_extraction_unescapes_entities() {
    let html = "<p>a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41;&#66;</p>";
    let text = html_to_text(html);
    assert_eq!(text, "a & b <c> \"d\" 'e' AB");
}

#[test]
fn html_extraction_renders_links_as_text_and_url() {
    let html = r#"<p>See <a href="https://docs.rs/tokio">tokio docs</a> for details.</p>"#;
    let text = html_to_text(html);
    assert!(
        text.contains("tokio docs (https://docs.rs/tokio)"),
        "link should render as text (url): {text}"
    );
}

#[test]
fn html_extraction_skips_fragment_and_javascript_links() {
    let html =
        r##"<p><a href="#top">Back to top</a> and <a href="javascript:void(0)">click</a></p>"##;
    let text = html_to_text(html);
    assert!(text.contains("Back to top"));
    assert!(text.contains("click"));
    assert!(!text.contains('('));
}

#[test]
fn html_extraction_turns_block_tags_into_newlines_and_collapses_blanks() {
    let html = "<div>one</div><div></div><div></div><div>two</div><br>three";
    let text = html_to_text(html);
    assert_eq!(text, "one\ntwo\nthree");
}

#[test]
fn html_extraction_drops_comments() {
    let text = html_to_text("before<!-- hidden <p>fake</p> -->after");
    assert_eq!(text, "beforeafter");
}

const DDG_FIXTURE: &str = r#"
<html><body>
<div class="serp__results">
  <div class="result results_links results_links_deep web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F&amp;rut=abc123">The Rust Programming <b>Language</b></a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F">Affectionately nicknamed &quot;the book&quot; &mdash; learn <b>Rust</b>.</a>
  </div>
  <div class="result results_links result--ad">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=ads.example&u3=spam">Buy Rust Now</a>
    </h2>
  </div>
  <div class="result results_links results_links_deep web-result">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="https://docs.rs/tokio/latest/tokio/">tokio - Rust</a>
    </h2>
    <a class="result__snippet" href="https://docs.rs/tokio/latest/tokio/">A runtime for writing reliable async applications.</a>
  </div>
</div>
</body></html>
"#;

#[test]
fn ddg_parser_extracts_results_and_decodes_redirects() {
    let hits = parse_ddg_results(DDG_FIXTURE, 8);
    assert_eq!(hits.len(), 2, "ad result must be skipped: {hits:?}");

    assert_eq!(hits[0].title, "The Rust Programming Language");
    assert_eq!(hits[0].url, "https://doc.rust-lang.org/book/");
    assert_eq!(
        hits[0].snippet,
        "Affectionately nicknamed \"the book\" — learn Rust."
    );

    assert_eq!(hits[1].title, "tokio - Rust");
    assert_eq!(hits[1].url, "https://docs.rs/tokio/latest/tokio/");
    assert_eq!(
        hits[1].snippet,
        "A runtime for writing reliable async applications."
    );
}

#[test]
fn ddg_parser_respects_count_cap() {
    let hits = parse_ddg_results(DDG_FIXTURE, 1);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "The Rust Programming Language");
}

#[test]
fn ddg_parser_handles_layout_changes_honestly() {
    let hits = parse_ddg_results("<html><body><div>totally different</div></body></html>", 8);
    assert!(hits.is_empty());
    let output = format_search_results("anything", &hits);
    assert!(output.contains("results: 0"));
    assert!(output.contains("no results parsed — DDG layout may have changed"));
}

#[test]
fn fetch_output_is_capped_with_truncation_notice() {
    let long = "x".repeat(500);
    let capped = cap_chars(&long, 100);
    assert!(capped.starts_with(&"x".repeat(100)));
    assert!(capped.ends_with("… truncated at 100 chars"));
    assert_eq!(cap_chars("short", 100), "short");
}

#[test]
fn percent_decode_handles_escapes_and_plus() {
    assert_eq!(
        percent_decode("https%3A%2F%2Fexample.com%2Fa+b"),
        "https://example.com/a b"
    );
    assert_eq!(percent_decode("no-escapes"), "no-escapes");
    assert_eq!(percent_decode("bad%zz"), "bad%zz");
}
