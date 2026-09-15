//! The Resend API, the receiving half and the one send with attachments.
//!
//! `email.rs` mails a sign-in link and knows nothing else about Resend; that
//! is deliberate and it stays. This module is for the inbox: reading a mail
//! Resend received for us, listing and downloading its attachments, and
//! sending the forward that carries them on. The base URL is a parameter so
//! a test can point the client at a local server; the key is a field that
//! no `Debug` prints and no log line names.

use base64::Engine;
use serde::Deserialize;
use tokio_stream::StreamExt;

/// A client for the receiving side of Resend and the one send that needs
/// attachments. Cheap to clone: the `reqwest::Client` is a handle.
///
/// No `Debug` on purpose. The derive would print the key, and a `{:?}` in
/// a log line is exactly how a key ends up in a log.
#[derive(Clone)]
pub struct ResendClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

/// An attachment as the received-mail record lists it: a name and a type,
/// but no way to fetch it. `attachments` is the call that adds the URL.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// A mail Resend received for us, body included. Only what the worker
/// reads: `to` is on the wire too, but the webhook already decided whose
/// the mail is, and a field no one reads is a field no one keeps right.
///
/// Every field but `from` is optional because Resend's shape has been seen
/// to omit an empty one rather than send `null`, and a forward that fails
/// to parse because a mail had no subject is a mail nobody gets.
/// No `Debug`: a mail body in a log is a mail body in a log.
#[derive(Clone, Deserialize)]
pub struct Received {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Meta>,
}

/// One entry of the attachment list: what `Meta` says, plus a pre-signed
/// `download_url` that is good for an hour.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub content_type: String,
    pub download_url: String,
}

/// A message to send. `attachments` are `(filename, bytes)`; the client
/// base64s them, since that is a fact about Resend's wire format and not
/// about the mail.
/// No `Debug`, as for `Received`.
#[derive(Clone)]
pub struct Outgoing {
    pub from: String,
    pub to: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<(String, Vec<u8>)>,
}

/// How the list endpoint pages. `has_more` and `after` are absent from the
/// documented attachment response today, so both default to "one page";
/// if Resend starts paging it, we follow rather than silently drop the
/// second page's attachments.
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    data: Vec<AttachmentMeta>,
    #[serde(default)]
    has_more: bool,
}

/// A bound on how many pages `attachments` will turn, so a server that
/// always says `has_more` cannot keep us in the loop. Nobody attaches a
/// thousand files to a booking.
const MOST_PAGES: usize = 20;

/// An id fit for a path. Resend's are `re_…`, `att_…` or UUIDs; anything
/// else — a slash, a query, a fragment — would address a different
/// endpoint with our key on the request.
fn path_id(id: &str) -> anyhow::Result<&str> {
    let plain = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !plain {
        anyhow::bail!("not a resend id");
    }
    Ok(id)
}

impl ResendClient {
    /// `http` is the caller's: it should carry a timeout and a bounded
    /// redirect policy, since `download` follows a URL the attachment
    /// list named. `email::client` is the one built for this.
    pub fn new(http: reqwest::Client, api_key: String, base_url: String) -> Self {
        Self { http, api_key, base_url: base_url.trim_end_matches('/').to_string() }
    }

    /// `GET /emails/receiving/{id}`: the mail with its body.
    pub async fn received(&self, id: &str) -> anyhow::Result<Received> {
        let res = self
            .http
            .get(format!("{}/emails/receiving/{}", self.base_url, path_id(id)?))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        Ok(accepted(res, "reading a received mail")?.json().await?)
    }

    /// `GET /emails/receiving/{id}/attachments`: every attachment with a
    /// URL to fetch it from, across pages if there are pages.
    pub async fn attachments(&self, id: &str) -> anyhow::Result<Vec<AttachmentMeta>> {
        let url = format!("{}/emails/receiving/{}/attachments", self.base_url, path_id(id)?);
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MOST_PAGES {
            let mut req = self.http.get(&url).bearer_auth(&self.api_key);
            if let Some(a) = &after {
                req = req.query(&[("after", a)]);
            }
            let page: Page = accepted(req.send().await?, "listing attachments")?.json().await?;
            let last = page.data.last().map(|a| a.id.clone());
            all.extend(page.data);
            match (page.has_more, last) {
                (true, Some(id)) => after = Some(id),
                // An empty page that claims more is a server going in
                // circles; take what we have.
                _ => return Ok(all),
            }
        }
        // Past the bound, the same answer as the empty page: what was
        // collected, not an error. A mail with more attachments than
        // this is still a mail, and the forward carries what we saw.
        Ok(all)
    }

    /// Fetches a pre-signed attachment URL, refusing anything over
    /// `cap_bytes` — by `Content-Length` when the server states one, and
    /// by counting as the bytes arrive either way, so a server that says
    /// nothing or lies about its size still cannot make us hold more than
    /// the cap in memory.
    ///
    /// No bearer token on this request. The URL is Resend's storage, not
    /// its API, and a key sent to whatever host the list named is a key
    /// given away.
    pub async fn download(&self, url: &str, cap_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let res = accepted(self.http.get(url).send().await?, "downloading an attachment")?;
        let stated = res.content_length();
        if let Some(len) = stated {
            if len > cap_bytes as u64 {
                anyhow::bail!("the attachment is {len} bytes, over the {cap_bytes}-byte cap");
            }
        }
        let mut body = Vec::with_capacity(stated.unwrap_or(0).min(cap_bytes as u64) as usize);
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > cap_bytes {
                anyhow::bail!("the attachment ran past the {cap_bytes}-byte cap");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// `POST /emails`: one message, attachments inline as base64.
    pub async fn send(&self, mail: &Outgoing) -> anyhow::Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let attachments: Vec<serde_json::Value> = mail
            .attachments
            .iter()
            .map(|(filename, bytes)| serde_json::json!({ "filename": filename, "content": b64.encode(bytes) }))
            .collect();
        // Absent rather than `null`: Resend validates `html: null` as a
        // present-but-wrong field and refuses the message.
        let mut body = serde_json::json!({
            "from": mail.from,
            "to": [mail.to],
            "subject": mail.subject,
            "attachments": attachments,
        });
        if let Some(text) = &mail.text {
            body["text"] = serde_json::Value::String(text.clone());
        }
        if let Some(html) = &mail.html {
            body["html"] = serde_json::Value::String(html.clone());
        }
        if let Some(reply_to) = &mail.reply_to {
            body["reply_to"] = serde_json::json!([reply_to]);
        }
        let res = self
            .http
            .post(format!("{}/emails", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        accepted(res, "sending a message")?;
        Ok(())
    }
}

/// The response if it was a success, else an error naming the status and
/// only the status. Resend's error bodies are prose about the request,
/// and this error is the thing a log line will print; the body is the one
/// place the key could come back to us.
fn accepted(res: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!("resend answered {status} while {what}");
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn the_client_reads_a_received_mail_its_attachments_and_sends_a_forward() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"re_1","from":"hotel@example.com","to":["sasha@goodscout.fyi"],"subject":"Your booking","text":"Check-in 12 Oct","html":"<p>Check-in 12 Oct</p>","attachments":[{"id":"att_1","filename":"ticket.pdf","content_type":"application/pdf","size":3}]})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[{"id":"att_1","filename":"ticket.pdf","size":3,"content_type":"application/pdf","download_url":format!("{}/dl/att_1", server.uri()),"expires_at":"2026-09-15T13:00:00Z"}]})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/dl/att_1")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF".to_vec())).mount(&server).await;
        Mock::given(method("POST")).and(path("/emails")).and(header("authorization", "Bearer k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"sent_1"}))).mount(&server).await;

        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        let mail = client.received("re_1").await.unwrap();
        assert_eq!(mail.text.as_deref(), Some("Check-in 12 Oct"));
        assert_eq!(mail.from, "hotel@example.com");
        assert_eq!(mail.attachments[0].id, "att_1");
        let atts = client.attachments("re_1").await.unwrap();
        assert_eq!(atts[0].filename, "ticket.pdf");
        assert_eq!(client.download(&atts[0].download_url, 10).await.unwrap(), b"%PDF");
        client.send(&Outgoing { from: "scout@send.goodscout.fyi".into(), to: "me@example.com".into(), reply_to: Some("hotel@example.com".into()), subject: "Your booking".into(), text: Some("Check-in 12 Oct".into()), html: None, attachments: vec![("ticket.pdf".into(), b"%PDF".to_vec())] }).await.unwrap();
        let sent = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent.iter().find(|r| r.url.path() == "/emails").unwrap().body).unwrap();
        assert_eq!(body["reply_to"], json!(["hotel@example.com"]));
        assert_eq!(body["attachments"][0]["content"], "JVBERg==");
        assert_eq!(body["to"], json!(["me@example.com"]));
        assert!(body.get("html").is_none(), "an absent html must not be sent as null");
        // The reads carried the key too — Resend refuses them otherwise.
        let read = sent.iter().find(|r| r.url.path() == "/emails/receiving/re_1").unwrap();
        assert_eq!(read.headers.get("authorization").unwrap(), "Bearer k");
        // And the download did not: that URL is pre-signed, on whatever
        // host Resend's storage lives on, and our key is not its business.
        let dl = sent.iter().find(|r| r.url.path() == "/dl/att_1").unwrap();
        assert!(dl.headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn a_download_over_the_cap_is_refused_whether_or_not_the_server_says_its_size() {
        let server = wiremock::MockServer::start().await;
        // Honest about its size: refused before a byte of body is read.
        Mock::given(method("GET")).and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 64]))
            .mount(&server).await;
        // Lying — no Content-Length at all — so only the count can catch it.
        Mock::given(method("GET")).and(path("/chunked"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0u8; 64], "application/octet-stream").insert_header("transfer-encoding", "chunked"))
            .mount(&server).await;
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        assert!(client.download(&format!("{}/big", server.uri()), 10).await.is_err());
        assert!(client.download(&format!("{}/chunked", server.uri()), 10).await.is_err());
        assert_eq!(client.download(&format!("{}/big", server.uri()), 64).await.unwrap().len(), 64, "exactly the cap is allowed");
    }

    #[tokio::test]
    async fn a_refusal_reports_the_status_and_not_the_body() {
        // The body of a 401 from Resend can quote the request, and the
        // request carried the key. The error is what the log will print.
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST")).and(path("/emails"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid key: k-secret-echo"))
            .mount(&server).await;
        let client = ResendClient::new(reqwest::Client::new(), "k-secret-echo".into(), server.uri());
        let err = client.send(&Outgoing { from: "a".into(), to: "b".into(), reply_to: None, subject: "s".into(), text: None, html: None, attachments: vec![] }).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("401"), "{text}");
        assert!(!text.contains("k-secret-echo"), "{text}");
    }

    #[tokio::test]
    async fn an_id_that_is_not_an_id_never_reaches_the_wire() {
        // The id goes into a path. One with a slash or a query in it
        // would address a different endpoint with our key attached.
        let server = wiremock::MockServer::start().await;
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        for bad in ["re_1/../emails", "re_1?x=1", "re 1", "", "re_1#a"] {
            assert!(client.received(bad).await.is_err(), "{bad:?}");
            assert!(client.attachments(bad).await.is_err(), "{bad:?}");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_attachment_list_follows_has_more() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments")).and(wiremock::matchers::query_param("after", "att_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[{"id":"att_2","filename":"b.pdf","size":1,"content_type":"application/pdf","download_url":"u2"}],"has_more":false})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/emails/receiving/re_1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object":"list","data":[{"id":"att_1","filename":"a.pdf","size":1,"content_type":"application/pdf","download_url":"u1"}],"has_more":true})))
            .mount(&server).await;
        let client = ResendClient::new(reqwest::Client::new(), "k".into(), server.uri());
        let atts = client.attachments("re_1").await.unwrap();
        assert_eq!(atts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["att_1", "att_2"]);
    }
}
