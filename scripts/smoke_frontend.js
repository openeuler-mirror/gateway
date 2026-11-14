#!/usr/bin/env node
// smoke_frontend.js — 前端渲染冒烟测试（无依赖，node 直接跑）。
//
// 背景：node --check 只查语法，抓不到未定义引用（PR1 的 header_row 事故）。
// 本脚本从 app.js 提取目标函数，stub 掉 document/t/esc 等外部符号后真实执行，
// 断言关键输出。改 renderModelsTable / renderCardRollback / 池徽标后必须跑：
//
//   node scripts/smoke_frontend.js
//
// 覆盖：池视图分组（无层分布文本）、徽标四态（关闭/未加入/混配/激活/无回落目标）、
// 价格列头对齐（无 num 右对齐）、回落卡片模型勾选列表、空配置 unchecked。

const fs = require('fs');
const path = require('path');

const APP = path.join(__dirname, '..', 'crates', 'boom-dashboard', 'src', 'frontend', 'app.js');
const src = fs.readFileSync(APP, 'utf8');

// ── stubs ─────────────────────────────────────────────────────────
const t = (k, o) => (o ? k.replace(/\{(\w+)\}/g, (_, n) => o[n]) : k);
const esc = (s) => String(s);
const renderDeployCell = (name) => esc(name);
const visibilityBadgeFor = () => '';
const vendorOf = () => 'default';
const tip = (k) => `[${k}]`;
const el = { innerHTML: '', checked: false, indeterminate: false, disabled: false, closest: () => null };
const document = { getElementById: () => el, querySelectorAll: () => [] };

function fail(msg) { console.error('SMOKE FAIL: ' + msg); process.exit(1); }

// ── extract + eval function blocks ────────────────────────────────
function loadBlock(startMarker, endMarker, returns) {
  const start = src.indexOf(startMarker);
  const end = src.indexOf(endMarker);
  if (start < 0 || end < 0 || end <= start) fail(`block not found: ${startMarker}`);
  const block = src.slice(start, end);
  return new Function('t', 'esc', 'tip', 'renderDeployCell', 'visibilityBadgeFor', 'vendorOf', 'document',
    block + `\nreturn { ${returns} };`
  )(t, esc, tip, renderDeployCell, visibilityBadgeFor, vendorOf, document);
}

const pool = loadBlock('  function rollbackPoolBadge', '  async function showNewModelModal',
  'renderModelsTable, rollbackPoolBadge');


// ── fixtures ──────────────────────────────────────────────────────
const models = [
  { id: '1', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: true, rollback_enabled: true, max_context_tokens: 204800, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '2', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: true, rollback_enabled: true, max_context_tokens: 204800, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '3', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: false, auto_disabled: true, rollback_enabled: true, max_context_tokens: 1048576, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '4', model_name: 'qwen3', litellm_model: 'openai/qwen3', enabled: true, quota_count_ratio: 1, timeout: 1200, source: 'db' }, // 未开启 → 无徽标
];

// ── 1. 池头行 + 缩进行结构（无独立 rollback 列）────────────────
pool.renderModelsTable(models, {});
const html = el.innerHTML;
// 无独立 rollback 列
if (html.includes('col-rollback')) fail('dedicated rollback column removed');
if (html.includes('cell-rollback')) fail('per-row rollback cells removed');
// 池头行：每池一个，含开关+名称+计数+徽标
const headers = html.match(/class="pool-header"/g) || [];
if (headers.length !== 2) fail(`2 pools → 2 header rows, got ${headers.length}`);
if (!html.includes('pool-header-inner')) fail('header has inner layout div');
if (!html.includes('rb-switch')) fail('iOS switch present');
if (!html.includes('rb-slider')) fail('slider present');
if (!html.includes('pool-name')) fail('pool name span present');
if (!html.includes('pool-count')) fail('instance count present');
// 勾选状态在池头行的 checkbox
if (!html.includes("window._togglePoolRollback('glm-5.3'")) fail('glm toggle wired');
if (!html.includes('checked')) fail('on pool has checked switch');
// 激活徽标在池头行
if (!html.includes('models.rollback.active')) fail('active badge in header');
// 部署行缩进
if (!html.includes('pool-dep')) fail('indented deployment rows');
// 黑框分隔已删（池头行就是分隔）
if (html.includes('pool-start')) fail('old border separator removed');

// 开关关的池无徽标
pool.renderModelsTable([models[3]], {});  // qwen3 off
if (el.innerHTML.includes('models.rollback.no_target')) fail('off pool → no badge');
// 开关开的池有徽标
pool.renderModelsTable([models[0], models[1], models[2]], {});
if (!el.innerHTML.includes('models.rollback.active')) fail('on pool → badge');

// 主开关三态：混合（glm 开、qwen3 关）→ indeterminate
pool.renderModelsTable(models, {});
if (el.indeterminate !== true || el.checked !== false) fail('mixed state → indeterminate');
// 全开 → checked
pool.renderModelsTable([models[0], models[1], models[2]], {});
if (el.checked !== true || el.indeterminate !== false) fail('all on → checked');
// 空表 → disabled
pool.renderModelsTable([], {});
if (el.disabled !== true) fail('empty table → master disabled');

// 价格列头不再右对齐
if (html.includes('<th class="num">models.col.cost</th>')) fail('cost header must not carry .num');

// ── 2. Config 卡片已移除（回落配置全部在模型部署页）───────────────
if (src.includes('renderCardRollback')) fail('renderCardRollback should be removed from app.js');

// ── 3. Rollback 统计树：错误码子行 × 全部 9 个标题 ────────────────
// fixture 是 boom-core/examples/dump_snapshot.rs 的真实输出（后端
// error_codes key 形态的唯一真相源），勿手改 —— 重新生成：
//   cargo run -p boom-core --example dump_snapshot
const rb = loadBlock('  function renderRollbackStats(data) {', '  // ── KVC Hit Calibration',
  'renderRollbackStats');
const rbSnapshot = {
  identify_total: 6, identify_miss: 1, pool_inert: 1, no_target: 1, fallback_selected: 3,
  retry_success: 1, retry_error: 1, guard_error: 1, no_fallback_return: 1, affinity_direct: 1,
  models: {
    'glm-5.3': {
      identify_miss: 1, pool_inert: 1, no_target: 1, fallback_selected: 3, retry_success: 1,
      retry_error: 1, guard_error: 1, no_fallback_return: 1, affinity_direct: 1,
      error_codes: {
        'guard_error:flow_control_timeout': 1,
        'identify_miss:WeirdModelError': 1,
        'no_fallback_return:InputTokenExceeded': 1,
        'no_target:InputTokenExceeded': 1,
        'pool_inert:InputTokenExceeded': 1,
        'retry_error:internal HTTP(500)': 1,
        'retry_success:InputTokenExceeded': 1,
      },
      avg_retry_ms: 42,
    },
  },
};
rb.renderRollbackStats(rbSnapshot);
const rbHtml = el.innerHTML;
// 9 个 treeNode 按渲染顺序切分
const nodes = rbHtml.split('<div class="rb-tree-node">').slice(1);
if (nodes.length !== 9) fail(`9 tree nodes expected, got ${nodes.length}`);
// (标题, 该节点的错误码子行期望; null = 成功指标，不应有子行)
// (标题, 该节点的错误码子行期望; null = 无码指标[已选目标/亲和直达])
// 键名=快照字段名,与后端 error_codes 前缀构造性一致,无别名层。
// 重试成功节点展示"修复了什么错误"(原始上下文错误的码)+ 个数。
const rbExpect = [
  ['debug.rollback.miss',        'WeirdModelError'],
  ['debug.rollback.inert',       'InputTokenExceeded'],
  ['debug.rollback.no_target',   'InputTokenExceeded'],
  ['debug.rollback.selected',    null],
  ['debug.rollback.success',     'InputTokenExceeded'],
  ['debug.rollback.retry_error', 'internal HTTP(500)'],
  ['debug.rollback.guard_error', 'flow_control_timeout'],
  ['debug.rollback.no_fallback', 'InputTokenExceeded'],
  ['debug.rollback.affinity_direct', null],
];
nodes.forEach((seg, i) => {
  const [label, code] = rbExpect[i];
  if (!seg.includes(label)) fail(`node ${i}: expected label ${label}`);
  if (!seg.includes('glm-5.3')) fail(`node ${i} (${label}): model row missing`);
  if (!seg.includes('▸')) fail(`node ${i} (${label}): expandable arrow missing`);
  if (code !== null) {
    if (!seg.includes('rb-tree-sub')) fail(`node ${i} (${label}): error-code sub-row missing`);
    if (!seg.includes(code)) fail(`node ${i} (${label}): expected code ${code}`);
  } else if (seg.includes('rb-tree-sub')) {
    fail(`node ${i} (${label}): success metric must not have error-code sub-rows`);
  }
});
// 全量数据下不允许出现占位行
if (rbHtml.includes('no per-model data')) fail('placeholder shown despite per-model data');
// 两条不变量均成立（fixture 满足 identify_total = miss+inert+no_target+selected 等）
if (!rbHtml.includes('✓')) fail('invariant checkmarks missing');

console.log('SMOKE OK: pool header rows with switches, indented dep rows, no rollback column');
console.log('SMOKE OK: rollback tree — codes under all 6 error headers + retry-success fixed-error sub-rows, alias-free keys');

// ── 4. KVC DFX 表:Detail 列 + 错误详情链接(与主日志页同款) ─────
// 提取块引用闭包变量 debugEnabled / promptLogEnabled → 以参数注入。
const fmtPrefixHit = (v) => v.toFixed(1) + '%';
const formatNumber = (v) => String(v);
const formatTimestamp = (v) => String(v);
function loadKvcBlock(debugEnabled, promptLogEnabled) {
  return new Function('t', 'esc', 'formatTimestamp', 'formatNumber', 'fmtPrefixHit',
    'debugEnabled', 'promptLogEnabled',
    src.slice(src.indexOf('  function kvcDfxShellHtml() {'), src.indexOf('  function setupKvcFilters() {')) +
    '\nreturn { kvcDfxShellHtml, renderKvcDfxRow };'
  )(t, esc, formatTimestamp, formatNumber, fmtPrefixHit, debugEnabled, promptLogEnabled);
}
// 表头:16 列,Detail 在末尾;筛选行 16 格,10 个输入框不变
const kvcShell = loadKvcBlock(false, false).kvcDfxShellHtml();
if (!kvcShell.includes('<th>Detail</th>')) fail('KVC DFX header lacks Detail column');
const shellCells = (kvcShell.match(/<td[>\s]/g) || []).length;
if (shellCells !== 16) fail(`filter row must have 16 cells, got ${shellCells}`);
if ((kvcShell.match(/col-filter/g) || []).length !== 9) fail('filter input count changed');
// debug 开 + prompt log 开:错误类型可点 → _showDebugError;Detail 有 View 按钮
const kvcOn = loadKvcBlock(true, true);
const errRow = kvcOn.renderKvcDfxRow({
  request_id: 'rid-1', key_hash: 'kh', team_alias: 'team-a', model: 'm1:d1', api_path: '/v1/chat/completions',
  status_code: 502, error_type: 'upstream_error', error_message: 'boom', is_stream: true,
  input_tokens: 10, output_tokens: 2, cached_tokens: 4, kv_hit_blocks: 1, kv_input_blocks: 2,
  trie_blocks: 3, trie_max_blocks: 100, request_tokens: 100, duration_ms: 5, ttft_ms: 1, policy: 'rr+rollback',
});
if (!errRow.includes("window._showDebugError('rid-1')")) fail('error cell must link _showDebugError when debug on');
if (!errRow.includes("window._viewPromptLog('rid-1'")) fail('detail cell must offer prompt-log view');
if (!errRow.includes('action.view')) fail('detail cell button label');
// 两个开关都关:无链接、Detail 为 "-"
const errRowOff = loadKvcBlock(false, false).renderKvcDfxRow({
  request_id: 'rid-2', error_type: 'upstream_error', error_message: 'x', status_code: 500, model: 'm', api_path: '/p',
});
if (errRowOff.includes('_showDebugError')) fail('no debug link when debug off');
if (errRowOff.includes('_viewPromptLog')) fail('no view button when prompt log off');
// 回退 400 透传行(error_type=context_length_exceeded)同样可点——
// 后端 debug store 已把该类型纳入记录集
const ctxRow = kvcOn.renderKvcDfxRow({
  request_id: 'rid-ctx', key_hash: 'kh', model: 'm2:d1', api_path: '/v1/chat/completions',
  status_code: 400, error_type: 'context_length_exceeded', error_message: 'InputTokenExceeded', policy: 'rr',
});
if (!ctxRow.includes("window._showDebugError('rid-ctx')")) fail('context-length rows must link the debug viewer');
// 成功行:无错误 → Error 列 "-",Detail 仍有按钮
const okRow = kvcOn.renderKvcDfxRow({
  request_id: 'rid-3', key_hash: 'kh', model: 'm1:d2', api_path: '/v1/chat/completions',
  status_code: 200, is_stream: false, policy: 'rr',
});
if (!okRow.includes('>-</td>')) fail('success row renders');
if (!okRow.includes("window._viewPromptLog('rid-3'")) fail('success row detail cell present');
// 列数:每行 16 个 <td>
for (const row of [errRow, errRowOff, okRow]) {
  const tds = (row.match(/<td/g) || []).length;
  if (tds !== 16) fail(`row must render 16 cells, got ${tds}`);
}

console.log('SMOKE OK: KVC DFX detail column + debug-error link mirror the logs page');
