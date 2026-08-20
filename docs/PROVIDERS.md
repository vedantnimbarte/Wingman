# Supported Providers

Every backend Wingman speaks to, and which ones are validated for pilot mode.

| Provider           | id          | Env var                  | Default base URL                                  |
| ------------------ | ----------- | ------------------------ | ------------------------------------------------- |
| Anthropic          | `anthropic` | `ANTHROPIC_API_KEY`      | (native adapter)                                  |
| Google Gemini      | `gemini`    | `GOOGLE_API_KEY`         | (native adapter)                                  |
| ChatGPT (OAuth)    | `chatgpt`   | OAuth via `/login`       | (token in OS keychain)                            |
| OpenAI             | `openai`    | `OPENAI_API_KEY`         | `https://api.openai.com/v1`                       |
| OpenRouter         | `openrouter`| `OPENROUTER_API_KEY`     | `https://openrouter.ai/api/v1`                    |
| LiteLLM            | `litellm`   | `LITELLM_API_KEY`        | `http://localhost:4000/v1`                        |
| Groq               | `groq`      | `GROQ_API_KEY`           | `https://api.groq.com/openai/v1`                  |
| Together AI        | `together`  | `TOGETHER_API_KEY`       | `https://api.together.xyz/v1`                     |
| Fireworks AI       | `fireworks` | `FIREWORKS_API_KEY`      | `https://api.fireworks.ai/inference/v1`           |
| DeepInfra          | `deepinfra` | `DEEPINFRA_API_KEY`      | `https://api.deepinfra.com/v1/openai`             |
| Perplexity         | `perplexity`| `PERPLEXITY_API_KEY`     | `https://api.perplexity.ai`                       |
| xAI (Grok)         | `xai`       | `XAI_API_KEY`            | `https://api.x.ai/v1`                             |
| DeepSeek           | `deepseek`  | `DEEPSEEK_API_KEY`       | `https://api.deepseek.com/v1`                     |
| Mistral            | `mistral`   | `MISTRAL_API_KEY`        | `https://api.mistral.ai/v1`                       |
| Cerebras           | `cerebras`  | `CEREBRAS_API_KEY`       | `https://api.cerebras.ai/v1`                      |
| SambaNova          | `sambanova` | `SAMBANOVA_API_KEY`      | `https://api.sambanova.ai/v1`                     |
| Azure OpenAI       | `azure`     | `AZURE_OPENAI_API_KEY`   | (set to your deployment URL)                      |
| GitHub Models      | `github`    | `GITHUB_TOKEN`           | `https://models.inference.ai.azure.com`           |
| LM Studio          | `lmstudio`  | (none — local)           | `http://localhost:1234/v1`                        |
| vLLM               | `vllm`      | (none — local)           | `http://localhost:8000/v1`                        |
| Ollama             | `ollama`    | (none — local)           | `http://localhost:11434/v1`                       |
| llama.cpp server   | `llamacpp`  | (none — local)           | `http://localhost:8080/v1`                        |
| HF TGI             | `tgi`       | (none — local)           | `http://localhost:3000/v1`                        |
| Cohere             | `cohere`    | `COHERE_API_KEY`         | `https://api.cohere.com` (native `/v2/chat`)      |
| Anyscale           | `anyscale`  | `ANYSCALE_API_KEY`       | `https://api.endpoints.anyscale.com/v1`           |
| Lepton AI          | `lepton`    | `LEPTON_API_KEY`         | `https://api.lepton.ai/api/v1`                    |
| Replicate          | `replicate` | `REPLICATE_API_TOKEN`    | `https://openai-proxy.replicate.com/v1`           |
| Novita AI          | `novita`    | `NOVITA_API_KEY`         | `https://api.novita.ai/v3/openai`                 |
| Hyperbolic         | `hyperbolic`| `HYPERBOLIC_API_KEY`     | `https://api.hyperbolic.xyz/v1`                   |
| Lambda Inference   | `lambda`    | `LAMBDA_API_KEY`         | `https://api.lambdalabs.com/v1`                   |
| Nebius AI Studio   | `nebius`    | `NEBIUS_API_KEY`         | `https://api.studio.nebius.ai/v1`                 |
| HF Inference       | `hf`        | `HF_TOKEN`               | `https://router.huggingface.co/v1`                |
| GLHF.chat          | `glhf`      | `GLHF_API_KEY`           | `https://glhf.chat/api/openai/v1`                 |
| Featherless        | `featherless`| `FEATHERLESS_API_KEY`   | `https://api.featherless.ai/v1`                   |
| OctoAI             | `octoai`    | `OCTOAI_API_KEY`         | `https://text.octoai.run/v1`                      |
| NVIDIA NIM         | `nvidia`    | `NVIDIA_API_KEY`         | `https://integrate.api.nvidia.com/v1`             |
| Avian              | `avian`     | `AVIAN_API_KEY`          | `https://api.avian.io/v1`                         |
| Kluster.ai         | `kluster`   | `KLUSTER_API_KEY`        | `https://api.kluster.ai/v1`                       |
| Inference.net      | `inferencenet`| `INFERENCE_NET_API_KEY`| `https://api.inference.net/v1`                    |
| Snowflake Cortex   | `snowflake` | `SNOWFLAKE_API_KEY`      | (set to your account URL)                         |
| Databricks         | `databricks`| `DATABRICKS_TOKEN`       | (set to your workspace URL)                       |
| Writer Palmyra     | `writer`    | `WRITER_API_KEY`         | `https://api.writer.com/v1`                       |
| GPT4All            | `gpt4all`   | (none — local)           | `http://localhost:4891/v1`                        |
| Jan / Cortex       | `jan`       | (none — local)           | `http://localhost:1337/v1`                        |
| KoboldCpp          | `koboldcpp` | (none — local)           | `http://localhost:5001/v1`                        |
| Oobabooga          | `oobabooga` | (none — local)           | `http://localhost:5000/v1`                        |
| Alibaba Qwen       | `qwen`      | `DASHSCOPE_API_KEY`      | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| Zhipu GLM          | `zhipu`     | `ZHIPU_API_KEY`          | `https://open.bigmodel.cn/api/paas/v4`            |
| Moonshot Kimi      | `moonshot`  | `MOONSHOT_API_KEY`       | `https://api.moonshot.cn/v1`                      |
| MiniMax            | `minimax`   | `MINIMAX_API_KEY`        | `https://api.minimaxi.com/v1`                     |
| Yi (01.AI)         | `yi`        | `YI_API_KEY`             | `https://api.lingyiwanwu.com/v1`                  |
| Baichuan           | `baichuan`  | `BAICHUAN_API_KEY`       | `https://api.baichuan-ai.com/v1`                  |
| Tencent Hunyuan    | `hunyuan`   | `HUNYUAN_API_KEY`        | `https://api.hunyuan.cloud.tencent.com/v1`        |
| ByteDance Doubao   | `doubao`    | `ARK_API_KEY`            | `https://ark.cn-beijing.volces.com/api/v3`        |
| SiliconFlow        | `siliconflow`| `SILICONFLOW_API_KEY`   | `https://api.siliconflow.cn/v1`                   |
| Cloudflare Workers | `cloudflare`| `CLOUDFLARE_API_TOKEN`   | (set to your account-id URL)                      |
| Vercel AI Gateway  | `vercel`    | `VERCEL_AI_GATEWAY_KEY`  | `https://gateway.ai.vercel.com/v1`                |
| AIMLAPI            | `aimlapi`   | `AIMLAPI_KEY`            | `https://api.aimlapi.com/v1`                      |
| OpenPipe           | `openpipe`  | `OPENPIPE_API_KEY`       | `https://api.openpipe.ai/api/v1`                  |
| Targon             | `targon`    | `TARGON_API_KEY`         | `https://api.targon.com/v1`                       |
| Pollinations       | `pollinations`| (none — free tier)     | `https://text.pollinations.ai/openai/v1`          |
| AI21 Jamba         | `ai21`      | `AI21_API_KEY`           | `https://api.ai21.com/studio/v1`                  |
| Z.ai (GLM coding)  | `zai`       | `ZAI_API_KEY`            | `https://api.z.ai/api/coding/paas/v4`             |
| Friendli AI        | `friendli`  | `FRIENDLI_TOKEN`         | `https://inference.friendli.ai/v1`                |
| Mancer             | `mancer`    | `MANCER_API_KEY`         | `https://neuro.mancer.tech/oai/v1`                |
| Reka               | `reka`      | `REKA_API_KEY`           | `https://api.reka.ai/v1`                          |
| mlx-lm-server      | `mlx`       | (none — local)           | `http://localhost:8080/v1`                        |
| LocalAI            | `localai`   | (none — local)           | `http://localhost:8080/v1`                        |
| Aphrodite Engine   | `aphrodite` | (none — local)           | `http://localhost:2242/v1`                        |
| Mistral.rs server  | `mistralrs` | (none — local)           | `http://localhost:1234/v1`                        |
| AWS Bedrock        | `bedrock`   | `AWS_BEARER_TOKEN_BEDROCK`| `https://bedrock-runtime.<region>.amazonaws.com/openai/v1` |
| GCP Vertex AI      | `vertex`    | `GOOGLE_VERTEX_TOKEN`    | (set to your project/region OpenAPI URL)          |
| IBM watsonx.ai     | `watsonx`   | `WATSONX_API_KEY` + `WATSONX_PROJECT_ID` | `https://<region>.ml.cloud.ibm.com` |

All non-Anthropic / non-Gemini / non-ChatGPT / non-Cohere entries share the
`OpenAiCompatProvider` adapter (`crates/wingman-providers/src/openai_compat.rs`).
Add a new hosted OpenAI-shape clone by extending its `Variant` enum and the
mapper functions in `runtime.rs` + `login.rs`.

**Notes on the enterprise providers (Bedrock / Vertex / watsonx):**

- **AWS Bedrock** ships via the OpenAI-compat surface released in 2024 —
  set `AWS_BEARER_TOKEN_BEDROCK` (long-term API key generated from the
  AWS console) and adjust the region in `base_url`. The SigV4 path
  against `/model/<id>/invoke-with-response-stream` (with the AWS Event
  Stream binary framing) is **not** implemented; if your AWS setup
  doesn't permit Bedrock API keys, that adapter is the follow-up work.
- **GCP Vertex AI** uses the OpenAPI endpoint with an OAuth2 access
  token. Populate `GOOGLE_VERTEX_TOKEN` with the output of
  `gcloud auth print-access-token` (refresh hourly) and set `base_url`
  to your project + region. Service-account JWT signing is the
  follow-up work for unattended use.
- **IBM watsonx.ai** is a native adapter (`watsonx.rs`) — provide
  `WATSONX_API_KEY` + `WATSONX_PROJECT_ID` and the adapter exchanges
  the API key for an IAM token internally (cached for ~1h). Pass
  `WATSONX_ACCESS_TOKEN` instead if you've already minted one.

**Reasoning support.** The portable `reasoning` level
(see [CONFIGURATION.md](CONFIGURATION.md#reasoning)) maps onto a native
parameter only where one exists:

| Backend | Native control | Notes |
| --- | --- | --- |
| Anthropic | `thinking.budget_tokens` | Thinking blocks are signed and round-trip through history verbatim. `max_tokens` is raised above the budget and `temperature` is dropped, both required by the API. |
| OpenAI | `reasoning_effort` | Sent only to reasoning-family models (`o*`, `gpt-5*`); `gpt-4.1` and friends reject the parameter, so it is omitted there. Reasoning is not echoed back — the server keeps it. |
| Gemini | `generationConfig.thinkingConfig` | `includeThoughts` is set so thoughts stream; `thoughtsTokenCount` is already folded into output tokens for costing. |
| OpenAI-compat servers | `reasoning_content` / `reasoning` deltas | Streamed reasoning is picked up from either spelling (DeepSeek, vLLM, LM Studio, OpenRouter). Whether the server honours `reasoning_effort` is up to that server. |
| Cohere, watsonx, ChatGPT (OAuth) | — | No reasoning control; a configured level is ignored and `wingman doctor` says so. |
