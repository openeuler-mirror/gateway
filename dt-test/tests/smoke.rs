//! DT 冒烟用例 — 进程内直调 boom-* pub API，验证覆盖链路本身可用。
//! 后续正式 DT 用例按 `testcase/cases/` 的域分文件（access / invocation /
//! billing / quota / admin / scheduling），沿用这里的模式。

use boom_dt::{simple_chat_request, MockUpstream};
use std::collections::HashMap;

/// DT-SMOKE-01：boom_config 解析最小 model_list 配置（覆盖 YAML 解析路径）。
#[tokio::test]
async fn config_load_parses_model_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        r#"
model_list:
  - model_name: dt-mock-model
    litellm_params:
      model: openai/dt-mock-model
      api_base: http://127.0.0.1:9/v1
      api_key: sk-dt-upstream
general_settings:
  master_key: sk-dt-master
"#,
    )
    .unwrap();

    let config = boom_config::load_config(path.to_str().unwrap()).expect("load_config");
    assert_eq!(config.model_list.len(), 1);
    assert_eq!(config.model_list[0].model_name, "dt-mock-model");
}

/// DT-SMOKE-02：boom_provider::create_provider 构造 OpenAI provider，
/// 经真实 HTTP 调用 mock 上游拿回回复（覆盖 provider 的请求组装/响应解析）。
#[tokio::test]
async fn provider_chat_roundtrip_via_mock_upstream() {
    let upstream = MockUpstream::start_chat_ok("hello from dt mock").await;

    let provider = boom_provider::create_provider(
        "openai/dt-mock-model",
        Some("sk-dt-upstream".to_string()),
        Some(format!("{}/v1", upstream.uri())),
        30,
        &HashMap::new(),
        &HashMap::new(),
        None,
        false,
    )
    .expect("create_provider");

    let resp = provider
        .chat(simple_chat_request("dt-mock-model", "hi"))
        .await
        .expect("provider.chat");
    match &resp.choices[0].message.content {
        boom_core::types::MessageContent::Text(text) => {
            assert_eq!(text, "hello from dt mock");
        }
        other => panic!("expected Text content, got {other:?}"),
    }
}

/// DT-SMOKE-03：boom_routing::DeploymentStore 注册两个 deployment，
/// round-robin 轮询两个都出现（覆盖 store 的注册/选择/计数器路径）。
#[tokio::test]
async fn deployment_store_round_robin_rotates() {
    let upstream = MockUpstream::start_empty().await;
    let extra = HashMap::new();

    let p1 = boom_provider::create_provider(
        "openai/dt-mock-model",
        None,
        Some(format!("{}/v1", upstream.uri())),
        30,
        &extra,
        &HashMap::new(),
        None,
        false,
    )
    .unwrap();
    let p2 = boom_provider::create_provider(
        "openai/dt-mock-model",
        None,
        Some(format!("{}/v1", upstream.uri())),
        30,
        &extra,
        &HashMap::new(),
        Some("deploy-2".to_string()),
        false,
    )
    .unwrap();

    let store = boom_routing::DeploymentStore::new();
    assert!(store.set_deployments("dt-mock-model".into(), vec![p1, p2]));
    assert_eq!(store.get_providers("dt-mock-model").unwrap().len(), 2);

    // round-robin：连续 select 两次应命中两个不同实例
    let a = store.select("dt-mock-model").unwrap();
    let b = store.select("dt-mock-model").unwrap();
    assert_ne!(
        a.deployment_id(),
        b.deployment_id(),
        "round-robin should rotate"
    );
    // 未注册的模型返回 None
    assert!(store.select("no-such-model").is_none());
}
