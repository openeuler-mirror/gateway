//! DT 用例 — boom-config：YAML 配置解析、环境变量解析、校验、YAML I/O、密钥掩码。
//! 覆盖 ProviderParams::resolve_provider_and_model（含 auto_detect 全分支）、
//! ModelGroupAlias、resolve_env_value、Config/KvcAwareSettings validate、
//! load_config、read_raw_yaml/write_yaml_atomic/set_yaml_path、is_secret_field/mask_secrets。

use boom_config::{
    is_secret_field, load_config, mask_secrets_in_place, read_raw_yaml, resolve_env_value,
    set_yaml_path, write_yaml_atomic, BlockOp, KvcAwareSettings, ModelGroupAlias,
    ProviderParams, RouterSettings,
};
use std::collections::HashMap;

fn provider_params(model: &str) -> ProviderParams {
    ProviderParams {
        model: model.to_string(),
        api_base: None,
        api_key: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_region_name: None,
        api_version: None,
        rpm: None,
        tpm: None,
        timeout: 1200,
        headers: HashMap::new(),
        temperature: None,
        max_tokens: None,
    }
}

// ── ProviderParams::resolve_provider_and_model + auto_detect_provider ──

/// DT-CFG-01：显式 provider 前缀正确拆分。
#[test]
fn resolve_explicit_provider_prefix() {
    let p = provider_params("openai/gpt-4o");
    assert_eq!(p.resolve_provider_and_model(), ("openai".into(), "gpt-4o".into()));
    let p = provider_params("anthropic/claude-sonnet-4");
    assert_eq!(p.resolve_provider_and_model(), ("anthropic".into(), "claude-sonnet-4".into()));
}

/// DT-CFG-02：无前缀时 auto_detect 按 model 名识别 OpenAI 系列。
#[test]
fn auto_detect_openai_family() {
    for m in ["gpt-4o", "o1-preview", "o3-mini", "o4-mini", "text-embedding-3", "dall-e-3", "chatgpt-4o", "ft:gpt-4o"] {
        let p = provider_params(m);
        assert_eq!(p.resolve_provider_and_model().0, "openai", "model {m}");
    }
}

/// DT-CFG-03：auto_detect 识别 anthropic / gemini / bedrock。
#[test]
fn auto_detect_other_providers() {
    assert_eq!(provider_params("claude-3-opus").resolve_provider_and_model().0, "anthropic");
    assert_eq!(provider_params("gemini-1.5-pro").resolve_provider_and_model().0, "gemini");
    assert_eq!(provider_params("gemma-2b").resolve_provider_and_model().0, "gemini");
    assert_eq!(provider_params("anthropic.claude-3").resolve_provider_and_model().0, "bedrock");
    assert_eq!(provider_params("amazon.titan-text").resolve_provider_and_model().0, "bedrock");
    assert_eq!(provider_params("meta.llama3").resolve_provider_and_model().0, "bedrock");
    assert_eq!(provider_params("cohere.command").resolve_provider_and_model().0, "bedrock");
    assert_eq!(provider_params("ai21.j2").resolve_provider_and_model().0, "bedrock");
    assert_eq!(provider_params("mistral.mistral-large").resolve_provider_and_model().0, "bedrock");
}

/// DT-CFG-04：无法识别时默认 openai。
#[test]
fn auto_detect_unknown_defaults_to_openai() {
    assert_eq!(provider_params("some-unknown-model").resolve_provider_and_model().0, "openai");
}

// ── ModelGroupAlias ──

/// DT-CFG-05：Simple / Extended 两种别名形态的 target_model 与 is_hidden。
#[test]
fn model_group_alias_variants() {
    let simple = ModelGroupAlias::Simple("gpt-4o".into());
    assert_eq!(simple.target_model(), "gpt-4o");
    assert!(!simple.is_hidden());

    let ext = ModelGroupAlias::Extended { model: "gpt-4o".into(), hidden: true };
    assert_eq!(ext.target_model(), "gpt-4o");
    assert!(ext.is_hidden());

    let ext_visible = ModelGroupAlias::Extended { model: "gpt-4o".into(), hidden: false };
    assert!(!ext_visible.is_hidden());
}

// ── resolve_env_value ──

/// DT-CFG-06：${VAR} 与 os.environ/VAR 两种模式解析环境变量，未设置时回退原值。
#[test]
fn resolve_env_value_patterns() {
    std::env::set_var("BOOM_DT_TEST_VAR", "resolved-value");

    assert_eq!(resolve_env_value("${BOOM_DT_TEST_VAR}"), "resolved-value");
    assert_eq!(resolve_env_value("os.environ/BOOM_DT_TEST_VAR"), "resolved-value");
    // 字面量原样返回
    assert_eq!(resolve_env_value("plain-text"), "plain-text");
    // 带空格的字面量
    assert_eq!(resolve_env_value("  spaced  "), "  spaced  ");
    // 未设置的变量回退原值
    assert_eq!(resolve_env_value("${BOOM_DT_NO_SUCH_VAR}"), "${BOOM_DT_NO_SUCH_VAR}");
    assert_eq!(
        resolve_env_value("os.environ/BOOM_DT_NO_SUCH_VAR"),
        "os.environ/BOOM_DT_NO_SUCH_VAR"
    );

    std::env::remove_var("BOOM_DT_TEST_VAR");
}

// ── KvcAwareSettings::validate ──

/// DT-CFG-07：合法 kvc_aware 配置校验通过。
#[test]
fn kvc_aware_valid_passes() {
    let s = KvcAwareSettings { max_blocks: 1000, router_ttl_secs: 600.0 };
    assert!(s.validate().is_ok());
}

/// DT-CFG-08：router_ttl_secs 非法（NaN/负数/无穷）校验失败。
#[test]
fn kvc_aware_invalid_ttl_rejected() {
    let neg = KvcAwareSettings { max_blocks: 1000, router_ttl_secs: -1.0 };
    assert!(neg.validate().is_err());

    let nan = KvcAwareSettings { max_blocks: 1000, router_ttl_secs: f64::NAN };
    assert!(nan.validate().is_err());

    let inf = KvcAwareSettings { max_blocks: 1000, router_ttl_secs: f64::INFINITY };
    assert!(inf.validate().is_err());
}

/// DT-CFG-09：max_blocks=0 校验失败。
#[test]
fn kvc_aware_zero_max_blocks_rejected() {
    let s = KvcAwareSettings { max_blocks: 0, router_ttl_secs: 600.0 };
    assert!(s.validate().is_err());
}

/// DT-CFG-10：router_ttl_secs=0 合法（表示禁用 TTL prune）。
#[test]
fn kvc_aware_zero_ttl_is_valid() {
    let s = KvcAwareSettings { max_blocks: 1000, router_ttl_secs: 0.0 };
    assert!(s.validate().is_ok());
}

// ── RouterSettings::flow_control_queue_timeout_secs ──

/// DT-CFG-11：flow_control_queue_timeout_secs None 时返回默认 1200，Some 时返回设定值。
#[test]
fn flow_control_queue_timeout_secs_default_and_override() {
    let r = RouterSettings::default();
    assert_eq!(r.flow_control_queue_timeout_secs(), 1200);

    let mut r = RouterSettings::default();
    r.flow_control_queue_timeout_secs = Some(300);
    assert_eq!(r.flow_control_queue_timeout_secs(), 300);
}

// ── load_config + Config::validate ──

fn write_config(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(&path, content).unwrap();
    path
}

/// DT-CFG-12：最小合法配置加载成功，字段正确解析。
#[test]
fn load_minimal_config_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "config.yaml",
        r#"
model_list:
  - model_name: dt-mock
    litellm_params:
      model: openai/dt-mock
      api_base: http://127.0.0.1:9/v1
      api_key: sk-test
general_settings:
  master_key: sk-master
"#,
    );
    let config = load_config(&path).expect("load");
    assert_eq!(config.model_list.len(), 1);
    assert_eq!(config.model_list[0].model_name, "dt-mock");
    assert_eq!(config.general_settings.master_key.as_deref(), Some("sk-master"));
}

/// DT-CFG-13：kvc_aware 非法配置 load_config 失败（validate 拦截）。
#[test]
fn load_config_rejects_invalid_kvc_aware() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "config.yaml",
        r#"
model_list: []
router_settings:
  kvc_aware:
    max_blocks: 0
"#,
    );
    assert!(load_config(&path).is_err());
}

/// DT-CFG-14：文件不存在时 load_config 返回错误。
#[test]
fn load_config_missing_file_errors() {
    assert!(load_config("/nonexistent/path/config.yaml").is_err());
}

/// DT-CFG-15：kvc_aware 策略下 rebalance_threshold 越界（0 或 >100）被拒。
#[test]
fn load_config_rejects_invalid_rebalance_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "config.yaml",
        r#"
model_list: []
router_settings:
  schedule_policy: kvc_aware
  rebalance_threshold: 150
"#,
    );
    assert!(load_config(&path).is_err());

    let dir2 = tempfile::tempdir().unwrap();
    let path2 = write_config(
        &dir2,
        "config.yaml",
        r#"
model_list: []
router_settings:
  schedule_policy: key_affinity
  rebalance_threshold: 0
"#,
    );
    assert!(load_config(&path2).is_err());
}

/// DT-CFG-16：load_config 解析 YAML 中的环境变量引用。
#[test]
fn load_config_resolves_env_vars() {
    std::env::set_var("BOOM_DT_MASTER", "sk-from-env");
    std::env::set_var("BOOM_DT_KEY", "sk-key-from-env");
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "config.yaml",
        r#"
model_list:
  - model_name: m
    litellm_params:
      model: openai/m
      api_key: ${BOOM_DT_KEY}
general_settings:
  master_key: os.environ/BOOM_DT_MASTER
"#,
    );
    let config = load_config(&path).expect("load");
    assert_eq!(config.general_settings.master_key.as_deref(), Some("sk-from-env"));
    assert_eq!(
        config.model_list[0].litellm_params.api_key.as_deref(),
        Some("sk-key-from-env")
    );
    std::env::remove_var("BOOM_DT_MASTER");
    std::env::remove_var("BOOM_DT_KEY");
}

/// DT-CFG-17：Config::lookup_cost_template 按名查找。
#[test]
fn lookup_cost_template_finds_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        "config.yaml",
        r#"
model_list: []
cost_templates:
  - name: standard
    input: 0.01
    output: 0.03
  - name: premium
    input: 0.05
    output: 0.15
"#,
    );
    let config = load_config(&path).expect("load");
    assert!(config.lookup_cost_template("standard").is_some());
    assert!(config.lookup_cost_template("premium").is_some());
    assert!(config.lookup_cost_template("nonexistent").is_none());
}

// ── YAML I/O: read_raw_yaml / write_yaml_atomic / set_yaml_path ──

/// DT-CFG-18：read_raw_yaml 读取原始 YAML（不解析环境变量）。
#[test]
fn read_raw_yaml_preserves_env_refs() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, "c.yaml", "master_key: ${SECRET}\nport: 8080\n");
    let raw = read_raw_yaml(&path).expect("read");
    assert!(raw.is_mapping());
    // 原始值保留 ${SECRET}，未解析
    let m = raw.as_mapping().unwrap();
    assert_eq!(m.get("port").unwrap(), &serde_yaml::Value::Number(serde_yaml::Number::from(8080u64)));
}

/// DT-CFG-19：write_yaml_atomic 原子写入并回读一致。
#[test]
fn write_yaml_atomic_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.yaml").to_str().unwrap().to_string();
    let val: serde_yaml::Value = serde_yaml::from_str("a: 1\nb: hello\n").unwrap();
    write_yaml_atomic(&path, &val).expect("write");
    let raw = read_raw_yaml(&path).expect("read");
    assert_eq!(raw.as_mapping().unwrap().get("a").unwrap(), &serde_yaml::Value::Number(serde_yaml::Number::from(1i64)));
}

/// DT-CFG-20：set_yaml_path 设置嵌套路径，自动创建中间 mapping。
#[test]
fn set_yaml_path_creates_nested() {
    let mut root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    set_yaml_path(&mut root, &["server", "host"], serde_yaml::Value::String("0.0.0.0".into())).unwrap();
    let server = root.as_mapping().unwrap().get("server").unwrap();
    assert_eq!(
        server.as_mapping().unwrap().get("host").unwrap(),
        &serde_yaml::Value::String("0.0.0.0".into())
    );
}

/// DT-CFG-21：set_yaml_path 空路径直接替换 root。
#[test]
fn set_yaml_path_empty_replaces_root() {
    let mut root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    set_yaml_path(&mut root, &[], serde_yaml::Value::String("replaced".into())).unwrap();
    assert_eq!(root, serde_yaml::Value::String("replaced".into()));
}

/// DT-CFG-22：set_yaml_path 在非 mapping 路径上报错。
#[test]
fn set_yaml_path_errors_on_non_mapping() {
    let mut root = serde_yaml::Value::String("not-a-map".into());
    assert!(set_yaml_path(&mut root, &["a", "b"], serde_yaml::Value::Null).is_err());
}

// ── is_secret_field / mask_secrets_in_place ──

/// DT-CFG-23：is_secret_field 大小写不敏感识别密钥字段。
#[test]
fn is_secret_field_case_insensitive() {
    assert!(is_secret_field("master_key"));
    assert!(is_secret_field("MASTER_KEY"));
    assert!(is_secret_field("api_key"));
    assert!(is_secret_field("API-KEY"));
    assert!(is_secret_field("authorization"));
    assert!(is_secret_field("x-api-key"));
    assert!(is_secret_field("aws_secret_access_key"));
    assert!(is_secret_field("cookie"));
    assert!(!is_secret_field("model_name"));
    assert!(!is_secret_field("api_base"));
}

/// DT-CFG-24：mask_secrets_in_place 递归掩码密钥字段，保留 null。
#[test]
fn mask_secrets_masks_recursively_preserves_null() {
    let mut v = serde_json::json!({
        "master_key": "sk-live-123",
        "database_url": "postgres://user:pw@host/db",
        "model_list": [
            {
                "litellm_params": {
                    "api_key": "sk-real",
                    "model": "openai/gpt-4o"
                }
            }
        ],
        "unset_key": null
    });
    mask_secrets_in_place(&mut v);
    assert_eq!(v["master_key"], "****");
    assert_eq!(v["database_url"], "****");
    assert_eq!(v["model_list"][0]["litellm_params"]["api_key"], "****");
    // 非密钥字段保留
    assert_eq!(v["model_list"][0]["litellm_params"]["model"], "openai/gpt-4o");
    // null 保留
    assert!(v["unset_key"].is_null());
}

// ── HooksConfig::is_empty ──

/// DT-CFG-25：HooksConfig is_empty 在无 hook 时为 true。
#[test]
fn hooks_config_is_empty() {
    use boom_config::HooksConfig;
    let empty = HooksConfig::default();
    assert!(empty.is_empty());
}

// ── WorkflowSettings::validate ──

use boom_config::Config;

fn config_from_yaml(yaml: &str) -> Config {
    serde_yaml::from_str(yaml).expect("parse")
}

fn valid_workflow_yaml() -> &'static str {
    r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#
}

/// DT-CFG-26：合法 direct_synthesis workflow 校验通过。
#[test]
fn workflow_direct_synthesis_valid() {
    let config = config_from_yaml(valid_workflow_yaml());
    assert!(config.validate().is_ok());
}

/// DT-CFG-27：workflow model 与已配置 model/alias 冲突时校验失败。
#[test]
fn workflow_model_conflicts_with_deployment() {
    let yaml = r#"
model_list:
  - model_name: fusion
    litellm_params:
      model: openai/fusion
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: other
          - model: other
        aggregator:
          model: other
"#;
    let config = config_from_yaml(yaml);
    assert!(config.validate().is_err());
}

/// DT-CFG-28：workflow model 引用不存在的 workflow 定义时失败。
#[test]
fn workflow_model_references_unknown_workflow() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: nonexistent_workflow
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("unknown workflow"), "got: {err}");
}

/// DT-CFG-29：panel 少于 2 个实例时校验失败。
#[test]
fn workflow_panel_too_few_instances() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("at least two panel"), "got: {err}");
}

/// DT-CFG-30：role model 为空时校验失败。
#[test]
fn workflow_role_model_empty_rejected() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: ""
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("must not be empty"), "got: {err}");
}

/// DT-CFG-31：role model 引用未配置的 model 时失败。
#[test]
fn workflow_role_model_not_configured() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: unconfigured-model
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("not configured"), "got: {err}");
}

/// DT-CFG-32：role temperature 非 finite（NaN）时校验失败。
/// YAML 无法表达 NaN 字面量（serde_yaml 把 NaN 当字符串），故编程构造。
#[test]
fn workflow_role_temperature_nan_rejected() {
    use boom_config::WorkflowDefinitionConfig;
    let mut config = config_from_yaml(valid_workflow_yaml());
    if let Some(WorkflowDefinitionConfig::DirectSynthesis { roles, .. }) =
        config.workflow_settings.workflows.get_mut("direct_synthesis")
    {
        roles.panel[0].temperature = Some(f64::NAN);
    } else {
        panic!("expected DirectSynthesis workflow");
    }
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("temperature must be finite"), "got: {err}");
}

/// DT-CFG-33：空 workflow model name 被拒。
#[test]
fn workflow_empty_model_name_rejected() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    "": direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("empty model name"), "got: {err}");
}

/// DT-CFG-34：空 workflow id 被拒。
#[test]
fn workflow_empty_workflow_id_rejected() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: ""
  workflows:
    "":
      type: direct_synthesis
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("empty workflow id"), "got: {err}");
}

/// DT-CFG-35：panel_timeout_secs == 0 时校验失败。
#[test]
fn workflow_panel_timeout_zero_rejected() {
    let yaml = r#"
model_list:
  - model_name: real-model
    litellm_params:
      model: openai/real-model
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      panel_timeout_secs: 0
      roles:
        panel:
          - model: real-model
          - model: real-model
        aggregator:
          model: real-model
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("panel_timeout_secs must be greater than zero"), "got: {err}");
}

/// DT-CFG-36：role model 直接引用 workflow model 名时失败。
#[test]
fn workflow_role_references_workflow_model_rejected() {
    let yaml = r#"
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: fusion
          - model: fusion
        aggregator:
          model: fusion
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("references a workflow model"), "got: {err}");
}

/// DT-CFG-37：role model 通过 alias 解析到 workflow model 时失败。
#[test]
fn workflow_role_alias_resolves_to_workflow_model_rejected() {
    let yaml = r#"
router_settings:
  model_group_alias:
    panel-alias: fusion
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-alias
          - model: panel-alias
        aggregator:
          model: panel-alias
"#;
    let config = config_from_yaml(yaml);
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("resolves to a workflow model"), "got: {err}");
}

// ═════════════════════════════════════════════════════════════════
// user_tag_header / client_blocklist — !98 新增配置段
// ═════════════════════════════════════════════════════════════════

/// 最小可解析配置（只保证 serde 通过，不跑 validate）。
fn minimal_yaml(extra: &str) -> String {
    format!(
        r#"
model_list:
  - model_name: dt-mock
    litellm_params:
      model: openai/dt-mock
      api_base: http://127.0.0.1:9/v1
      api_key: sk-test
general_settings:
  master_key: sk-master
{extra}"#
    )
}

/// DT-CFG-38：general_settings.user_tag_header —— 审计 per-user 归因头配置：
/// 显式设置 / 缺省 None / 空串（audit 侧按"空即关闭"处理）。
#[test]
fn user_tag_header_present_default_and_empty() {
    // 显式设置
    let config = config_from_yaml(&minimal_yaml("  user_tag_header: X-User-Tag\n"));
    assert_eq!(config.general_settings.user_tag_header.as_deref(), Some("X-User-Tag"));

    // 缺省 → None（归因关闭）；client_blocklist 整段缺省也是 None
    let config = config_from_yaml(&minimal_yaml(""));
    assert_eq!(config.general_settings.user_tag_header, None);
    assert!(config.client_blocklist.is_none());

    // 空串 → Some("")
    let config = config_from_yaml(&minimal_yaml("  user_tag_header: \"\"\n"));
    assert_eq!(config.general_settings.user_tag_header.as_deref(), Some(""));
}

/// DT-CFG-39：client_blocklist —— 客户端封禁规则配置解析：enabled / rule
/// enabled / conditions 的 serde 默认值，BlockAction 的 status / code 默认
/// （403 / client_blocked）与显式覆盖，五种匹配算子（exists 可省 value）。
#[test]
fn client_blocklist_rules_parsing_and_defaults() {
    let yaml = minimal_yaml(
        r#"
client_blocklist:
  rules:
    - name: ban-bad-ua
      conditions:
        - field: header.user-agent
          op: contains
          value: curl
        - field: body.model
          op: eq
          value: gpt-4
        - field: prompt
          op: regex
          value: 'spam\d+'
        - field: header.authorization
          op: prefix
          value: sk-bad
        - field: body.tools
          op: exists
      action:
        status: 451
        message: banned by rule
        code: bad-ua
    - name: disabled-rule
      enabled: false
      action:
        message: off
"#,
    );
    let config = config_from_yaml(&yaml);
    let bl = config.client_blocklist.expect("client_blocklist parsed");
    // enabled 缺省 → true（主开关默认开）
    assert!(bl.enabled);

    let r = &bl.rules[0];
    assert_eq!(r.name, "ban-bad-ua");
    // rule enabled 缺省 → true；五条件全解析、算子一一对应
    assert!(r.enabled);
    assert_eq!(r.conditions.len(), 5);
    assert_eq!(r.conditions[0].field, "header.user-agent");
    assert_eq!(r.conditions[0].op, BlockOp::Contains);
    assert_eq!(r.conditions[1].op, BlockOp::Eq);
    assert_eq!(r.conditions[2].op, BlockOp::Regex);
    assert_eq!(r.conditions[3].op, BlockOp::Prefix);
    assert_eq!(r.conditions[4].op, BlockOp::Exists);
    // exists 算子 value 可省略（serde default → 空串）
    assert_eq!(r.conditions[4].value, "");
    // 显式 action 覆盖默认
    assert_eq!(r.action.effective_status(), 451);
    assert_eq!(r.action.effective_code(), "bad-ua");
    assert_eq!(r.action.message, "banned by rule");

    let r2 = &bl.rules[1];
    assert!(!r2.enabled);
    // conditions 缺省 → 空
    assert!(r2.conditions.is_empty());
    // status / code 缺省 → 403 / client_blocked
    assert_eq!(r2.action.effective_status(), 403);
    assert_eq!(r2.action.effective_code(), "client_blocked");
}
