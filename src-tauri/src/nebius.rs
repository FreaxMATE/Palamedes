use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, CreateEmbeddingRequestArgs, EmbeddingInput,
    },
    Client,
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::extraction::{build_prompt, parse_response, BeliefDraft, TurnContext};
use crate::summarization::{
    build_prompt as build_summary_prompt, parse_response as parse_summary_response, SummarizationContext, SummaryDraft,
};

const NEBIUS_BASE_URL: &str = "https://api.tokenfactory.nebius.com/v1";

/// Small, fast, non-thinking instruct model used for one-shot keyword
/// extraction (graph labels, cluster names). Hardcoded because it must
/// NOT be a reasoning model — Kimi K2.5 and other CoT models route their
/// output to `reasoning_content` and leave `content` null, which strips
/// to empty and silently breaks the label/name path.
const LABEL_MODEL: &str = "Qwen/Qwen3-30B-A3B-Instruct-2507";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Clone)]
pub struct NebiusClient {
    client: Client<OpenAIConfig>,
    api_key: String,
    http: reqwest::Client,
}

/// One streaming chunk from the chat endpoint. Either part of the visible
/// reply (`content`) or part of the model's chain-of-thought (`reasoning`).
/// Reasoning models like Kimi K2.5 and DeepSeek-V3.2 emit both in parallel
/// over the same SSE stream; non-thinking models only emit `Content`.
#[derive(Debug, Clone)]
pub enum StreamPiece {
    Content(String),
    Reasoning(String),
}

/// Raw result of a single extraction call: the drafted beliefs plus the raw
/// response string (useful for logging when parsing fails).
pub struct ExtractionOutcome {
    pub drafts: Vec<BeliefDraft>,
    pub raw_response: String,
}

pub struct SummarizationOutcome {
    pub summaries: Vec<SummaryDraft>,
    pub raw_response: String,
}

impl NebiusClient {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("NEBIUS_API_KEY")
            .map_err(|_| anyhow::anyhow!("NEBIUS_API_KEY not set (check .env)"))?;
        Ok(Self::from_api_key(api_key))
    }

    /// Construct a client from an explicit API key. Used by tests that
    /// don't need to hit the network — the API key is required for the
    /// struct but unused by code paths that don't call out.
    pub fn from_api_key(api_key: String) -> Self {
        let config = OpenAIConfig::new()
            .with_api_key(api_key.clone())
            .with_api_base(NEBIUS_BASE_URL);
        Self {
            client: Client::with_config(config),
            api_key,
            http: reqwest::Client::new(),
        }
    }

    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let models = self.client.models().list().await?;
        let mut ids: Vec<String> = models.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Stream a chat completion. Calls `on_piece` for each delta — either
    /// visible `Content` or model `Reasoning` chain-of-thought. Returns when
    /// the stream ends naturally, is cancelled, or errors.
    ///
    /// We bypass async-openai's typed stream because its 0.30 schema lacks
    /// the `reasoning` delta field that Nebius (and any thinking-model
    /// provider) emits parallel to `content`. Parsing the SSE ourselves
    /// with serde_json::Value lets us see both without forking the SDK.
    pub async fn stream_chat<F>(
        &self,
        model: &str,
        messages: Vec<Message>,
        cancel: CancellationToken,
        mut on_piece: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(StreamPiece),
    {
        let body = serde_json::json!({
            "model": model,
            "messages": messages.iter().map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                serde_json::json!({ "role": role, "content": m.content })
            }).collect::<Vec<_>>(),
            "stream": true,
        });

        let response = self
            .http
            .post(format!("{}/chat/completions", NEBIUS_BASE_URL))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("chat stream HTTP {}: {}", status, text));
        }

        let mut events = response.bytes_stream().eventsource();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("stream cancelled by user");
                    return Ok(());
                }
                event = events.next() => {
                    let event = match event {
                        Some(Ok(e)) => e,
                        Some(Err(e)) => return Err(anyhow::anyhow!("sse error: {}", e)),
                        None => return Ok(()),
                    };
                    if event.data == "[DONE]" {
                        return Ok(());
                    }
                    let json: serde_json::Value = match serde_json::from_str(&event.data) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    let Some(choices) = json.get("choices").and_then(|c| c.as_array()) else {
                        continue;
                    };
                    for choice in choices {
                        let Some(delta) = choice.get("delta") else { continue };
                        // `reasoning` is the chain-of-thought field; some providers
                        // also use `reasoning_content`. Check both to be safe.
                        if let Some(r) = delta
                            .get("reasoning")
                            .or_else(|| delta.get("reasoning_content"))
                            .and_then(|v| v.as_str())
                        {
                            if !r.is_empty() {
                                on_piece(StreamPiece::Reasoning(r.to_string()));
                            }
                        }
                        if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                            if !c.is_empty() {
                                on_piece(StreamPiece::Content(c.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Single-shot belief extraction: build the prompt, call the model,
    /// parse the JSON. Returns drafts + raw response for logging.
    pub async fn extract_beliefs(
        &self,
        model: &str,
        ctx: TurnContext,
    ) -> anyhow::Result<ExtractionOutcome> {
        let prompt = build_prompt(&ctx);

        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?;

        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages(vec![message.into()])
            .temperature(0.0)
            .build()?;

        let response = self.client.chat().create(request).await?;
        let raw_response = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        let drafts = parse_response(&raw_response)?;
        Ok(ExtractionOutcome { drafts, raw_response })
    }

    /// Embed a single text as a *document*. Returns a fixed-size float vector.
    /// Caller is responsible for L2-normalizing before storage if cosine
    /// similarity is desired. Use this for beliefs/notes/anything stored in
    /// the corpus.
    pub async fn embed(&self, model: &str, text: &str) -> anyhow::Result<Vec<f32>> {
        self.embed_raw(model, text).await
    }

    /// Embed a *query* with the asymmetric instruct-prefix that
    /// Qwen3-Embedding (and most modern embedding models) expects. Without
    /// this, recall against documents drops noticeably because the model
    /// uses different internal representations for queries vs documents.
    /// For non-Qwen3 models the prefix is harmless: it just adds a tiny
    /// constant signal that gets normalized out.
    pub async fn embed_query(&self, model: &str, query: &str) -> anyhow::Result<Vec<f32>> {
        let wrapped = format!(
            "Instruct: Given a question or statement from the user, retrieve relevant beliefs about the user that help answer or contextualize it.\nQuery: {}",
            query
        );
        self.embed_raw(model, &wrapped).await
    }

    async fn embed_raw(&self, model: &str, text: &str) -> anyhow::Result<Vec<f32>> {
        let request = CreateEmbeddingRequestArgs::default()
            .model(model)
            .input(EmbeddingInput::String(text.to_string()))
            .build()?;
        let response = self.client.embeddings().create(request).await?;
        let vec = response
            .data
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding response had no data"))?
            .embedding;
        Ok(vec)
    }

    /// Generate a short, evocative name for a cluster of belief statements.
    /// Used by the memory map to label constellations / regions / districts.
    /// Stays short (2–4 words) and is cheap (~50 tokens out).
    pub async fn name_cluster(&self, statements: &[String]) -> anyhow::Result<String> {
        let bullet_list = statements
            .iter()
            .take(20)
            .map(|s| format!("- {}", s))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Give a short evocative name (2 to 4 words) for this cluster of beliefs about a person. \
             Pick something poetic but specific — like a region or constellation name. \
             Output ONLY the name, no quotes, no explanation, no punctuation at the end.\n\n\
             BELIEFS:\n{}",
            bullet_list
        );

        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?;
        let request = CreateChatCompletionRequestArgs::default()
            .model(LABEL_MODEL)
            .messages(vec![message.into()])
            .temperature(0.7)
            .max_tokens(50_u32)
            .build()?;
        let response = self.client.chat().create(request).await?;
        let raw = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        // Defensive cleanup: model sometimes adds quotes or trailing periods.
        let name = raw
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim_end_matches('.')
            .trim()
            .to_string();
        Ok(name)
    }

    /// Boil a belief statement down to a 1–4 word keyword label for the
    /// memory map. Always uses `LABEL_MODEL` (a small, fast, non-thinking
    /// instruct model). Reasoning models like Kimi K2.5 emit nothing into
    /// `content` for short prompts — their output goes to `reasoning_content`
    /// — so they can never finish a one-shot keyword task.
    pub async fn extract_label(&self, statement: &str) -> anyhow::Result<String> {
        let prompt = format!(
            "Summarize this belief about a person as 1 to 4 keywords for a graph node label. \
             Use specific nouns and topics, not full sentences. Drop articles, pronouns, \
             and verbs like 'enjoys' or 'has'. Lowercase unless a proper noun. \
             Output ONLY the keywords, no quotes, no punctuation, no explanation.\n\n\
             Examples:\n\
             - \"The user enjoys playing racket sports including tennis and squash\" → racket sports\n\
             - \"He is a native German speaker with basic Swedish\" → german, swedish\n\
             - \"The user has experience in quantitative energy trading\" → quant energy trading\n\
             - \"The user prefers concise direct answers\" → concise replies\n\n\
             BELIEF: {}",
            statement
        );

        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?;
        let request = CreateChatCompletionRequestArgs::default()
            .model(LABEL_MODEL)
            .messages(vec![message.into()])
            .temperature(0.2)
            .max_tokens(32_u32)
            .build()?;
        let response = self.client.chat().create(request).await?;
        let raw = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        let label = raw
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim_end_matches('.')
            .trim_end_matches(',')
            .trim()
            .to_string();
        if label.is_empty() {
            return Err(anyhow::anyhow!(
                "label model returned empty (raw: {:?})",
                raw
            ));
        }
        Ok(label)
    }

    /// Single-shot summarization: cluster + summarize a category's beliefs.
    pub async fn summarize(
        &self,
        model: &str,
        ctx: SummarizationContext,
    ) -> anyhow::Result<SummarizationOutcome> {
        let prompt = build_summary_prompt(&ctx);

        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()?;

        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages(vec![message.into()])
            .temperature(0.0)
            .build()?;

        let response = self.client.chat().create(request).await?;
        let raw_response = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        let summaries = parse_summary_response(&raw_response)?;
        Ok(SummarizationOutcome { summaries, raw_response })
    }
}
