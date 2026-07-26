use reqwest::Url;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Headers for all page requests (fetch_page and link verification). A real
/// browser UA matters: e.g. Amazon serves full product pages to it but a
/// 503 bot-wall to bot-styled UAs — this is a personal assistant fetching a
/// handful of public pages, not bulk scraping.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// GET with browser-like headers; shared by fetch_page and link probing.
pub(crate) fn browser_get(http: &reqwest::Client, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
    http.get(url)
        .header("User-Agent", BROWSER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,nl;q=0.8")
}

/// Readable-text cap: enough for a product/listing page's useful content
/// without flooding the model context.
const MAX_TEXT_CHARS: usize = 6000;
const MAX_LINKS: usize = 30;
const MAX_LINK_TEXT_CHARS: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("fetch failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Deserialize)]
pub struct FetchArgs {
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct PageLink {
    pub url: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct PageContent {
    pub text: String,
    pub truncated: bool,
    pub links: Vec<PageLink>,
}

/// Plain-HTTP page fetcher: readable text + links, so the agent can open a
/// retailer listing page and pull out direct product URLs and prices.
pub struct FetchPageTool {
    pub http: reqwest::Client,
}

impl Tool for FetchPageTool {
    const NAME: &'static str = "fetch_page";
    type Error = FetchError;
    type Args = FetchArgs;
    type Output = PageContent;

    fn description(&self) -> String {
        "Fetch a web page and return its readable text plus its links. Use it \
         to open a retailer listing/search page or a product page and extract \
         direct product links and prices."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "http(s) URL of the page to fetch"}
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let url = Url::parse(&args.url)
            .map_err(|e| FetchError::Invalid(format!("invalid url: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(FetchError::Invalid(format!(
                "unsupported url scheme: {}",
                url.scheme()
            )));
        }
        let resp = browser_get(&self.http, url.clone()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Invalid(format!("page returned HTTP {status}")));
        }
        let html = resp.text().await?;
        Ok(extract_page(&html, &url))
    }
}

fn extract_page(html: &str, base: &Url) -> PageContent {
    let without_scripts = strip_tag_blocks(&strip_tag_blocks(html, "script"), "style");
    let links = extract_links(&without_scripts, base);
    let full_text = collapse_ws(&decode_entities(&strip_tags(&without_scripts)));
    let truncated = full_text.chars().count() > MAX_TEXT_CHARS;
    let text = if truncated {
        full_text.chars().take(MAX_TEXT_CHARS).collect()
    } else {
        full_text
    };
    PageContent { text, truncated, links }
}

/// Remove `<tag ...>...</tag>` blocks wholesale (for script/style, whose
/// bodies are not content). ASCII-lowercased shadow keeps byte offsets valid.
fn strip_tag_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(&open) {
        let start = pos + rel;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => pos = start + rel_end + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the common entities; `&amp;` last so `&amp;lt;` becomes `&lt;`,
/// not `<`.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_links(html: &str, base: &Url) -> Vec<PageLink> {
    let lower = html.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let a_start = pos + rel;
        // Guard against <article>, <abbr>, ...: "<a" must end the tag name.
        match lower.as_bytes().get(a_start + 2) {
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') => {}
            _ => {
                pos = a_start + 2;
                continue;
            }
        }
        let Some(tag_end_rel) = lower[a_start..].find('>') else {
            break;
        };
        let tag_end = a_start + tag_end_rel;
        let href = find_attr(&html[a_start..tag_end], "href");
        let Some(close_rel) = lower[tag_end..].find("</a>") else {
            pos = tag_end + 1;
            continue;
        };
        let close = tag_end + close_rel;
        let text = collapse_ws(&decode_entities(&strip_tags(&html[tag_end + 1..close])));
        pos = close + "</a>".len();

        let Some(href) = href else { continue };
        let href = decode_entities(href.trim());
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let Ok(abs) = base.join(&href) else { continue };
        if !matches!(abs.scheme(), "http" | "https") || text.is_empty() {
            continue;
        }
        links.push(PageLink {
            url: abs.to_string(),
            text: text.chars().take(MAX_LINK_TEXT_CHARS).collect(),
        });
        if links.len() >= MAX_LINKS {
            break;
        }
    }
    links
}

fn find_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let idx = lower.find(&format!("{name}="))?;
    let rest = &tag[idx + name.len() + 1..];
    let mut chars = rest.chars();
    match chars.next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next(),
        _ => rest.split([' ', '\t', '\n', '\r', '>']).next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base() -> Url {
        Url::parse("https://shop.example/s/?q=widget").unwrap()
    }

    #[test]
    fn extracts_text_without_scripts_and_styles() {
        let html = r#"<html><head><style>.x{color:red}</style>
            <script>var a = "<b>ignored</b>";</script></head>
            <body><h1>Alfa AWUS036ACHM</h1><p>Price: &euro;class &#39;A&#39; &amp; 49.99</p></body></html>"#;
        let page = extract_page(html, &base());
        assert!(page.text.contains("Alfa AWUS036ACHM"));
        assert!(page.text.contains("'A' & 49.99"));
        assert!(!page.text.contains("ignored"));
        assert!(!page.text.contains("color:red"));
        assert!(!page.truncated);
    }

    #[test]
    fn extracts_and_absolutizes_links_skipping_junk() {
        let html = r##"<body>
            <a href="/nl/p/alfa-awus036achm/93000123/">Alfa AWUS036ACHM adapter</a>
            <a href="https://other.example/full">Full link</a>
            <a href="#reviews">Reviews</a>
            <a href="javascript:void(0)">JS</a>
            <a href="/empty-text"><img src="x.png"></a>
            <article>not a link</article>
            </body>"##;
        let page = extract_page(html, &base());
        assert_eq!(
            page.links,
            vec![
                PageLink {
                    url: "https://shop.example/nl/p/alfa-awus036achm/93000123/".into(),
                    text: "Alfa AWUS036ACHM adapter".into()
                },
                PageLink {
                    url: "https://other.example/full".into(),
                    text: "Full link".into()
                },
            ]
        );
    }

    #[test]
    fn long_text_is_truncated_with_flag() {
        let html = format!("<body>{}</body>", "word ".repeat(3000));
        let page = extract_page(&html, &base());
        assert!(page.truncated);
        assert_eq!(page.text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn link_cap_respected() {
        let many: String = (0..50)
            .map(|i| format!("<a href=\"/p/{i}\">Product {i}</a>"))
            .collect();
        let page = extract_page(&format!("<body>{many}</body>"), &base());
        assert_eq!(page.links.len(), MAX_LINKS);
    }

    #[tokio::test]
    async fn tool_fetches_and_extracts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/p/1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html><body><h1>Widget</h1><a href="/p/2">Other widget</a></body></html>"#,
            ))
            .mount(&server)
            .await;

        let tool = FetchPageTool { http: reqwest::Client::new() };
        let page = tool
            .call(FetchArgs { url: format!("{}/p/1", server.uri()) })
            .await
            .unwrap();
        assert!(page.text.contains("Widget"));
        assert_eq!(page.links.len(), 1);
        assert!(page.links[0].url.ends_with("/p/2"));
    }

    #[tokio::test]
    async fn tool_reports_http_errors_and_bad_urls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let tool = FetchPageTool { http: reqwest::Client::new() };
        let err = tool
            .call(FetchArgs { url: format!("{}/gone", server.uri()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "got: {err}");

        let err = tool
            .call(FetchArgs { url: "ftp://x".into() })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("scheme"), "got: {err}");
    }
}
