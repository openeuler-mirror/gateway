# boom-config 去改动重构执行清单（决策 A：protocol 留守）

基线：upstream/master = a3575ed。目标：boom-config 对 upstream 的 diff 收敛到
仅 ProviderParams.protocol + manifest 两条登记（决策 A），其余四组增量全部移出。

## 移出项

1. ResponsesStoreSettings → boom-responses
   - struct + serde defaults 移入 crates/boom-responses/src/lib.rs（域归属：response store 自己的配置）
   - GeneralSettings.responses_store 字段删除；boom-main routes.rs 改为从 boom_config
     general_settings 行的原始 JSON 解析 responses_store（restore 后 boom-main 持有
     原始行值即可；或 ResponseStore::settings_from_value(&Value)）
   - 注意：GeneralSettings 无 flatten catch-all，serde 默认忽略未知字段——
     responses_store 留在 boom_config 行里不会被 Config 解析报错，但会被丢弃，
     所以必须由 boom-main 在 restore 路径单独解析并传递
2. bootstrap_from_env_with_hint → boom-main 新模块 src/bootstrap.rs
   - 提示文件读取（--config > config_master.yaml）、身份填补、server_hint、
     scrub 语义全部随迁；最终 serde_yaml::from_value::<Config> 走公开 Deserialize
   - boom-config 只保留 upstream 的 bootstrap_from_env 原样（回退我们对它的修改：
     master_key 可选化、错误文案、server_hint 逻辑全部移到 boom-main 侧）
3. ServerSettings 回退 upstream
   - host: String（default_host 0.0.0.0）、port: u16（default_port 4000）恢复
   - 三级解析（CLI > 提示文件 > 默认）在 boom-main bootstrap 组装 server 段实现；
     "默认层 WARN"逻辑保留在 main.rs 解析处
   - 连带回退 boom-config 契约测试的 Option 断言
4. manifest 清理
   - responses_store 三条 FieldMeta + general_settings required_fields 对应项删除
   - protocol 两条登记保留（决策 A）

## 保留项（决策 A）
- ProviderParams.protocol 字段 + manifest protocol FieldMeta + required_fields

## 验证
- git diff upstream/master -- crates/boom-config/ 仅剩 protocol 相关
- cargo test --workspace 零失败
- e2e：双 deployment Agent 7/7（直通+转换）、双实例 NOTIFY、--reboot 替换、
  responses_store 卡片读写（boom_config 行解析路径）
