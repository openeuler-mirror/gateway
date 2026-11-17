//! 网关 config.yaml 生成。字段形态以代码调查结论为准（boom-config/src/lib.rs）。

use std::path::Path;

use anyhow::Context;

pub struct RenderConfig {
    pub instance_id: &'static str,
    pub port: u16,
    pub dsn: String,
    pub master_key: String,
    pub model_name: String,
    pub mock_url: String,
    pub quota_sync_interval_secs: u32,
}

pub fn render(cfg: &RenderConfig) -> String {
    // 与主仓 config.yaml 同构；model_list 在 DB 权威模式下空库首次 boot 自动 seed
    format!(
        r#"server:
  host: 127.0.0.1
  port: {port}
  instance_id: {instance_id}

general_settings:
  master_key: "{master_key}"
  database_url: "{dsn}"

router_settings:
  schedule_policy: round_robin

quota_sync:
  interval_secs: {interval}

model_list:
  - model_name: {model}
    litellm_params:
      model: openai/gpt-4o-mini
      api_base: {mock_url}
      api_key: sk-e2e-mock
"#,
        port = cfg.port,
        instance_id = cfg.instance_id,
        master_key = cfg.master_key,
        dsn = cfg.dsn,
        interval = cfg.quota_sync_interval_secs,
        model = cfg.model_name,
        mock_url = cfg.mock_url,
    )
}

pub fn render_to_file(path: &Path, cfg: &RenderConfig) -> anyhow::Result<()> {
    std::fs::write(path, render(cfg)).with_context(|| format!("写 {}", path.display()))?;
    Ok(())
}
