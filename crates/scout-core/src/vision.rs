use crate::agent::{side_call, AgentDeps, SIDE_PAGE, SIDE_PHOTO};
use crate::text::strip_thinking;
use anyhow::{bail, Result};
use base64::prelude::{Engine, BASE64_STANDARD};
use rig::client::CompletionClient;
use rig::completion::message::Image;
use rig::message::{DocumentSourceKind, ImageDetail, ImageMediaType};

const VISION_PREAMBLE: &str = "\
Look at the product in the photo and describe it as one concise web-search \
query: brand and model if identifiable, otherwise category plus distinguishing \
features (color, material, style). Reply with the query text only - no \
explanations, no quotes.";

/// How long a photo may take to describe: a query is a line, and the
/// same minute a page gets.
pub const PHOTO_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Telegram photos are always JPEG.
pub async fn describe_photo(
    d: &AgentDeps,
    image_jpeg: &[u8],
    caption: Option<&str>,
) -> Result<String> {
    let mut preamble = VISION_PREAMBLE.to_string();
    if let Some(c) = caption.map(str::trim).filter(|c| !c.is_empty()) {
        preamble.push_str(&format!("\nUser note about this photo: {c}"));
    }
    let agent = d.side.llm.agent(&d.side.name).preamble(&preamble).build();
    let image = Image {
        data: DocumentSourceKind::base64(&BASE64_STANDARD.encode(image_jpeg)),
        media_type: Some(ImageMediaType::JPEG),
        // MiniMax rejects the "auto" detail rig sends by default (error 2013);
        // request high detail explicitly — better for reading brand/model text.
        detail: Some(ImageDetail::High),
        ..Default::default()
    };
    let draft = strip_thinking(&side_call(d, SIDE_PHOTO, PHOTO_BUDGET, &agent, image).await?);
    if draft.is_empty() {
        bail!("vision model returned no description besides thinking output");
    }
    Ok(draft)
}

const TRANSCRIBE_PREAMBLE: &str = "\
This is one page of a document: a booking confirmation, a ticket, an invoice \
or similar, saved as a picture. Write out all the text on it, exactly as \
written, in its own language and in reading order. Keep names, dates, times, \
numbers, prices and codes exactly as they appear. Do not translate, explain \
or summarise. Reply with the page's text only.";

/// How long one page may take to read. A page of a confirmation is a few
/// hundred words; a model that has not written them in a minute is not
/// going to.
pub const PAGE_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The text on one page of a document, read from its picture.
///
/// For a PDF whose text cannot be read as text — printed to PDF as drawn
/// shapes, or scanned — so what comes back is handed to the same extractor
/// a text PDF's words are. `page_jpeg` is a JPEG of the page.
pub async fn transcribe_page(d: &AgentDeps, page_jpeg: &[u8]) -> Result<String> {
    let agent = d.side.llm.agent(&d.side.name).preamble(TRANSCRIBE_PREAMBLE).build();
    let image = Image {
        data: DocumentSourceKind::base64(&BASE64_STANDARD.encode(page_jpeg)),
        media_type: Some(ImageMediaType::JPEG),
        // As for a photo: MiniMax refuses rig's default "auto", and small
        // print is the whole point here.
        detail: Some(ImageDetail::High),
        ..Default::default()
    };
    let text = strip_thinking(&side_call(d, SIDE_PAGE, PAGE_BUDGET, &agent, image).await?);
    Ok(text.trim().to_string())
}
