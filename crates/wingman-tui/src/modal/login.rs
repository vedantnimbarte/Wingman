//! Provider login wizard — the `/login` modal.
//!
//! State machine:
//!   API-key flow:   PickProvider → EnterKey → EnterModel → Testing → Committing → Done
//!   OAuth flow:     PickProvider → OAuthPending → OAuthRunning → EnterModel → Committing → Done
//!   Local URL flow: PickProvider → EnterBaseUrl → EnterModel → Testing → Committing → Done
//!
//! Async work (probe, commit, OAuth login) is dispatched back to the host loop
//! via [`take_pending_task`] / [`task_completed`] — the modal itself stays sync.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

/// Stable id, display label, default model, whether it needs an API key,
/// the default base URL for local providers, and whether it uses OAuth.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        label: "Anthropic (Claude)",
        default_model: "claude-opus-4-7",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "openai",
        label: "OpenAI (API key)",
        default_model: "gpt-4.1",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "chatgpt",
        label: "ChatGPT (subscription)",
        default_model: "gpt-4o",
        needs_key: false,
        needs_oauth: true,
        default_base_url: None,
    },
    ProviderSpec {
        id: "openrouter",
        label: "OpenRouter",
        default_model: "anthropic/claude-opus-4-7",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "gemini",
        label: "Google Gemini",
        default_model: "gemini-2.5-pro",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "litellm",
        label: "LiteLLM proxy",
        default_model: "anthropic/claude-opus-4-7",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some("http://localhost:4000/v1"),
    },
    ProviderSpec {
        id: "lmstudio",
        label: "LM Studio (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:1234/v1"),
    },
    ProviderSpec {
        id: "vllm",
        label: "vLLM (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:8000/v1"),
    },
    ProviderSpec {
        id: "ollama",
        label: "Ollama (local)",
        default_model: "llama3.1:8b",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:11434/v1"),
    },
    ProviderSpec {
        id: "groq",
        label: "Groq",
        default_model: "llama-3.3-70b-versatile",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "together",
        label: "Together AI",
        default_model: "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "fireworks",
        label: "Fireworks AI",
        default_model: "accounts/fireworks/models/llama-v3p1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "deepinfra",
        label: "DeepInfra",
        default_model: "meta-llama/Meta-Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "perplexity",
        label: "Perplexity (Sonar)",
        default_model: "sonar-pro",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "xai",
        label: "xAI (Grok)",
        default_model: "grok-2-latest",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "deepseek",
        label: "DeepSeek",
        default_model: "deepseek-chat",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "mistral",
        label: "Mistral La Plateforme",
        default_model: "mistral-large-latest",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "cerebras",
        label: "Cerebras",
        default_model: "llama3.1-70b",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "sambanova",
        label: "SambaNova",
        default_model: "Meta-Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "azure",
        label: "Azure OpenAI",
        default_model: "gpt-4o",
        needs_key: true,
        needs_oauth: false,
        // User MUST replace with their resource/deployment URL.
        default_base_url: Some(
            "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT",
        ),
    },
    ProviderSpec {
        id: "github",
        label: "GitHub Models",
        default_model: "gpt-4o",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "llamacpp",
        label: "llama.cpp server (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:8080/v1"),
    },
    ProviderSpec {
        id: "tgi",
        label: "HuggingFace TGI (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:3000/v1"),
    },
    // Wave 2 hosted.
    ProviderSpec {
        id: "anyscale",
        label: "Anyscale Endpoints",
        default_model: "meta-llama/Meta-Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "lepton",
        label: "Lepton AI",
        default_model: "llama3-1-70b",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "replicate",
        label: "Replicate (OpenAI proxy)",
        default_model: "meta/meta-llama-3.1-405b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "novita",
        label: "Novita AI",
        default_model: "meta-llama/llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "hyperbolic",
        label: "Hyperbolic",
        default_model: "meta-llama/Meta-Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "lambda",
        label: "Lambda Inference",
        default_model: "llama3.1-70b-instruct-fp8",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "nebius",
        label: "Nebius AI Studio",
        default_model: "meta-llama/Meta-Llama-3.1-70B-Instruct-fast",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "hf",
        label: "HuggingFace Inference",
        default_model: "meta-llama/Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "glhf",
        label: "GLHF.chat",
        default_model: "hf:meta-llama/Llama-3.1-70B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "featherless",
        label: "Featherless",
        default_model: "meta-llama/Meta-Llama-3.1-8B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "octoai",
        label: "OctoAI",
        default_model: "meta-llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "nvidia",
        label: "NVIDIA NIM",
        default_model: "meta/llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "avian",
        label: "Avian",
        default_model: "Meta-Llama-3.1-405B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "kluster",
        label: "Kluster.ai",
        default_model: "klusterai/Meta-Llama-3.1-405B-Instruct-Turbo",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "inferencenet",
        label: "Inference.net",
        default_model: "meta-llama/llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "snowflake",
        label: "Snowflake Cortex",
        default_model: "llama3.1-70b",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some(
            "https://YOUR-ACCOUNT.snowflakecomputing.com/api/v2/cortex/inference/v1",
        ),
    },
    ProviderSpec {
        id: "databricks",
        label: "Databricks Foundation",
        default_model: "databricks-meta-llama-3-1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some(
            "https://YOUR-WORKSPACE.cloud.databricks.com/serving-endpoints/v1",
        ),
    },
    ProviderSpec {
        id: "writer",
        label: "Writer Palmyra",
        default_model: "palmyra-x5",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "cohere",
        label: "Cohere (Command R/A)",
        default_model: "command-r-plus",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    // Wave 3 hosted: Chinese clouds.
    ProviderSpec {
        id: "qwen",
        label: "Alibaba Qwen (DashScope)",
        default_model: "qwen-max",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "zhipu",
        label: "Zhipu GLM",
        default_model: "glm-4-plus",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "moonshot",
        label: "Moonshot Kimi",
        default_model: "moonshot-v1-128k",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "minimax",
        label: "MiniMax",
        default_model: "abab6.5s-chat",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "yi",
        label: "Yi (01.AI)",
        default_model: "yi-large",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "baichuan",
        label: "Baichuan",
        default_model: "Baichuan4-Turbo",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "hunyuan",
        label: "Tencent Hunyuan",
        default_model: "hunyuan-pro",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "doubao",
        label: "ByteDance Doubao (Ark)",
        default_model: "doubao-pro-32k",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "siliconflow",
        label: "SiliconFlow",
        default_model: "Qwen/Qwen2.5-72B-Instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    // Wave 3 hosted: aggregators / gateways.
    ProviderSpec {
        id: "cloudflare",
        label: "Cloudflare Workers AI",
        default_model: "@cf/meta/llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some(
            "https://api.cloudflare.com/client/v4/accounts/YOUR-ACCOUNT-ID/ai/v1",
        ),
    },
    ProviderSpec {
        id: "vercel",
        label: "Vercel AI Gateway",
        default_model: "openai/gpt-4o",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "aimlapi",
        label: "AIMLAPI",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "openpipe",
        label: "OpenPipe",
        default_model: "openpipe:meta-llama-3.1-70b",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "targon",
        label: "Targon (Bittensor)",
        default_model: "NousResearch/Hermes-3-Llama-3.1-70B",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "pollinations",
        label: "Pollinations (free)",
        default_model: "openai",
        needs_key: false,
        needs_oauth: false,
        default_base_url: None,
    },
    // Wave 3 hosted: other.
    ProviderSpec {
        id: "ai21",
        label: "AI21 Jamba",
        default_model: "jamba-1.5-large",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "zai",
        label: "Z.ai (GLM coding)",
        default_model: "glm-4-plus",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "friendli",
        label: "Friendli AI",
        default_model: "meta-llama-3.1-70b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "mancer",
        label: "Mancer",
        default_model: "weaver",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    ProviderSpec {
        id: "reka",
        label: "Reka",
        default_model: "reka-core",
        needs_key: true,
        needs_oauth: false,
        default_base_url: None,
    },
    // Wave 3 local runtimes.
    ProviderSpec {
        id: "mlx",
        label: "mlx-lm-server (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:8080/v1"),
    },
    ProviderSpec {
        id: "localai",
        label: "LocalAI (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:8080/v1"),
    },
    ProviderSpec {
        id: "aphrodite",
        label: "Aphrodite Engine (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:2242/v1"),
    },
    ProviderSpec {
        id: "mistralrs",
        label: "Mistral.rs server (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:1234/v1"),
    },
    // Wave 2 local.
    ProviderSpec {
        id: "gpt4all",
        label: "GPT4All (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:4891/v1"),
    },
    ProviderSpec {
        id: "jan",
        label: "Jan / Cortex (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:1337/v1"),
    },
    ProviderSpec {
        id: "koboldcpp",
        label: "KoboldCpp (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:5001/v1"),
    },
    ProviderSpec {
        id: "oobabooga",
        label: "Oobabooga (local)",
        default_model: "local-model",
        needs_key: false,
        needs_oauth: false,
        default_base_url: Some("http://localhost:5000/v1"),
    },
    // Wave 4: enterprise clouds.
    ProviderSpec {
        id: "bedrock",
        label: "AWS Bedrock (API key)",
        default_model: "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1"),
    },
    ProviderSpec {
        id: "vertex",
        label: "GCP Vertex AI (access token)",
        default_model: "google/gemini-1.5-pro-002",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/YOUR-PROJECT/locations/us-central1/endpoints/openapi",
        ),
    },
    ProviderSpec {
        id: "watsonx",
        label: "IBM watsonx.ai",
        default_model: "ibm/granite-3-8b-instruct",
        needs_key: true,
        needs_oauth: false,
        default_base_url: Some("https://us-south.ml.cloud.ibm.com"),
    },
];

#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub default_model: &'static str,
    pub needs_key: bool,
    /// Uses browser-based OAuth instead of an API key.
    pub needs_oauth: bool,
    pub default_base_url: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    PickProvider,
    EnterKey,
    EnterBaseUrl,
    EnterModel,
    /// OAuth: waiting for the user to press Enter to open the browser.
    OAuthPending,
    /// OAuth: browser is open, waiting for the callback to complete.
    OAuthRunning,
    #[allow(dead_code)]
    Testing,
    Committing,
    Done,
}

/// Snapshot of the wizard fields the host needs to perform Probe / Commit.
#[derive(Debug, Clone)]
pub struct LoginPayload {
    pub provider_id: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: String,
}

/// An async task the wizard is waiting on the host to perform.
#[derive(Debug, Clone)]
pub enum LoginTask {
    Probe(LoginPayload),
    Commit(LoginPayload),
    /// Perform the ChatGPT OAuth browser login and store both tokens in the
    /// keychain.  Reports `Ok(())` on success.
    OAuthLogin {
        provider_id: String,
    },
}

#[derive(Debug)]
pub struct LoginWizard {
    stage: Stage,
    provider_idx: usize,
    api_key: String,
    base_url: String,
    model: String,
    /// Set when the most recent async task failed. Cleared on retry.
    error: Option<String>,
    /// Task the host should pick up and execute.
    pending: Option<LoginTask>,
}

impl LoginWizard {
    pub fn new() -> Self {
        let spec = PROVIDERS[0];
        Self {
            stage: Stage::PickProvider,
            provider_idx: 0,
            api_key: String::new(),
            base_url: spec.default_base_url.unwrap_or("").to_string(),
            model: spec.default_model.to_string(),
            error: None,
            pending: None,
        }
    }

    fn spec(&self) -> ProviderSpec {
        PROVIDERS[self.provider_idx]
    }

    fn select_provider(&mut self, idx: usize) {
        self.provider_idx = idx.min(PROVIDERS.len() - 1);
        let spec = self.spec();
        self.api_key.clear();
        self.base_url = spec.default_base_url.unwrap_or("").to_string();
        self.model = spec.default_model.to_string();
    }

    fn payload(&self) -> LoginPayload {
        let spec = self.spec();
        LoginPayload {
            provider_id: spec.id.to_string(),
            api_key: if spec.needs_key && !self.api_key.is_empty() {
                Some(self.api_key.clone())
            } else {
                None
            },
            base_url: if self.base_url.is_empty() {
                None
            } else {
                Some(self.base_url.clone())
            },
            model: self.model.clone(),
        }
    }

    /// Drain the pending async task.
    pub fn take_pending_task(&mut self) -> Option<LoginTask> {
        self.pending.take()
    }

    /// Non-draining peek: is an async task queued for the host to run?
    pub fn has_pending_task(&self) -> bool {
        self.pending.is_some()
    }

    /// Host reports completion of the most recently dispatched task.
    pub fn task_completed(&mut self, result: Result<(), String>) {
        match (self.stage, result) {
            (Stage::OAuthRunning, Ok(())) => {
                // OAuth succeeded — advance to model selection.
                self.stage = Stage::EnterModel;
            }
            (Stage::OAuthRunning, Err(msg)) => {
                self.error = Some(msg);
                // Let user retry by pressing Enter again.
                self.stage = Stage::OAuthPending;
            }
            (Stage::Testing, Ok(())) => {
                self.stage = Stage::Committing;
                self.pending = Some(LoginTask::Commit(self.payload()));
            }
            (Stage::Committing, Ok(())) => {
                self.stage = Stage::Done;
            }
            (_, Err(msg)) => {
                self.error = Some(msg);
                self.stage = Stage::EnterModel;
            }
            _ => {}
        }
    }

    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    pub fn final_payload(&self) -> LoginPayload {
        self.payload()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        if matches!(
            self.stage,
            Stage::OAuthRunning | Stage::Testing | Stage::Committing | Stage::Done
        ) {
            return ModalOutcome::Continue;
        }
        self.error = None;
        match self.stage {
            Stage::PickProvider => self.handle_pick_provider(k),
            Stage::EnterKey => self.handle_text(k, TextField::Key),
            Stage::EnterBaseUrl => self.handle_text(k, TextField::BaseUrl),
            Stage::EnterModel => self.handle_text(k, TextField::Model),
            Stage::OAuthPending => self.handle_oauth_pending(k),
            _ => ModalOutcome::Continue,
        }
    }

    fn handle_pick_provider(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up => {
                if self.provider_idx == 0 {
                    self.select_provider(PROVIDERS.len() - 1);
                } else {
                    self.select_provider(self.provider_idx - 1);
                }
            }
            KeyCode::Down => {
                self.select_provider((self.provider_idx + 1) % PROVIDERS.len());
            }
            KeyCode::Enter => {
                let spec = self.spec();
                self.stage = if spec.needs_oauth {
                    Stage::OAuthPending
                } else if spec.needs_key {
                    Stage::EnterKey
                } else {
                    Stage::EnterBaseUrl
                };
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    fn handle_oauth_pending(&mut self, k: KeyEvent) -> ModalOutcome {
        if k.code == KeyCode::Enter {
            self.stage = Stage::OAuthRunning;
            self.pending = Some(LoginTask::OAuthLogin {
                provider_id: self.spec().id.to_string(),
            });
        }
        ModalOutcome::Continue
    }

    fn handle_text(&mut self, k: KeyEvent, field: TextField) -> ModalOutcome {
        let buf = match field {
            TextField::Key => &mut self.api_key,
            TextField::BaseUrl => &mut self.base_url,
            TextField::Model => &mut self.model,
        };
        match k.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => {
                let spec = self.spec();
                match field {
                    TextField::Key => {
                        if self.api_key.trim().is_empty() {
                            self.error = Some("API key required".into());
                            return ModalOutcome::Continue;
                        }
                        if spec.default_base_url.is_some() {
                            self.stage = Stage::EnterBaseUrl;
                        } else {
                            self.stage = Stage::EnterModel;
                        }
                    }
                    TextField::BaseUrl => {
                        self.stage = Stage::EnterModel;
                    }
                    TextField::Model => {
                        if self.model.trim().is_empty() {
                            self.error = Some("model required".into());
                            return ModalOutcome::Continue;
                        }
                        self.stage = Stage::Committing;
                        self.pending = Some(LoginTask::Commit(self.payload()));
                    }
                }
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 70, 70);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /login — connect a provider ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(2),
            ])
            .split(inner);

        self.render_step(chunks[0], buf);
        self.render_body(chunks[1], buf);
        self.render_hint(chunks[2], buf);
    }

    fn render_step(&self, area: Rect, buf: &mut Buffer) {
        let label = match self.stage {
            Stage::PickProvider => "Step 1/3 · pick a provider",
            Stage::EnterKey => "Step 2/3 · enter API key",
            Stage::EnterBaseUrl => "Step 2/3 · base URL",
            Stage::OAuthPending => "Step 2/3 · authenticate via browser",
            Stage::OAuthRunning => "Step 2/3 · browser authentication in progress",
            Stage::EnterModel => "Step 3/3 · pick model",
            Stage::Testing => "testing connection",
            Stage::Committing => "saving",
            Stage::Done => "Done",
        };
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(Color::DarkGray),
        )))
        .render(area, buf);
    }

    fn render_body(&self, area: Rect, buf: &mut Buffer) {
        match self.stage {
            Stage::PickProvider => {
                let items: Vec<ListItem> = PROVIDERS
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let marker = if i == self.provider_idx { "› " } else { "  " };
                        let style = if i == self.provider_idx {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{marker}{}", p.label),
                            style,
                        )))
                    })
                    .collect();
                List::new(items).render(area, buf);
            }
            Stage::EnterKey => {
                let masked: String = "*".repeat(self.api_key.chars().count());
                let line = Line::from(vec![
                    Span::styled("API key: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(masked, Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::EnterBaseUrl => {
                let line = Line::from(vec![
                    Span::styled("Base URL: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.base_url.clone(), Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::OAuthPending => {
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "Your default browser will open to authenticate with ChatGPT.",
                        Style::default().fg(Color::White),
                    )),
                    Line::from(Span::styled(
                        "A ChatGPT Plus or Pro subscription is required.",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Press Enter to open browser…",
                        Style::default().fg(Color::Cyan),
                    )),
                ])
                .render(area, buf);
            }
            Stage::OAuthRunning => {
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "Browser opened — complete authentication there.",
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(Span::styled(
                        "Waiting for callback on localhost:1455…",
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
                .render(area, buf);
            }
            Stage::EnterModel => {
                let line = Line::from(vec![
                    Span::styled("Model: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.model.clone(), Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::Testing => {
                Paragraph::new(Line::from(Span::styled(
                    "Testing connection…",
                    Style::default().fg(Color::Yellow),
                )))
                .render(area, buf);
            }
            Stage::Committing => {
                Paragraph::new(Line::from(Span::styled(
                    "Saving credentials…",
                    Style::default().fg(Color::Yellow),
                )))
                .render(area, buf);
            }
            Stage::Done => {
                Paragraph::new(Line::from(Span::styled(
                    "Saved.",
                    Style::default().fg(Color::Green),
                )))
                .render(area, buf);
            }
        }
    }

    fn render_hint(&self, area: Rect, buf: &mut Buffer) {
        let line = if let Some(err) = &self.error {
            Line::from(vec![
                Span::styled("error: ", Style::default().fg(Color::Red)),
                Span::styled(err.clone(), Style::default().fg(Color::Red)),
            ])
        } else {
            let hint = match self.stage {
                Stage::PickProvider => "↑/↓ navigate · Enter select · Esc cancel",
                Stage::EnterKey => "type key (hidden) · Enter continue · Esc cancel",
                Stage::EnterBaseUrl | Stage::EnterModel => {
                    "Enter continue · Backspace edit · Esc cancel"
                }
                Stage::OAuthPending => "Enter open browser · Esc cancel",
                Stage::OAuthRunning => "(waiting for browser auth…)",
                Stage::Testing | Stage::Committing => "(working…)",
                Stage::Done => "press Esc to close",
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        };
        Paragraph::new(line).render(area, buf);
    }
}

impl Default for LoginWizard {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum TextField {
    Key,
    BaseUrl,
    Model,
}
