//! Model providers: Anthropic Messages, OpenAI-compatible, mock.
//!
//! Each HTTP provider retries failures that happen before the response stream starts
//! (connect errors, non-2xx statuses) and never retries a stream that is already flowing.

mod anthropic;
mod common;
mod mock;
mod openai;
mod registry;
mod retry;
mod sse;

pub use anthropic::AnthropicProvider;
pub use mock::{MockProvider, MockTurn};
pub use openai::OpenAiProvider;
pub use registry::{ProviderRegistry, build_provider, http_client};
pub use retry::{with_retries, with_retries_base};
pub use sse::{SseEvent, SseParser};
