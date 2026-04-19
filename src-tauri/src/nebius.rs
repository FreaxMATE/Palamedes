use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, ChatCompletionRequestAssistantMessageArgs,
        CreateChatCompletionRequestArgs,
    },
    Client,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

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

pub struct NebiusClient {
    client: Client<OpenAIConfig>,
    model: String,
}

impl NebiusClient {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("NEBIUS_API_KEY")
            .map_err(|_| anyhow::anyhow!("NEBIUS_API_KEY not set (check .env)"))?;
        let model = std::env::var("PALAMEDES_MODEL")
            .unwrap_or_else(|_| "moonshotai/Kimi-K2.5".to_string());

        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(NEBIUS_BASE_URL);

        Ok(Self {
            client: Client::with_config(config),
            model,
        })
    }

    /// Stream a chat completion. Calls `on_chunk` for each text delta.
    /// Returns when the stream ends naturally, is cancelled, or errors.
    pub async fn stream_chat<F>(
        &self,
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
            .model(&self.model)
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
}
