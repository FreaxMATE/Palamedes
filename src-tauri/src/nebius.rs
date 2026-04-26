use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, ChatCompletionRequestAssistantMessageArgs,
        CreateChatCompletionRequestArgs, CreateEmbeddingRequestArgs, EmbeddingInput,
    },
    Client,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::extraction::{build_prompt, parse_response, BeliefDraft, TurnContext};
use crate::summarization::{
    build_prompt as build_summary_prompt, parse_response as parse_summary_response, SummarizationContext, SummaryDraft,
};

const NEBIUS_BASE_URL: &str = "https://api.tokenfactory.nebius.com/v1";

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

        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(NEBIUS_BASE_URL);

        Ok(Self {
            client: Client::with_config(config),
        })
    }

    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let models = self.client.models().list().await?;
        let mut ids: Vec<String> = models.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Stream a chat completion. Calls `on_chunk` for each text delta.
    /// Returns when the stream ends naturally, is cancelled, or errors.
    pub async fn stream_chat<F>(
        &self,
        model: &str,
        messages: Vec<Message>,
        cancel: CancellationToken,
        mut on_chunk: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&str),
    {
        let oai_messages: Vec<ChatCompletionRequestMessage> = messages
            .into_iter()
            .map(|m| match m.role {
                Role::System => ChatCompletionRequestSystemMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map(Into::into),
                Role::User => ChatCompletionRequestUserMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map(Into::into),
                Role::Assistant => ChatCompletionRequestAssistantMessageArgs::default()
                    .content(m.content)
                    .build()
                    .map(Into::into),
            })
            .collect::<Result<_, _>>()?;

        let request = CreateChatCompletionRequestArgs::default()
            .model(model)
            .messages(oai_messages)
            .stream(true)
            .build()?;

        let mut stream = self.client.chat().create_stream(request).await?;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    log::info!("stream cancelled by user");
                    return Ok(());
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(response)) => {
                            for choice in response.choices {
                                if let Some(content) = choice.delta.content {
                                    on_chunk(&content);
                                }
                            }
                        }
                        Some(Err(e)) => return Err(e.into()),
                        None => return Ok(()),
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
