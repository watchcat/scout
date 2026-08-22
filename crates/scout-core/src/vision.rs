use crate::agent::{LlmClient, MODEL};
use crate::text::strip_thinking;
use anyhow::{bail, Result};
use base64::prelude::{Engine, BASE64_STANDARD};
use rig::client::CompletionClient;
use rig::completion::message::Image;
use rig::completion::Prompt;
use rig::message::{DocumentSourceKind, ImageDetail, ImageMediaType};

const VISION_PREAMBLE: &str = "\
Look at the product in the photo and describe it as one concise web-search \
query: brand and model if identifiable, otherwise category plus distinguishing \
features (color, material, style). Reply with the query text only - no \
explanations, no quotes.";

/// Telegram photos are always JPEG.
pub async fn describe_photo(
    llm: &LlmClient,
    image_jpeg: &[u8],
    caption: Option<&str>,
) -> Result<String> {
    let mut preamble = VISION_PREAMBLE.to_string();
    if let Some(c) = caption.map(str::trim).filter(|c| !c.is_empty()) {
        preamble.push_str(&format!("\nUser note about this photo: {c}"));
    }
    let agent = llm.agent(MODEL).preamble(&preamble).build();
    let image = Image {
        data: DocumentSourceKind::base64(&BASE64_STANDARD.encode(image_jpeg)),
        media_type: Some(ImageMediaType::JPEG),
        // MiniMax rejects the "auto" detail rig sends by default (error 2013);
        // request high detail explicitly — better for reading brand/model text.
        detail: Some(ImageDetail::High),
        ..Default::default()
    };
    let draft = strip_thinking(&agent.prompt(image).await?);
    if draft.is_empty() {
        bail!("vision model returned no description besides thinking output");
    }
    Ok(draft)
}
