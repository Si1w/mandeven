use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mandeven::config::{self, AppConfig, LLMProfile};
use mandeven::cron::CronEngine;
use mandeven::llm::{Message, Request, Thinking};
use mandeven::memory;
use mandeven::prompt::{PromptContext, PromptEngine};
use mandeven::security::SandboxPolicy;
use mandeven::skill::{self, SkillIndex};
use mandeven::task;
use mandeven::tools;
use mandeven::utils::workspace;

const ROUNDS: usize = 4;
const TARGET_RATIO: f64 = 0.80;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cfg = AppConfig::load()?;
    let (provider, profile_name, profile) = default_profile(&cfg)?;
    if provider != "deepseek" {
        return Err(format!("default provider is {provider}, expected deepseek").into());
    }
    let client = mandeven::llm::providers::client_for(provider)
        .ok_or_else(|| format!("unknown provider: {provider}"))?;

    let cwd = std::env::current_dir()?;
    workspace::init(std::fs::canonicalize(&cwd)?);
    SandboxPolicy::init(cfg.sandbox.policy);

    let skills = Arc::new(load_skills(&cfg)?);
    let prompts = PromptEngine::load(&cfg.data_dir(), &skills)?;
    let mut registry = tools::Registry::new();
    tools::register_builtins(&mut registry);
    tools::task::register(
        &mut registry,
        Arc::new(task::Manager::new(&config::project_bucket(&cwd))),
    );
    if cfg.agent.memory.enabled {
        tools::memory::register(
            &mut registry,
            Arc::new(memory::Manager::new(
                &cfg.data_dir(),
                &config::project_bucket(&cwd),
            )),
        );
    }
    if cfg.agent.cron.enabled {
        let (engine, _rx) = CronEngine::new(&cfg.agent.cron, &cfg.data_dir()).await?;
        tools::cron::register(&mut registry, Arc::new(engine));
    }
    if !skills.is_empty() {
        registry.register(Arc::new(tools::skill::SkillTool::new(skills.clone())));
    }

    let system = prompts
        .iteration_system(&PromptContext {
            model_id: &profile.model_name,
            cwd: Path::new(&cwd),
        })
        .into_message();
    let tools = registry.schemas();
    println!(
        "profile={provider}/{profile_name} model={} tools={} rounds={ROUNDS}",
        profile.model_name,
        tools.len()
    );

    let mut measured = Vec::new();
    for round in 1..=ROUNDS {
        let req = Request {
            messages: vec![
                system.clone(),
                Message::User {
                    content: format!(
                        "DeepSeek prefix-cache probe round {round}. Reply with exactly: OK"
                    ),
                },
            ],
            tools: tools.clone(),
            model_name: profile.model_name.clone(),
            max_tokens: Some(8),
            temperature: Some(0.0),
            timeout_secs: cfg.llm.timeout_secs.or(Some(60)),
            thinking: profile.thinking.map(|enabled| Thinking {
                enabled,
                reasoning_effort: None,
            }),
        };

        let response = client.complete(req).await?;
        let usage = response.usage;
        let ratio = cache_ratio(usage.cache_hit_tokens, usage.cache_miss_tokens);
        println!(
            "round={round} prompt={} completion={} hit={} miss={} ratio={}",
            usage.prompt_tokens,
            usage.completion_tokens,
            display_opt(usage.cache_hit_tokens),
            display_opt(usage.cache_miss_tokens),
            ratio
                .map(|r| format!("{:.2}%", r * 100.0))
                .unwrap_or_else(|| "n/a".to_string())
        );
        if round > 1
            && let Some(ratio) = ratio
        {
            measured.push(ratio);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    if measured.is_empty() {
        println!("verdict=unknown reason=DeepSeek did not return cache hit/miss tokens");
    } else {
        let avg = measured.iter().sum::<f64>() / measured.len() as f64;
        let min = measured.iter().copied().fold(f64::INFINITY, f64::min);
        let verdict = if avg >= TARGET_RATIO { "ok" } else { "low" };
        println!(
            "measured_after_warmup={} avg={:.2}% min={:.2}% target=>={:.0}% verdict={verdict}",
            measured.len(),
            avg * 100.0,
            min * 100.0,
            TARGET_RATIO * 100.0
        );
    }

    Ok(())
}

fn default_profile(
    cfg: &AppConfig,
) -> Result<(&str, &str, &LLMProfile), Box<dyn Error + Send + Sync>> {
    let (provider, profile_name) = cfg
        .llm
        .default
        .split_once('/')
        .ok_or("llm.default must be provider/profile")?;
    let profile = cfg
        .llm
        .providers
        .get(provider)
        .and_then(|models| models.get(profile_name))
        .ok_or_else(|| format!("profile not found: {}", cfg.llm.default))?;
    Ok((provider, profile_name, profile))
}

fn load_skills(cfg: &AppConfig) -> Result<SkillIndex, Box<dyn Error + Send + Sync>> {
    if cfg.agent.skill.enabled {
        Ok(skill::load(&cfg.data_dir().join(skill::SKILLS_SUBDIR))?)
    } else {
        Ok(SkillIndex::new())
    }
}

fn cache_ratio(hit: Option<u32>, miss: Option<u32>) -> Option<f64> {
    let hit = hit?;
    let miss = miss?;
    let total = hit + miss;
    (total > 0).then_some(hit as f64 / total as f64)
}

fn display_opt(v: Option<u32>) -> String {
    v.map(|n| n.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}
