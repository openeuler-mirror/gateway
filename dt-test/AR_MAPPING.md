# DT 用例 ↔ AR 验收项映射

把 boom-dt 的 **508 个 DT 用例**（20 个测试文件）对应到 5 条 AR。规则：

- 每个用例按其验证的行为归入**一个主 AR**；横切用例在"关联"栏标注（计入主 AR 的数量，
  不重复计数）。总数 508 = 各 AR 之和。
- 用例 ID 前缀与文件一一对应（如 `DT-ANT-*` 都在 `tests/anthropic.rs`）。
  各 AR 的表均为**函数名 → 用例 ID → 覆盖点**：函数名按用例 ID 升序排列、
  与 ID 逐位对应；编号跳号处按文件中实际存在的用例计（如 DT-FUS-31 不存在）。

| AR | 用例数 | 主要文件 |
| --- | ---: | --- |
| AR1 推理链路可观测 | 76 | trace.rs / trace_otlp.rs / alert.rs / stressmon.rs / core.rs / routing.rs / ctxaware.rs |
| AR2 权限·配额计费·路由兜底·限流 | 80 | limiter.rs / flowcontrol.rs / routing.rs / core.rs / ml_service.rs |
| AR3 协议转换·team 层级·认证校验 | 279 | anthropic.rs / provider.rs / azure.rs / core.rs / fusion.rs / routing_fusion.rs / routing.rs / auth.rs / config.rs |
| AR4 用户级 kvc 亲和调度 | 19 | routing.rs（KeyAffinityPolicy + 负载/迁移原语） |
| AR5 前缀 kvc 缓存感知调度 | 30 | kvindex.rs / routing.rs（KvcAwarePolicy）/ routing_fusion.rs / config.rs |
| 公共支撑（不专属单一 AR） | 24 | smoke.rs / config.rs / harness.rs |

---

## AR1 推理链路可观测（审计日志 / 缓存命中率统计 / 详细 prompt 上下文记录 / 模型检测与自恢复）— 76 例

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `parse_well_formed_traceparent` / `parse_unsampled_traceparent` / `parse_rejects_malformed` / `build_child_round_trips` / `tracestate_value_lookup` / `filter_matches_when_both_empty` / `filter_matches_on_tracestate_key` / `filter_matches_on_trace_id_regex` / `span_new_has_distinct_id_and_defaults` / `span_set_attribute_replaces_and_body_setters` / `span_finalize_ok_and_error` / `span_to_snapshot_encodes_ids_as_hex` / `registry_new_has_no_otlp` / `registry_start_finalize_ok_lands_in_recent` / `registry_finalize_error_bumps_counter` / `registry_finalize_unknown_is_noop` / `registry_with_span_mut` / `guard_none_when_disabled` / `guard_drop_finalizes_ok` / `guard_mark_error_routes_to_finalize_error` / `guard_attribute_setters_and_child_span_id`（trace.rs） | DT-TRC-01..21 | 请求级 trace 链路：W3C traceparent 解析/采样传播、RequestSpan 属性与 LLM 请求/响应体记录、finalize ok/error、recent ring、TraceGuard RAII |
| `exporter_starts_online` / `flush_pushes_batch_with_headers` / `flush_failure_goes_offline_and_drops_later_enqueues` / `probe_recovers_from_offline` / `probe_failure_counts_streak_without_offline` / `queue_overflow_drops_oldest` / `batch_full_triggers_background_flush` / `spawn_flush_task_flushes_on_tick` / `spawn_flush_task_to_handle_probes_and_recovers` / `convert_span_maps_ids_status_and_attributes` / `convert_span_carries_resource_metadata` / `ping_endpoint_matrix`（trace_otlp.rs） | DT-OTLP-01..12 | OTLP 导出：攒批/队列溢出/后台 flush；**失败重试→Offline→探测自恢复**（03/04/09）；protobuf 映射；ping 健康检查 |
| `raise_is_idempotent_per_key` / `clear_moves_alert_to_history` / `history_ring_is_bounded` / `active_keys_lists_keys` / `notifier_fires_on_transitions_only` / `notifier_can_be_cleared` / `default_equals_new` / `snapshot_sorts_active_by_raised_at_desc` / `no_notifier_does_not_panic`（alert.rs） | DT-ALR-01..09 | 告警状态机：raise/clear 幂等、history ring 有界、notifier 仅迁移触发 |
| `empty_snapshot_is_empty` / `snapshot_returns_recent_in_chronological_order` / `window_clamps_to_available` / `window_clamps_to_capacity` / `wraps_and_overwrites_oldest` / `cpu_over_threshold_increments_counter` / `cpu_under_threshold_keeps_zero_counter` / `zero_or_negative_window_clamps_to_one` / `timeseries_returns_full_snapshot`（stressmon.rs） | DT-STR-01..09 | worker 压力监控：CPU 时序采样环形缓冲、80% 阈值累计 |
| `error_should_log_to_db_excludes_expected_rejections` / `error_should_dedup_log_whitelist` / `error_dedup_superset_of_non_db` / `error_is_deployment_failure_classification`（core.rs） | DT-CORE-03..06 | **审计日志记录判定**（should_log_to_db / dedup 白名单）；**部署故障判定** is_deployment_failure（检测输入） |
| `debug_store_new_disabled_ignores_records` / `debug_store_enabled_records_and_lookup` / `debug_store_fifo_eviction_per_key` / `debug_store_per_key_limit_independent` / `debug_store_disable_clears_and_blocks` / `debug_store_clear_preserves_enabled`（core.rs） | DT-CORE-31..36 | **prompt 上下文记录**：DebugStore 按 key FIFO 记录、启停/清空 |
| `raw_capture_request_body_first_writer_wins` / `raw_capture_response_frames_join`（core.rs） | DT-CORE-43..44 | **详细 prompt 上下文**：raw_capture 原始请求体 + 响应帧记录 |
| `request_rate_record_and_snapshot` / `request_rate_rename_updates_label` / `request_rate_remove` / `mlstats_counters` / `mlstats_min_max_latency` / `mlstats_min_sentinel_when_no_success` / `mlstats_maybe_emit_summary_window`（routing.rs） | DT-RT-36..42 | 调用可观测：每模型/部署请求量、成败、时延 min/max、汇总发射窗口 |
| `record_buckets_by_path_and_aggregates_summary` / `cold_start_has_zero_ratio` / `record_tokens_accumulates_by_path` / `record_tokens_without_record_bumps_tokens_only` / `default_equals_new_and_event_labels` / `mixed_record_and_tokens_both_counted`（ctxaware.rs） | DT-CTX-04..09 | anthropic/openai 流量占比统计（关联 AR3 的协议识别） |

关联（计入其他 AR）：`DT-PRV-OAI-21` raw_capture 端到端（AR3）、`DT-PRV-OAI-22`
kv_cache_report_full 命中率条件全量上报开关（AR3）、`DT-CORE-41` usage 的
cached_tokens 统计（AR2）、`DT-KV-16/17` trie 命中率排序与 TTL 老化（AR5）。

## AR2 模型权限和配额计费管理 / 路由兜底 / 滑动窗口限流等 — 80 例

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `peek_and_commit_counts_dimension` / `peek_rejects_tokens_and_costs_dimensions` / `settle_usage_populates_windows_and_cumulative` / `window_expiry_resets_counters` / `cumulative_reset_returns_snapshot` / `dashboard_listing_queries` / `snapshot_restore_and_scan_edge_keys` / `clear_operations` / `decimal_micros_roundtrip` / `effective_limits_merge_shorthand` / `effective_limits_schedule_slots` / `validate_schedule_overlap_matrix` / `schedule_slot_activation_parsing` / `plan_store_crud_and_key_assignments` / `plan_store_team_assignments` / `concurrency_guard_acquire_release` / `guarded_stream_releases_on_end` / `migrations_ddl_contents`（limiter.rs） | DT-LM-01..18 | **滑动窗口限流**三段式（peek 权重感知/commit/settle，counts·tokens·costs 三维度）；窗口过期复位；累计配额 6 计数器与 reset；计划模板 schedule/stale；PlanStore key/team 分配三态与类型校验；并发守卫 RAII |
| `new_controller_has_no_slots` / `ensure_slot_creates_slot` / `ensure_slot_zero_removes_existing` / `ensure_slot_updates_existing_config` / `remove_slot_drops_slot` / `retain_slots_keeps_listed` / `acquire_no_slot_returns_no_slot` / `acquire_context_exceeded` / `acquire_immediate_dispatch` / `acquire_timeout_removes_from_queue` / `guard_drop_frees_slot` / `vip_dispatched_before_normal` / `periodic_dispatch_across_deployments` / `get_queued_waiters_lists_entries` / `get_dispatched_keys_lists_inflight` / `get_key_request_status_waiting_and_processing` / `deployment_queue_info_total_load_and_capacity` / `guard_wait_duration` / `flow_controlled_stream_passthrough_and_release` / `flow_controlled_stream_passthrough_no_guard`（flowcontrol.rs） | DT-FC-01..20 | 并发限流：槽位创建/派发/超时出队、VIP 优先、guard Drop 释放、排队/in飞可观测（关联 AR4 负载口径） |
| `alias_store_new_empty` / `alias_set_visible` / `alias_set_hidden_excluded_from_visible` / `alias_unhide_via_set` / `alias_remove` / `alias_all_and_clear`（routing.rs） | DT-RT-01..06 | 别名存储（路由解析链） |
| `store_quota_ratio` / `store_cost_rate` / `store_visibility_states` / `store_team_can_access` / `store_remove_clears_visibility` / `visibility_from_db_combinations` / `parse_allowed_teams_forms`（routing.rs） | DT-RT-16..20, 26, 27 | **模型权限**：quota_ratio、费率、可见性 Public/Private + team ACL（DB 行解析） |
| `cost_rate_no_cached_pricing` / `cost_rate_with_cached_pricing` / `cost_rate_cached_saturates_to_input` / `cost_rate_is_zero`（routing.rs） | DT-RT-22..25 | **配额计费**：compute_cost、cached 折扣价、cached_tokens 截断 |
| `auto_router_non_matching_model` / `auto_router_tier_classification` / `auto_router_tool_request_medium_plus` / `auto_router_unknown_tier_falls_back` / `router_with_classifier`（routing.rs） | DT-RT-43..46, 72 | tier 分级路由（成本感知分流 + 分类器挂载） |
| `router_resolve_model_name` / `router_resolve_candidates_cascade` / `router_empty_group_suppresses_fallback`（routing.rs） | DT-RT-64..66 | **路由兜底**：精确→别名→`"*"` 通配级联；空组抑制 fallback |
| `provider_cost_total_and_add` / `provider_billing_accumulates_cost` / `provider_billing_accumulates_usage_nested` / `provider_billing_usage_none_details`（core.rs） | DT-CORE-39..42 | ProviderCost/ProviderBilling 成本与 usage 累计（含 cached_tokens） |
| `window_limit_is_empty` / `deserialize_window_limit_vec_array_form` / `deserialize_window_limit_vec_object_form` / `plan_type_default_and_serde`（core.rs） | DT-CORE-56..58, 69 | WindowLimit 判空与紧凑/对象两种反序列化、PlanType serde（限流配置容错） |
| `try_new_validation_matrix` / `new_stats_and_name` / `classify_valid_tier_used_directly` / `classify_unknown_tier_falls_back` / `classify_non_2xx_falls_back` / `classify_malformed_json_falls_back` / `classify_connection_refused_falls_back` / `classify_forwards_tools_in_body`（ml_service.rs） | DT-ML-01..08 | 外置 ML 分级服务：合法 tier 直用、非 2xx/坏 JSON/连接失败全回退矩阵 |
| `flow_control_queue_timeout_secs_default_and_override`（config.rs） | DT-CFG-11 | flow_control 排队超时配置默认/覆盖 |

关联：`DT-CORE-49` 预算超限判定（AR3 认证链路）。

## AR3 集群调度网关接入：openai/anthropic 等主流协议转换 / team 用户层级管理 / 认证和权限校验等 — 279 例

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `extract_system_text_forms` / `anthropic_request_system_text_to_openai` / `anthropic_request_system_blocks_to_openai` / `anthropic_request_user_assistant_text` / `anthropic_request_user_image_url_block` / `anthropic_request_user_image_base64_block` / `anthropic_request_user_tool_result_text` / `anthropic_request_user_tool_result_error_prefix` / `anthropic_request_user_tool_result_image_placeholder` / `anthropic_request_user_tool_result_none_content` / `anthropic_request_user_document_block` / `anthropic_request_user_mixed_text_and_tool_result` / `anthropic_request_assistant_tool_use` / `anthropic_request_assistant_thinking_parts` / `anthropic_request_assistant_redacted_thinking_skipped` / `anthropic_request_assistant_document_block` / `anthropic_request_tools_conversion` / `anthropic_request_stop_sequences` / `anthropic_request_extra_fields_passthrough` / `anthropic_request_sampling_passthrough` / `response_text_to_anthropic` / `response_top_level_reasoning_to_thinking` / `response_parts_reasoning_to_thinking` / `response_null_content_fallback_empty_text` / `response_empty_choices_fallback` / `response_tool_calls_to_tool_use` / `response_tool_calls_invalid_json_to_null` / `response_finish_reason_mapping` / `response_usage_passthrough` / `response_generates_msg_id` / `transcoder_text_stream` / `transcoder_thinking_block` / `transcoder_thinking_then_text` / `transcoder_text_then_thinking` / `transcoder_tool_call_blocks` / `transcoder_finish_holds_then_releases_on_next_chunk` / `transcoder_drain_releases_held_finish` / `transcoder_extracts_usage` / `transcoder_empty_delta_first_chunk_emits_message_start` / `transcoder_finish_tool_calls`（anthropic.rs） | DT-ANT-01..40 | **Anthropic↔OpenAI 协议转换**：system/消息块（Image/ToolResult/ToolUse/Thinking/Document）、tools、finish_reason 映射、流式转码器（message_start/delta/块切换/usage 释放） |
| `sse_lf_crlf_cr_line_endings` / `sse_trailing_lone_cr_closed_by_finish` / `sse_crlf_split_across_pushes` / `sse_data_prefix_without_space` / `sse_event_field_captured` / `sse_comments_and_unknown_fields_ignored` / `sse_bom_stripped_once` / `sse_utf8_split_across_pushes` / `sse_finish_dispatches_pending` / `sse_empty_event_no_data_not_dispatched` / `sse_raw_preserves_line_endings` / `sse_take_pending_raw_incomplete_frame` / `sse_field_without_colon_empty_value`（provider.rs） | DT-PRV-SSE-01..13 | SSE 分帧容错：LF/CRLF/CR、跨 push 分割、BOM、注释、原始帧捕获 |
| `create_provider_protocol_per_type` / `create_provider_unknown_type_errors` / `custom_headers_sanitization` / `reserved_key_configures_anthropic_version` / `reserved_key_configures_azure_api_version` / `reserved_key_configures_bedrock_region` / `auto_detect_provider_from_model_name` / `vllm_ollama_no_key_uses_placeholder` / `openai_no_key_constructs` / `provider_trait_accessors` / `deployment_id_passed_through` / `client_type_header_flag_passed` / `kv_worker_id_from_api_base_variants` / `create_provider_timeout_zero_clamped`（provider.rs） | DT-PRV-LIB-01..14 | provider 工厂：类型/protocol、custom_headers 净化、reserved key、auto_detect、timeout 钳制（LIB-13 kv_worker_id 派生关联 AR4/5） |
| `openai_chat_success` / `openai_chat_no_usage` / `openai_chat_parse_error_carries_raw_body` / `openai_chat_bom_parses` / `openai_chat_sse_assembled` / `openai_chat_partial_usage_parses` / `openai_chat_null_identity_parses` / `openai_chat_unknown_role_parses` / `openai_chat_unknown_content_part_preserved` / `openai_chat_upstream_error` / `openai_chat_gateway_headers_and_bearer` / `openai_chat_no_priority_header_when_empty` / `openai_chat_extra_fields_forwarded` / `openai_chat_stream_success` / `openai_chat_stream_crlf` / `openai_chat_stream_data_no_space` / `openai_chat_stream_unknown_role` / `openai_chat_stream_tool_call_no_index` / `openai_chat_stream_upstream_error` / `openai_chat_stream_gateway_headers` / `openai_chat_raw_capture` / `openai_chat_kv_cache_report_full` / `openai_chat_reasoning_part_converted_to_text` / `openai_provider_direct_construct_with_headers`（provider.rs） | DT-PRV-OAI-01..24 | OpenAI 兼容 chat/stream：usage 透传、错误映射、gateway_headers、extra 透传、SSE 装配、raw_capture（21，关联 AR1）、kv_cache_report_full（22，关联 AR5） |
| `anthropic_chat_success` / `anthropic_chat_headers` / `anthropic_custom_api_version` / `anthropic_chat_upstream_error` / `anthropic_chat_stream_success` / `anthropic_chat_stream_crlf` / `anthropic_provider_direct_construct`（provider.rs） | DT-PRV-ANT-01..07 | Anthropic provider 行为：头注入（x-api-key/anthropic-version）、流式 |
| `gemini_provider_construct` / `gemini_provider_direct_construct` / `gemini_system_and_user_text` / `gemini_system_parts` / `gemini_assistant_tool_calls` / `gemini_tool_result_message` / `gemini_tool_result_parts` / `gemini_unknown_role_as_user` / `gemini_user_parts_all_variants` / `gemini_data_uri_no_base64_falls_back` / `gemini_generation_config_all_fields` / `gemini_stop_single` / `gemini_tools_with_description` / `gemini_chat_stream_builds_request` / `gemini_no_api_key` / `gemini_provider_accessors`（provider.rs） | DT-PRV-GEM-01..02, 10..23 | **Gemini 协议转换**：systemInstruction、functionCall/Response parts、generationConfig、tools |
| `azure_provider_construct` / `azure_chat_success` / `azure_provider_direct_construct` + `bedrock_provider_construct` / `bedrock_provider_direct_construct`（provider.rs） | DT-PRV-AZU-01..03, DT-PRV-BD-01..02 | Azure / Bedrock 构造与协议 |
| `azure_accessors` / `azure_chat_success` / `azure_chat_custom_api_version` / `azure_chat_gateway_headers_forwarded` / `azure_chat_non_2xx_error` / `azure_chat_connection_refused` / `azure_stream_success` / `azure_stream_non_2xx_error` / `azure_stream_connection_refused` / `azure_stream_dropped_early_terminates_pump`（azure.rs） | DT-AZ-01..10 | Azure OpenAI：deployment URL 改写、api-version、api-key 头、流式 |
| `error_status_code_all_variants` / `error_upstream_status_code_constant_regardless_of_status` / `error_type_all_variants` / `error_type_rate_limit_passes_through_limit_type` / `error_raw_upstream_body_only_for_parse_error`（core.rs） | DT-CORE-01, 02, 07..09 | 错误→HTTP 状态码/type 映射（接入行为契约）、raw_upstream_body |
| `normalize_alternation_consecutive_same_role` / `normalize_alternation_already_alternating_unchanged` / `normalize_alternation_tool_user_pairs_trigger_separator` / `normalize_alternation_system_does_not_trigger` / `normalize_alternation_empty_and_single_unchanged` / `convert_tool_choice_none_input` / `convert_tool_choice_none_input_parallel_false` / `convert_tool_choice_none_type_strips_tools` / `convert_tool_choice_auto` / `convert_tool_choice_required_maps_to_any` / `convert_tool_choice_function_maps_to_tool` / `convert_tool_choice_function_missing_name_defaults_empty` / `convert_tool_choice_unknown_type_passthrough` / `convert_tool_choice_missing_type_defaults_auto` / `convert_image_source_url` / `convert_image_source_base64` / `convert_image_source_unknown_type`（core.rs） | DT-CORE-10..26 | 协议归一原语：消息角色交替、tool_choice 五型转换、image source 三型 |
| `host_of_api_base_ipv4_and_hostname` / `host_of_api_base_ipv6_literal` / `host_of_api_base_empty_returns_none` / `host_of_api_base_empty_ipv6_brackets_returns_none`（core.rs） | DT-CORE-27..30 | api_base host 解析（kv_worker_id 基础，关联 AR4/5） |
| `key_format_valid_prefixes` / `key_format_invalid_prefixes` / `hard_blocked_header_policy` / `hard_blocked_header_allows_ordinary` / `auth_identity_can_call_model` / `auth_identity_is_expired` / `auth_identity_is_budget_exceeded`（core.rs） | DT-CORE-37, 38, 45..49 | **认证和权限校验**：key 前缀格式、网关 header 硬阻断、can_call_model/is_expired/is_budget_exceeded |
| `message_normalize_reasoning_extracts_and_collapses_text` / `message_normalize_reasoning_only_parts` / `message_normalize_reasoning_keeps_multipart` / `message_normalize_reasoning_no_reasoning_unchanged` / `content_part_serde_roundtrip` / `content_part_deserialize_edge_cases` / `completion_request_into_chat_request` / `usage_lenient_deserialize` / `chat_completion_response_lenient_identity` / `tool_call_lenient_string_fields` / `message_role_serde_lowercase_and_unknown_fallback` / `stop_sequence_untagged_forms` / `message_content_default_and_untagged`（core.rs） | DT-CORE-50..55, 59..64, 70 | 消息/usage/响应 serde 容错（兼容各家属格式） |
| `storage_tier_priority_score_ordered` / `storage_tier_serde_lowercase`（core.rs） | DT-CORE-65, 66 | StorageTier 集群分层存储优先级 |
| `otlp_config_default_values` / `otlp_config_serde_roundtrip`（core.rs） | DT-CORE-67, 68 | OtlpConfig 默认与 serde（关联 AR1） |
| `store_new_empty` / `store_set_deployments` / `store_set_empty_list_keeps_key` / `store_add_deployment` / `store_exclusive_deployment` / `store_remove_deployments` / `store_find_and_remove_by_deployment_id` / `store_select_round_robin` / `store_get_providers_and_names` / `store_clear_all` / `migrations_ddl_content`（routing.rs） | DT-RT-07..15, 21, 73 | **集群调度接入**：DeploymentStore 多 provider 轮询、独占模型、增量增删、migrations DDL |
| `router_select_with_candidates` / `router_select_provider_with_prefix` / `router_access_checks` / `router_visible_model_names` / `router_policy_name_and_swap`（routing.rs） | DT-RT-67..71 | Router 选择委托、策略热替换、可见模型列表 |
| `hash_token_is_sha256_of_whole_raw_key` / `master_key_authenticates_with_full_access` / `no_master_no_db_rejects_everything` / `check_model_access_matrix` / `lookup_key_aliases_without_db_returns_empty`（auth.rs） | DT-AU-01..05 | **认证**：整键 SHA-256（litellm 兼容）、主密钥常量时间比较、无库拒绝、check_model_access |
| `workflow_role_as_str` / `workflow_failure_display_and_source` / `model_invoker_default_invoke_stream_unsupported` / `workflow_default_execute_stream_unsupported` / `new_rejects_empty_id` / `new_rejects_single_panel` / `new_rejects_empty_panel_model` / `new_rejects_empty_aggregator_model` / `new_accepts_valid_config_and_id` / `registry_empty` / `registry_rejects_empty_model_name` / `registry_rejects_unknown_workflow` / `registry_lookup_and_sorted_names` / `execute_aggregates_two_panels` / `execute_panel_parts_reasoning_content` / `execute_tools_passthrough_and_empty_stripped` / `execute_empty_tools_array_stripped` / `execute_single_panel_with_tools_skips_aggregator` / `execute_aggregator_error_falls_back_to_first_panel` / `execute_aggregator_missing_fails` / `execute_aggregator_empty_not_revalidated` / `execute_rejects_n_gt_1_before_child_calls` / `execute_all_panels_fail_retries_and_reports` / `execute_retry_after_first_round_all_fail` / `execute_single_panel_no_tools_rejected` / `execute_panel_timeout_treated_as_failure` / `execute_stream_aggregates_usage` / `execute_stream_single_panel_with_tools_returns_panel_stream` / `execute_stream_aggregator_missing_fails` / `execute_stream_aggregator_error_wraps_with_workflow_id` / `response_stream_passes_tool_calls_and_reasoning` / `execute_tools_request_uses_reference_context_template` / `execute_no_tools_uses_self_moa_template` / `execute_panel_null_content_treated_as_invalid` / `execute_no_user_message_empty_question` / `execute_panel_no_choices_invalid_reason` / `execute_stream_negative_usage_clamped_to_zero` / `execute_stream_overflow_usage_capped_at_i32_max` / `execute_stream_no_cache_details_stays_none`（fusion.rs） | DT-FUS-01..30, 32..40 | **team/多模型编排**：工作流 panel+aggregator、重试、回退、流式聚合、usage/成本归账、prompt 模板 |
| `fusion_provider_requires_context` / `fusion_registration_is_exclusive` / `registration_rejects_deployment_conflict` / `registration_rejects_alias_conflict` / `registration_rejects_native_protocol_children` / `fusion_chat_full_pipeline` / `fusion_chat_non_vip_gets_zero_priority` / `fusion_chat_with_foreign_prompt_trace` / `fusion_stream_lazy_then_full_pipeline` / `fusion_stream_aggregator_start_error_falls_back_to_panel` / `fusion_stream_mid_stream_error_propagates` / `fusion_stream_dropped_midway_marks_unfinished` / `fusion_aggregator_failure_falls_back_to_panel` / `fusion_invalid_panels_error_but_bill_usage` / `fusion_missing_child_fails_at_runtime_wildcard_does_not_take_over` / `fusion_child_alias_resolving_to_fusion_rejected` / `fusion_child_flow_control_queue_timeout` / `fusion_child_context_exceeded` / `fusion_router_dropped_reports_unavailable` / `gateway_headers_combinations`（routing_fusion.rs） | DT-RF-01..19, 21 | fusion 虚拟模型端到端接入：独占候选集、冲突校验、非流式/流式全链路、队列与上下文限流联动、网关头构造（RF-20 归 AR5） |
| `resolve_explicit_provider_prefix` / `auto_detect_openai_family` / `auto_detect_other_providers` / `auto_detect_unknown_defaults_to_openai` / `model_group_alias_variants`（config.rs） | DT-CFG-01..05 | provider 前缀拆分与别名配置 |
| `workflow_direct_synthesis_valid` / `workflow_model_conflicts_with_deployment` / `workflow_model_references_unknown_workflow` / `workflow_panel_too_few_instances` / `workflow_role_model_empty_rejected` / `workflow_role_model_not_configured` / `workflow_role_temperature_nan_rejected` / `workflow_empty_model_name_rejected` / `workflow_empty_workflow_id_rejected` / `workflow_panel_timeout_zero_rejected` / `workflow_role_references_workflow_model_rejected` / `workflow_role_alias_resolves_to_workflow_model_rejected`（config.rs） | DT-CFG-26..37 | workflow（fusion）配置校验全矩阵 |
| `anthropic_path_exact_and_subpath` / `classify_routes_messages_to_anthropic` / `wire_label_and_header_constant`（ctxaware.rs） | DT-CTX-01..03 | 协议路径识别（anthropic 流量判定，关联 AR1 统计） |

**team 用户层级管理**的证据分散在：`DT-RT-19/26/27`（team ACL 模型访问）、
`DT-LM-15`（team plan 分配/默认 team plan）、`DT-LM-03`（team 累计配额 scope）、
`DT-FUS-*`（多模型编排）——auth 的 team 解析本体（lookup_team/
resolve_team_models）在 DB 路径上，见下"缺口"。

## AR4 推理集群 kvc 亲和调度器：用户级请求前缀 kvc 亲和调度 — 19 例

用户（key_hash）维度的亲和：同用户请求粘到同一 worker，含 warm-up 与过载迁移。

| 函数名（routing.rs） | 用例 | 覆盖点 |
| --- | --- | --- |
| `strategy_registry_register_lookup` | DT-RT-47 | 策略注册表（调度策略框架） |
| `round_robin_policy` | DT-RT-48 | RR 基线（无亲和时的行为） |
| `shuffle_policy_distribution` | DT-RT-49 | Shuffle 基线 |
| `key_affinity_empty_and_single` | DT-RT-50 | KeyAffinityPolicy 空候选/单候选 |
| `key_affinity_no_key_falls_back_to_lowest_load` | DT-RT-51 | 无 key_hash → 最低负载 |
| `key_affinity_initial_assignment_and_hit` | DT-RT-52 | 首次最低负载 + 记录亲和，同 key 再来命中 |
| `key_affinity_warmup_below_threshold` | DT-RT-53 | warm-up：上下文不足阈值不走亲和 |
| `key_affinity_rebalance_migrates` | DT-RT-54 | 亲和 worker 过载 → 迁移最低负载 |
| `inflight_new_empty` / `inflight_guard_model_level` / `inflight_guard_deployment_level` / `inflight_multiple_guards_accumulate` | DT-RT-28..31 | 模型/部署级在飞计数（负载信号，RAII 增减） |
| `rebalance_counter_record_snapshot` / `rebalance_move_tracker_basic` / `rebalance_move_tracker_none_and_empty` / `rebalance_move_tracker_same_id` | DT-RT-32..35 | 迁移计数与 move 追踪（亲和迁移可观测） |
| `load_helpers_should_rebalance` / `load_helpers_deployment_load` / `load_helpers_min_load_candidate` | DT-RT-61..63 | 共享负载原语：迁移判定、部署负载口径、最低负载候选（与 AR5 共用） |

关联：`DT-KV-09` record_request_prefix（用户请求前缀入库，AR5）、
`DT-FC-17` DeploymentQueueInfo total_load（AR2）。

## AR5 推理集群 kvc 亲和调度器：请求前缀 kvc 缓存感知和预测的调度策略 — 30 例

请求前缀（system+tools+messages 按 512B 块哈希）维度的缓存感知调度：trie 自学习、
命中率评分、降级回退。

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `single_root_block_match` | DT-KV-01 | 单块前缀命中（kvindex.rs） |
| `chained_prefix_partial_match` | DT-KV-02 | 链式前缀部分命中深度 |
| `store_batch_equivalent_to_stores` | DT-KV-03 | 批量存块等价性 |
| `find_matches_unknown_model_returns_empty` | DT-KV-04 | 未知模型空结果 |
| `find_matches_excludes_non_matching_worker` | DT-KV-05 | 前缀不匹配 worker 排除 |
| `remove_worker_clears_claims` | DT-KV-06 | worker 下线清 claim |
| `evict_blocks_removes_specific_hash` | DT-KV-07 | 按哈希精确驱逐 |
| `lru_evicts_oldest_when_full` | DT-KV-08 | LRU 容量驱逐（over-approximation 老化） |
| `record_request_prefix_stores_blocks` | DT-KV-09 | 自学习：请求前缀直接入库（不经事件） |
| `prefix_block_count_chunks_by_block_size` | DT-KV-10 | 前缀按块大小切分计数 |
| `find_matches_empty_inputs_return_empty` | DT-KV-11 | 空输入边界 |
| `empty_block_bytes_store_is_ignored` | DT-KV-12 | 空块忽略 |
| `node_count_tracks_trie_nodes` | DT-KV-13 | trie 节点统计 |
| `model_names_lists_registered_models` | DT-KV-14 | 注册模型列表 |
| `block_capacity_returns_configured_max` | DT-KV-15 | 容量配置 |
| `multiple_workers_sorted_by_score` | DT-KV-16 | 多 worker 命中按 combined_score（命中率）降序 |
| `prune_expired_removes_old_blocks` | DT-KV-17 | TTL prune 过期块 |
| `debug_dump_returns_populated_nodes` | DT-KV-18 | trie 调试转储 |
| `kvc_aware_empty_and_single` | DT-RT-55 | KvcAwarePolicy 空候选/单候选（routing.rs） |
| `kvc_aware_empty_prefix_degraded` | DT-RT-56 | 空前缀 → 最低负载（degraded） |
| `kvc_aware_no_kv_worker_id_falls_back` | DT-RT-57 | 候选无 kv_worker_id → 最低负载 |
| `kvc_aware_cold_round_robin` | DT-RT-58 | 冷启动 trie 空 → 全 0 分 round-robin |
| `kvc_aware_affinity_hit` | DT-RT-59 | trie 命中 → 亲和派发到命中 worker（hit_ratio>0） |
| `kvc_aware_rebalance_when_winner_overloaded` | DT-RT-60 | winner 过载超阈值 → 迁移 |
| `fusion_records_request_prefix_into_kv_index`（routing_fusion.rs） | DT-RF-20 | 端到端：路由后前缀写入 trie → 同前缀请求命中 |
| `kvc_aware_valid_passes` / `kvc_aware_invalid_ttl_rejected` / `kvc_aware_zero_max_blocks_rejected` / `kvc_aware_zero_ttl_is_valid` / `load_config_rejects_invalid_rebalance_threshold`（config.rs） | DT-CFG-07..10, 15 | kvc_aware 配置校验（TTL/max_blocks/rebalance_threshold 边界） |

关联：`DT-PRV-OAI-22` kv_cache_report_full（命中率驱动条件全量上报，AR3）、
`DT-CORE-27..30` kv_worker_id 从 api_base 派生（AR3）、`DT-RT-61..63` 共享负载原语（AR4）。

## 公共支撑 — 24 例

不验证单一 AR 的业务行为，是全部 AR 的地基：

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `config_load_parses_model_list` / `provider_chat_roundtrip_via_mock_upstream` / `deployment_store_round_robin_rotates`（smoke.rs） | DT-SMOKE-01..03 | 配置解析 / provider 构造 / 路由存储冒烟 |
| `resolve_env_value_patterns` / `load_minimal_config_succeeds` / `load_config_rejects_invalid_kvc_aware` / `load_config_missing_file_errors` / `load_config_resolves_env_vars` / `lookup_cost_template_finds_by_name` / `read_raw_yaml_preserves_env_refs` / `write_yaml_atomic_roundtrip` / `set_yaml_path_creates_nested` / `set_yaml_path_empty_replaces_root` / `set_yaml_path_errors_on_non_mapping` / `is_secret_field_case_insensitive` / `mask_secrets_masks_recursively_preserves_null` / `hooks_config_is_empty`（config.rs） | DT-CFG-06, 12..14, 16..25 | 环境变量展开、配置加载/缺失文件、原 YAML 读取、原子写、嵌套路径设置、密钥识别与递归掩码、hooks 空态 |
| `free_port_returns_bindable_port` / `chat_request_deserializes_fields_and_extra` / `chat_request_panics_on_type_mismatch` / `simple_chat_request_builds_minimal_request` / `mock_upstream_start_chat_ok_serves_200` / `mock_upstream_empty_then_remount_overrides` / `test_server_serves_router_and_stops_on_drop`（harness.rs） | DT-HAR-01..07 | DT harness 自身（free_port / chat_request / MockUpstream 起停与 remount / TestServer 服务与 Drop 清理），保证测试基础设施本身被验证 |

## 覆盖缺口（验收时需知）

1. **审计日志（AR1）**：boom-audit（boom_request_log 落库）本体无 DT —— 现有证据是
   记录判定原语（`DT-CORE-03..05`）与 trace 链路；落库 SQL 属 `*_db` 跳过范围。
2. **详细 prompt 上下文记录（AR1）**：boom-promptlog 落盘本体无 DT —— 现有证据是
   raw_capture（`DT-CORE-43/44`、`DT-PRV-OAI-21`）与 DebugStore（`DT-CORE-31..36`）。
3. **模型检测与自恢复（AR1）**：部署健康检查/熔断无独立 DT —— 现有证据是故障判定
   （`DT-CORE-06`）、告警状态机（`DT-ALR-*`）、OTLP 导出器自恢复（`DT-OTLP-03/04/09`）。
4. **team 用户层级管理（AR3）**：auth 的 team 解析（lookup_team/
   resolve_team_models、blocked/expired/budget 校验）在 DB 路径上无 Postgres 不可达，
   见 README"DB 路径约定"。
