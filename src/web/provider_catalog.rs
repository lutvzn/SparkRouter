//! Static catalog of well-known provider presets for the dashboard.
//!
//! Pure configuration data with no database or runtime dependencies: provider
//! forms offer these as one-click starting points. Cards show protocol badges;
//! the full endpoint is shown on the provider's detail page.
//!
//! The list starts with the original presets, then OmniRoute's API-key catalog
//! entries whose chat URL and credential header match this gateway's OpenAI or
//! Anthropic adapters. No keys, account-specific URLs, or other protocols are
//! imported. Duplicate endpoints are omitted so a connection attaches to one tile.

use crate::providers::Protocol;

/// A built-in provider template shown in the dashboard. Public catalog data
/// only (slug, display name, endpoint, protocol adapter); nothing secret.
#[derive(Debug, Clone, Copy)]
pub(super) struct Preset {
    pub slug: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub protocol: Protocol,
}

/// Well-known gateways, in dashboard display order.
pub(super) const PRESETS: &[Preset] = &[
    Preset {
        slug: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "together",
        name: "Together",
        base_url: "https://api.together.xyz/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "cheaperinference",
        name: "Cheaper Inference",
        base_url: "https://api.cheaperinference.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "charm-hyper",
        name: "Charm Hyper",
        base_url: "https://hyper.charm.land/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "agentrouter",
        name: "AgentRouter",
        base_url: "https://agentrouter.org/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "unorouter",
        name: "UnoRouter",
        base_url: "https://api.unorouter.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "opper",
        name: "Opper",
        base_url: "https://api.opper.ai/v3/compat",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "requesty",
        name: "Requesty",
        base_url: "https://router.requesty.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "zylo-api",
        name: "Zylo API",
        base_url: "https://api.zyloai.net/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "fastrouter",
        name: "FastRouter",
        base_url: "https://api.fastrouter.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "anyapi",
        name: "AnyAPI AI",
        base_url: "https://api.anyapi.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "electronhub",
        name: "Electron Hub",
        base_url: "https://api.electronhub.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "llmgateway",
        name: "LLM Gateway",
        base_url: "https://api.llmgateway.io/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "lyceum",
        name: "Lyceum",
        base_url: "https://api.lyceum.technology/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "llm-kiwi",
        name: "LLM.Kiwi",
        base_url: "https://api.llm.kiwi/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "literouter",
        name: "LiteRouter",
        base_url: "https://api.literouter.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "greenpt",
        name: "GreenPT",
        base_url: "https://api.greenpt.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "eurouter",
        name: "EURouter",
        base_url: "https://api.eurouter.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "mnn-ai",
        name: "MNN AI",
        base_url: "https://api.mnnai.ru/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "meganova-ai",
        name: "MegaNova AI",
        base_url: "https://api.meganova.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "mixlayer",
        name: "Mixlayer",
        base_url: "https://models.mixlayer.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "speka",
        name: "Speka AI",
        base_url: "https://speka.me/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "tokenreply",
        name: "TokenReply",
        base_url: "https://api.tokenreply.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "yolo-auto",
        name: "Yolo-Auto",
        base_url: "https://yolo-auto.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "dxnt",
        name: "DXNT / DX Token",
        base_url: "https://www.dxnt.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "cloudcode-one",
        name: "CloudCode.ONE",
        base_url: "https://api.cloudcode.one/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ofoxai",
        name: "OfoxAI",
        base_url: "https://api.ofox.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "zerolimitai",
        name: "ZeroLimitAI",
        base_url: "https://www.zerolimitai.com/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "chatanywhere",
        name: "ChatAnywhere",
        base_url: "https://api.chatanywhere.org/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "helyxai",
        name: "Helyx AI",
        base_url: "https://helyxai.space/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "auriko",
        name: "Auriko",
        base_url: "https://api.auriko.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "poixe-ai",
        name: "Poixe AI",
        base_url: "https://api.poixe.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "naga-ai",
        name: "Naga AI",
        base_url: "https://api.naga.ac/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "chat-oripe",
        name: "Chat Oripe",
        base_url: "https://api.oriper.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "freeinference",
        name: "FreeInference",
        base_url: "https://freeinference.org/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "dgrid",
        name: "DGrid",
        base_url: "https://api.dgrid.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "qiniu",
        name: "Qiniu",
        base_url: "https://api.qnaigc.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "orcarouter",
        name: "OrcaRouter",
        base_url: "https://api.orcarouter.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "api-airforce",
        name: "Api.airforce",
        base_url: "https://api.airforce/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "crof",
        name: "CrofAI",
        base_url: "https://crof.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "bazaarlink",
        name: "BazaarLink",
        base_url: "https://bazaarlink.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "synthetic",
        name: "Synthetic",
        base_url: "https://api.synthetic.new/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "kilo-gateway",
        name: "Kilo Gateway",
        base_url: "https://api.kilo.ai/api/gateway",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "opencode-zen",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "opencode-go",
        name: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "dahl",
        name: "Dahl",
        base_url: "https://inference.dahl.global/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "freetheai",
        name: "FreeTheAi",
        base_url: "https://api.freetheai.xyz/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "g4f-groq",
        name: "g4f.space — Groq",
        base_url: "https://g4f.space/api/groq/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "g4f-gemini",
        name: "g4f.space — Gemini",
        base_url: "https://g4f.space/api/gemini/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "g4f-pollinations",
        name: "g4f.space — Pollinations",
        base_url: "https://g4f.space/api/pollinations/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "g4f-ollama",
        name: "g4f.space — Ollama",
        base_url: "https://g4f.space/api/ollama/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "g4f-nvidia",
        name: "g4f.space — NVIDIA",
        base_url: "https://g4f.space/api/nvidia/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "vercel-ai-gateway",
        name: "Vercel AI Gateway",
        base_url: "https://ai-gateway.vercel.sh/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "llm7",
        name: "LLM7.io",
        base_url: "https://api.llm7.io/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "llamagate",
        name: "LlamaGate",
        base_url: "https://llamagate.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nanogpt",
        name: "NanoGPT",
        base_url: "https://nano-gpt.com/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "aimlapi",
        name: "AI/ML API",
        base_url: "https://api.aimlapi.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "novita",
        name: "Novita AI",
        base_url: "https://api.novita.ai/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "bai",
        name: "b.ai",
        base_url: "https://api.b.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "poe",
        name: "Poe",
        base_url: "https://api.poe.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "chutes",
        name: "Chutes.ai",
        base_url: "https://llm.chutes.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "factory",
        name: "Factory",
        base_url: "https://api.factory.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "bluesminds",
        name: "BluesMinds",
        base_url: "https://api.bluesminds.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "freemodel-dev",
        name: "FreeModel.dev",
        base_url: "https://api.freemodel.dev/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "freeaiapikey",
        name: "FreeAIAPIKey",
        base_url: "https://api.freeaiapikey.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "zenmux",
        name: "ZenMux",
        base_url: "https://zenmux.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "openadapter",
        name: "OpenAdapter",
        base_url: "https://api.openadapter.in/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "dit",
        name: "DIT.ai",
        base_url: "https://api.dit.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "tokenrouter",
        name: "TokenRouter",
        base_url: "https://api.tokenrouter.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "token-kiosk",
        name: "Token Kiosk",
        base_url: "https://agent-router.gaib.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "sumopod",
        name: "SumoPod",
        base_url: "https://ai.sumopod.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "x5lab",
        name: "X5Lab",
        base_url: "https://api.x5lab.dev/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "chenzk",
        name: "Chenzk API",
        base_url: "https://chenzk.top/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "kenari",
        name: "Kenari",
        base_url: "https://kenari.id/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "navy",
        name: "NavyAI",
        base_url: "https://api.navy/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ainative",
        name: "AINative Studio",
        base_url: "https://api.ainative.studio/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "aion",
        name: "Aion Labs",
        base_url: "https://api.aionlabs.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "routeway",
        name: "Routeway",
        base_url: "https://api.routeway.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nara",
        name: "NaraRouter",
        base_url: "https://router.bynara.id/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "xkiro",
        name: "xKiro",
        base_url: "https://api.xkiro.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "regolo",
        name: "Regolo AI",
        base_url: "https://api.regolo.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "void-ai",
        name: "Void AI",
        base_url: "https://api.voidai.app/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "helixmind",
        name: "HelixMind",
        base_url: "https://helixmind.online/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "logfare",
        name: "Logfare",
        base_url: "https://logfare.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "tabitoken",
        name: "TabiToken",
        base_url: "https://tabitoken.com/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "seekai",
        name: "SeekAi",
        base_url: "https://seekai.cc/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "reka",
        name: "Reka",
        base_url: "https://api.reka.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "xai",
        name: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "perplexity",
        name: "Perplexity",
        base_url: "https://api.perplexity.ai",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "cohere",
        name: "Cohere",
        base_url: "https://api.cohere.com/compatibility/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "meta-llama",
        name: "Meta Llama API",
        base_url: "https://api.llama.com/compat/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "morph",
        name: "Morph",
        base_url: "https://api.morphllm.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ai21",
        name: "AI21 Labs",
        base_url: "https://api.ai21.com/studio/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "venice",
        name: "Venice.ai",
        base_url: "https://api.venice.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "codestral",
        name: "Codestral",
        base_url: "https://codestral.mistral.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "upstage",
        name: "Upstage",
        base_url: "https://api.upstage.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nous-research",
        name: "Nous Research",
        base_url: "https://inference-api.nousresearch.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "arcee-ai",
        name: "Arcee AI",
        base_url: "https://api.arcee.ai/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "liquid",
        name: "Liquid AI",
        base_url: "https://inference.liquid.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "inception",
        name: "Inception",
        base_url: "https://api.inceptionlabs.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "writer",
        name: "Writer",
        base_url: "https://api.writer.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "openvecta",
        name: "OpenVecta",
        base_url: "https://api.openvecta.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "openference-api",
        name: "Openference API",
        base_url: "https://api.openference.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "poolside",
        name: "Poolside",
        base_url: "https://inference.poolside.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nvidia",
        name: "NVIDIA NIM",
        base_url: "https://integrate.api.nvidia.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nebius",
        name: "Nebius AI",
        base_url: "https://api.tokenfactory.nebius.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nube",
        name: "Nube.sh",
        base_url: "https://ai.nube.sh/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "siliconflow",
        name: "SiliconFlow",
        base_url: "https://api.siliconflow.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "hyperbolic",
        name: "Hyperbolic",
        base_url: "https://api.hyperbolic.xyz/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ollama-cloud",
        name: "Ollama Cloud",
        base_url: "https://ollama.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "huggingface",
        name: "HuggingFace",
        base_url: "https://router.huggingface.co/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "deepinfra",
        name: "DeepInfra",
        base_url: "https://api.deepinfra.com/v1/openai",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "lambda-ai",
        name: "Lambda AI",
        base_url: "https://api.lambda.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "sambanova",
        name: "SambaNova",
        base_url: "https://api.sambanova.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "nscale",
        name: "nScale",
        base_url: "https://inference.api.nscale.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "baseten",
        name: "Baseten",
        base_url: "https://inference.baseten.co/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "publicai",
        name: "PublicAI",
        base_url: "https://api.publicai.co/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "featherless-ai",
        name: "Featherless AI",
        base_url: "https://api.featherless.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "friendliai",
        name: "FriendliAI",
        base_url: "https://api.friendli.ai/serverless/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "wandb",
        name: "Weights & Biases Inference",
        base_url: "https://api.inference.wandb.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "inference-net",
        name: "Inference.net",
        base_url: "https://api.inference.net/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "bytez",
        name: "Bytez",
        base_url: "https://api.bytez.com/models/v2/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "modelscope",
        name: "ModelScope",
        base_url: "https://api-inference.modelscope.cn/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "byteplus",
        name: "BytePlus ModelArk",
        base_url: "https://ark.ap-southeast.bytepluses.com/api/v3",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "digitalocean",
        name: "DigitalOcean",
        base_url: "https://inference.do-ai.run/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "modal",
        name: "Modal",
        base_url: "https://api.modal.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "scaleway",
        name: "Scaleway AI",
        base_url: "https://api.scaleway.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ovhcloud",
        name: "OVHcloud AI",
        base_url: "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "heroku",
        name: "Heroku AI",
        base_url: "https://us.inference.heroku.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "qianfan",
        name: "Baidu Qianfan",
        base_url: "https://qianfan.baidubce.com/v2",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "glm",
        name: "GLM Coding",
        base_url: "https://api.z.ai/api/coding/paas/v4",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "glm-cn",
        name: "GLM Coding (China)",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "bailian-coding-plan",
        name: "Alibaba Token Plan",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "qwen-cloud",
        name: "Qwen Cloud",
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "qwen-cloud-token-plan",
        name: "Qwen Cloud Token Plan",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "minimax",
        name: "Minimax Coding",
        base_url: "https://api.minimax.io/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "minimax-cn",
        name: "Minimax (China)",
        base_url: "https://api.minimaxi.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "zai",
        name: "Z.AI",
        base_url: "https://api.z.ai/api/anthropic/v1",
        protocol: Protocol::Anthropic,
    },
    Preset {
        slug: "longcat",
        name: "LongCat AI",
        base_url: "https://api.longcat.chat/openai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "moonshot",
        name: "Kimi",
        base_url: "https://api.moonshot.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "volcengine",
        name: "Volcengine",
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "volcengine-agent-plan",
        name: "Volcengine Ark Agent Plan",
        base_url: "https://ark.cn-beijing.volces.com/api/plan/v3",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "volcengine-coding-plan",
        name: "Volcengine Ark Coding Plan",
        base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "xiaomi-mimo",
        name: "Xiaomi MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "xiaomi-mimo-token-plan",
        name: "Xiaomi MiMo Token Plan",
        base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "tencent",
        name: "Tencent Hunyuan",
        base_url: "https://api.hunyuan.cloud.tencent.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "iflytek",
        name: "iFlytek Spark",
        base_url: "https://spark-api-open.xf-yun.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "baichuan",
        name: "Baichuan",
        base_url: "https://api.baichuan-ai.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "yi",
        name: "Yi (01.AI)",
        base_url: "https://api.lingyiwanwu.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "stepfun",
        name: "StepFun",
        base_url: "https://api.stepfun.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "coze",
        name: "Coze",
        base_url: "https://api.coze.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "sensenova",
        name: "SenseNova",
        base_url: "https://token.sensenova.cn/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "hcnsec",
        name: "Huancheng Public API",
        base_url: "https://api.hcnsec.cn/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "agnes",
        name: "Agnes AI",
        base_url: "https://apihub.agnes-ai.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "agnes-cn",
        name: "Agnes AI (China)",
        base_url: "https://api.agnes-ai.cn/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "sealion",
        name: "SEA-LION",
        base_url: "https://api.sea-lion.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "internlm",
        name: "InternLM (Intern-S1)",
        base_url: "https://chat.intern-ai.org.cn/api/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "ant-ling",
        name: "Ant Ling / Ring (inclusionAI)",
        base_url: "https://api.ant-ling.com/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "sarvam",
        name: "Sarvam AI",
        base_url: "https://api.sarvam.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "plamo",
        name: "PLaMo",
        base_url: "https://api.platform.preferredai.jp/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "typhoon",
        name: "Typhoon",
        base_url: "https://api.opentyphoon.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "kie",
        name: "KIE.AI",
        base_url: "https://api.kie.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "pollinations",
        name: "Pollinations AI",
        base_url: "https://gen.pollinations.ai/v1",
        protocol: Protocol::OpenAi,
    },
    Preset {
        slug: "v0-vercel",
        name: "v0 (Vercel)",
        base_url: "https://api.v0.dev/v1",
        protocol: Protocol::OpenAi,
    },
];

/// Looks up a preset by its exact slug.
pub(super) fn find(slug: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.slug == slug)
}

/// Returns the preset whose endpoint and protocol both match exactly, after
/// trimming trailing slashes from `base_url`. The same endpoint advertised
/// with a different protocol is a different tile, not a match.
pub(super) fn matching(base_url: &str, protocol: &Protocol) -> Option<&'static Preset> {
    let base_url = base_url.trim_end_matches('/');
    PRESETS
        .iter()
        .find(|preset| preset.base_url == base_url && preset.protocol.as_str() == protocol.as_str())
}

/// Default label for a legacy connection: the preset name when endpoint and
/// protocol match the catalog, otherwise the URL hostname, falling back to a
/// generic label for values that are not parseable URLs. Named custom providers
/// use their stored names instead; detail pages show the full endpoint.
pub(super) fn display_name(base_url: &str, protocol: &Protocol) -> String {
    if let Some(preset) = matching(base_url, protocol) {
        return preset.name.to_string();
    }
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_string()))
        .unwrap_or_else(|| "Custom provider".to_string())
}

#[cfg(test)]
mod tests {
    use super::{PRESETS, Protocol, display_name, find, matching};

    #[test]
    fn find_locates_presets_by_exact_slug() {
        let groq = find("groq").expect("groq preset exists");
        assert_eq!(groq.slug, "groq");
        assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(groq.protocol, Protocol::OpenAi);
        assert!(find("Groq").is_none());
        assert!(find("unknown-provider").is_none());
        assert_eq!(PRESETS.len(), 171);
        assert_eq!(find("xai").expect("xai").name, "xAI (Grok)");
        assert_eq!(find("poe").expect("poe").base_url, "https://api.poe.com/v1");
        assert_eq!(
            find("agentrouter").expect("agentrouter").protocol,
            Protocol::Anthropic
        );
    }

    #[test]
    fn presets_have_unique_slugs_and_endpoints() {
        let mut slugs = std::collections::BTreeSet::new();
        let mut endpoints = std::collections::BTreeSet::new();
        for preset in PRESETS {
            assert!(slugs.insert(preset.slug), "duplicate slug {}", preset.slug);
            assert!(
                endpoints.insert((preset.base_url, preset.protocol.as_str())),
                "duplicate endpoint {} {}",
                preset.slug,
                preset.base_url
            );
            assert!(preset.base_url.starts_with("https://"));
            assert!(!preset.base_url.ends_with('/'));
        }
    }

    #[test]
    fn matching_trims_trailing_slash_and_requires_protocol() {
        let anthropic = matching("https://api.anthropic.com/v1/", &Protocol::Anthropic)
            .expect("anthropic preset matches despite trailing slash");
        assert_eq!(anthropic.slug, "anthropic");
        // Same endpoint advertised with the other protocol is not a match.
        assert!(matching("https://api.anthropic.com/v1", &Protocol::OpenAi).is_none());
        // Unknown endpoints never match, even with a catalogued protocol.
        assert!(matching("https://api.example.com/v1", &Protocol::OpenAi).is_none());
    }

    #[test]
    fn display_name_prefers_preset_then_hostname_then_fallback() {
        assert_eq!(
            display_name("https://api.openai.com/v1", &Protocol::OpenAi),
            "OpenAI"
        );
        assert_eq!(
            display_name("https://llm.internal.example:8080/v1", &Protocol::OpenAi),
            "llm.internal.example"
        );
        assert_eq!(
            display_name("not a valid url", &Protocol::Anthropic),
            "Custom provider"
        );
    }
}
