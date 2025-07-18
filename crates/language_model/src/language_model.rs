mod model;
mod rate_limiter;
mod registry;
mod request;
mod role;
mod telemetry;

#[cfg(any(test, feature = "test-support"))]
pub mod fake_provider;

use anthropic::{AnthropicError, parse_prompt_too_long};
use anyhow::Result;
use client::Client;
use futures::FutureExt;
use futures::{StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{AnyElement, AnyView, App, AsyncApp, SharedString, Task, Window};
use http_client::http;
use icons::IconName;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::ops::{Add, Sub};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, io};
use thiserror::Error;
use util::serde::is_default;
use zed_llm_client::CompletionRequestStatus;

pub use crate::model::*;
pub use crate::rate_limiter::*;
pub use crate::registry::*;
pub use crate::request::*;
pub use crate::role::*;
pub use crate::telemetry::*;

pub const ZED_CLOUD_PROVIDER_ID: &str = "zed.dev";

/// If we get a rate limit error that doesn't tell us when we can retry,
/// default to waiting this long before retrying.
const DEFAULT_RATE_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(4);

pub fn init(client: Arc<Client>, cx: &mut App) {
    init_settings(cx);
    RefreshLlmTokenListener::register(client.clone(), cx);
}

pub fn init_settings(cx: &mut App) {
    registry::init(cx);
}

/// Configuration for caching language model messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LanguageModelCacheConfiguration {
    pub max_cache_anchors: usize,
    pub should_speculate: bool,
    pub min_total_token: u64,
}

/// A completion event from a language model.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum LanguageModelCompletionEvent {
    StatusUpdate(CompletionRequestStatus),
    Stop(StopReason),
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse(LanguageModelToolUse),
    StartMessage {
        message_id: String,
    },
    UsageUpdate(TokenUsage),
}

#[derive(Error, Debug)]
pub enum LanguageModelCompletionError {
    #[error("rate limit exceeded, retry after {retry_after:?}")]
    RateLimitExceeded { retry_after: Duration },
    #[error("received bad input JSON")]
    BadInputJson {
        id: LanguageModelToolUseId,
        tool_name: Arc<str>,
        raw_input: Arc<str>,
        json_parse_error: String,
    },
    #[error("language model provider's API is overloaded")]
    Overloaded,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
    #[error("invalid request format to language model provider's API")]
    BadRequestFormat,
    #[error("authentication error with language model provider's API")]
    AuthenticationError,
    #[error("permission error with language model provider's API")]
    PermissionError,
    #[error("language model provider API endpoint not found")]
    ApiEndpointNotFound,
    #[error("prompt too large for context window")]
    PromptTooLarge { tokens: Option<u64> },
    #[error("internal server error in language model provider's API")]
    ApiInternalServerError,
    #[error("I/O error reading response from language model provider's API: {0:?}")]
    ApiReadResponseError(io::Error),
    #[error("HTTP response error from language model provider's API: status {status} - {body:?}")]
    HttpResponseError { status: u16, body: String },
    #[error("error serializing request to language model provider API: {0}")]
    SerializeRequest(serde_json::Error),
    #[error("error building request body to language model provider API: {0}")]
    BuildRequestBody(http::Error),
    #[error("error sending HTTP request to language model provider API: {0}")]
    HttpSend(anyhow::Error),
    #[error("error deserializing language model provider API response: {0}")]
    DeserializeResponse(serde_json::Error),
    #[error("unexpected language model provider API response format: {0}")]
    UnknownResponseFormat(String),
}

impl From<AnthropicError> for LanguageModelCompletionError {
    fn from(error: AnthropicError) -> Self {
        match error {
            AnthropicError::SerializeRequest(error) => Self::SerializeRequest(error),
            AnthropicError::BuildRequestBody(error) => Self::BuildRequestBody(error),
            AnthropicError::HttpSend(error) => Self::HttpSend(error),
            AnthropicError::DeserializeResponse(error) => Self::DeserializeResponse(error),
            AnthropicError::ReadResponse(error) => Self::ApiReadResponseError(error),
            AnthropicError::HttpResponseError { status, body } => {
                Self::HttpResponseError { status, body }
            }
            AnthropicError::RateLimit { retry_after } => Self::RateLimitExceeded { retry_after },
            AnthropicError::ApiError(api_error) => api_error.into(),
            AnthropicError::UnexpectedResponseFormat(error) => Self::UnknownResponseFormat(error),
        }
    }
}

impl From<anthropic::ApiError> for LanguageModelCompletionError {
    fn from(error: anthropic::ApiError) -> Self {
        use anthropic::ApiErrorCode::*;

        match error.code() {
            Some(code) => match code {
                InvalidRequestError => LanguageModelCompletionError::BadRequestFormat,
                AuthenticationError => LanguageModelCompletionError::AuthenticationError,
                PermissionError => LanguageModelCompletionError::PermissionError,
                NotFoundError => LanguageModelCompletionError::ApiEndpointNotFound,
                RequestTooLarge => LanguageModelCompletionError::PromptTooLarge {
                    tokens: parse_prompt_too_long(&error.message),
                },
                RateLimitError => LanguageModelCompletionError::RateLimitExceeded {
                    retry_after: DEFAULT_RATE_LIMIT_RETRY_AFTER,
                },
                ApiError => LanguageModelCompletionError::ApiInternalServerError,
                OverloadedError => LanguageModelCompletionError::Overloaded,
            },
            None => LanguageModelCompletionError::Other(error.into()),
        }
    }
}

/// Indicates the format used to define the input schema for a language model tool.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum LanguageModelToolSchemaFormat {
    /// A JSON schema, see https://json-schema.org
    JsonSchema,
    /// A subset of an OpenAPI 3.0 schema object supported by Google AI, see https://ai.google.dev/api/caching#Schema
    JsonSchemaSubset,
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "is_default")]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub cache_creation_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

impl Add<TokenUsage> for TokenUsage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens
                + other.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens + other.cache_read_input_tokens,
        }
    }
}

impl Sub<TokenUsage> for TokenUsage {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens - other.input_tokens,
            output_tokens: self.output_tokens - other.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens
                - other.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens - other.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct LanguageModelToolUseId(Arc<str>);

impl fmt::Display for LanguageModelToolUseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<T> From<T> for LanguageModelToolUseId
where
    T: Into<Arc<str>>,
{
    fn from(value: T) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct LanguageModelToolUse {
    pub id: LanguageModelToolUseId,
    pub name: Arc<str>,
    pub raw_input: String,
    pub input: serde_json::Value,
    pub is_input_complete: bool,
}

pub struct LanguageModelTextStream {
    pub message_id: Option<String>,
    pub stream: BoxStream<'static, Result<String, LanguageModelCompletionError>>,
    // Has complete token usage after the stream has finished
    pub last_token_usage: Arc<Mutex<TokenUsage>>,
}

impl Default for LanguageModelTextStream {
    fn default() -> Self {
        Self {
            message_id: None,
            stream: Box::pin(futures::stream::empty()),
            last_token_usage: Arc::new(Mutex::new(TokenUsage::default())),
        }
    }
}

pub trait LanguageModel: Send + Sync {
    fn id(&self) -> LanguageModelId;
    fn name(&self) -> LanguageModelName;
    fn provider_id(&self) -> LanguageModelProviderId;
    fn provider_name(&self) -> LanguageModelProviderName;
    fn telemetry_id(&self) -> String;

    fn api_key(&self, _cx: &App) -> Option<String> {
        None
    }

    /// Whether this model supports images
    fn supports_images(&self) -> bool;

    /// Whether this model supports tools.
    fn supports_tools(&self) -> bool;

    /// Whether this model supports choosing which tool to use.
    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool;

    /// Returns whether this model supports "burn mode";
    fn supports_max_mode(&self) -> bool {
        false
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchema
    }

    fn max_token_count(&self) -> u64;
    fn max_output_tokens(&self) -> Option<u64> {
        None
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &App,
    ) -> BoxFuture<'static, Result<u64>>;

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    >;

    fn stream_completion_text(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<LanguageModelTextStream, LanguageModelCompletionError>> {
        let future = self.stream_completion(request, cx);

        async move {
            let events = future.await?;
            let mut events = events.fuse();
            let mut message_id = None;
            let mut first_item_text = None;
            let last_token_usage = Arc::new(Mutex::new(TokenUsage::default()));

            if let Some(first_event) = events.next().await {
                match first_event {
                    Ok(LanguageModelCompletionEvent::StartMessage { message_id: id }) => {
                        message_id = Some(id.clone());
                    }
                    Ok(LanguageModelCompletionEvent::Text(text)) => {
                        first_item_text = Some(text);
                    }
                    _ => (),
                }
            }

            let stream = futures::stream::iter(first_item_text.map(Ok))
                .chain(events.filter_map({
                    let last_token_usage = last_token_usage.clone();
                    move |result| {
                        let last_token_usage = last_token_usage.clone();
                        async move {
                            match result {
                                Ok(LanguageModelCompletionEvent::StatusUpdate { .. }) => None,
                                Ok(LanguageModelCompletionEvent::StartMessage { .. }) => None,
                                Ok(LanguageModelCompletionEvent::Text(text)) => Some(Ok(text)),
                                Ok(LanguageModelCompletionEvent::Thinking { .. }) => None,
                                Ok(LanguageModelCompletionEvent::Stop(_)) => None,
                                Ok(LanguageModelCompletionEvent::ToolUse(_)) => None,
                                Ok(LanguageModelCompletionEvent::UsageUpdate(token_usage)) => {
                                    *last_token_usage.lock() = token_usage;
                                    None
                                }
                                Err(err) => Some(Err(err)),
                            }
                        }
                    }
                }))
                .boxed();

            Ok(LanguageModelTextStream {
                message_id,
                stream,
                last_token_usage,
            })
        }
        .boxed()
    }

    fn cache_configuration(&self) -> Option<LanguageModelCacheConfiguration> {
        None
    }

    #[cfg(any(test, feature = "test-support"))]
    fn as_fake(&self) -> &fake_provider::FakeLanguageModel {
        unimplemented!()
    }
}

#[derive(Debug, Error)]
pub enum LanguageModelKnownError {
    #[error("Context window limit exceeded ({tokens})")]
    ContextWindowLimitExceeded { tokens: u64 },
    #[error("Language model provider's API is currently overloaded")]
    Overloaded,
    #[error("Language model provider's API encountered an internal server error")]
    ApiInternalServerError,
    #[error("I/O error while reading response from language model provider's API: {0:?}")]
    ReadResponseError(io::Error),
    #[error("Error deserializing response from language model provider's API: {0:?}")]
    DeserializeResponse(serde_json::Error),
    #[error("Language model provider's API returned a response in an unknown format")]
    UnknownResponseFormat(String),
    #[error("Rate limit exceeded for language model provider's API; retry in {retry_after:?}")]
    RateLimitExceeded { retry_after: Duration },
}

impl LanguageModelKnownError {
    /// Attempts to map an HTTP response status code to a known error type.
    /// Returns None if the status code doesn't map to a specific known error.
    pub fn from_http_response(status: u16, _body: &str) -> Option<Self> {
        match status {
            429 => Some(Self::RateLimitExceeded {
                retry_after: DEFAULT_RATE_LIMIT_RETRY_AFTER,
            }),
            503 => Some(Self::Overloaded),
            500..=599 => Some(Self::ApiInternalServerError),
            _ => None,
        }
    }
}

pub trait LanguageModelTool: 'static + DeserializeOwned + JsonSchema {
    fn name() -> String;
    fn description() -> String;
}

/// An error that occurred when trying to authenticate the language model provider.
#[derive(Debug, Error)]
pub enum AuthenticateError {
    #[error("credentials not found")]
    CredentialsNotFound,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub trait LanguageModelProvider: 'static {
    fn id(&self) -> LanguageModelProviderId;
    fn name(&self) -> LanguageModelProviderName;
    fn icon(&self) -> IconName {
        IconName::ZedAssistant
    }
    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>>;
    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>>;
    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>>;
    fn recommended_models(&self, _cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        Vec::new()
    }
    fn is_authenticated(&self, cx: &App) -> bool;
    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>>;
    fn configuration_view(&self, window: &mut Window, cx: &mut App) -> AnyView;
    fn must_accept_terms(&self, _cx: &App) -> bool {
        false
    }
    fn render_accept_terms(
        &self,
        _view: LanguageModelProviderTosView,
        _cx: &mut App,
    ) -> Option<AnyElement> {
        None
    }
    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>>;
}

#[derive(PartialEq, Eq)]
pub enum LanguageModelProviderTosView {
    /// When there are some past interactions in the Agent Panel.
    ThreadtEmptyState,
    /// When there are no past interactions in the Agent Panel.
    ThreadFreshStart,
    PromptEditorPopup,
    Configuration,
}

pub trait LanguageModelProviderState: 'static {
    type ObservableEntity;

    fn observable_entity(&self) -> Option<gpui::Entity<Self::ObservableEntity>>;

    fn subscribe<T: 'static>(
        &self,
        cx: &mut gpui::Context<T>,
        callback: impl Fn(&mut T, &mut gpui::Context<T>) + 'static,
    ) -> Option<gpui::Subscription> {
        let entity = self.observable_entity()?;
        Some(cx.observe(&entity, move |this, _, cx| {
            callback(this, cx);
        }))
    }
}

#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd, Serialize, Deserialize)]
pub struct LanguageModelId(pub SharedString);

#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct LanguageModelName(pub SharedString);

#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct LanguageModelProviderId(pub SharedString);

#[derive(Clone, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct LanguageModelProviderName(pub SharedString);

impl fmt::Display for LanguageModelProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for LanguageModelId {
    fn from(value: String) -> Self {
        Self(SharedString::from(value))
    }
}

impl From<String> for LanguageModelName {
    fn from(value: String) -> Self {
        Self(SharedString::from(value))
    }
}

impl From<String> for LanguageModelProviderId {
    fn from(value: String) -> Self {
        Self(SharedString::from(value))
    }
}

impl From<String> for LanguageModelProviderName {
    fn from(value: String) -> Self {
        Self(SharedString::from(value))
    }
}
