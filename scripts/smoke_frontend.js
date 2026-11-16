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
  return new Function('t', 'esc', 'tip', 'renderDeployCell', 'visibilityBadgeFor', 'vendorOf', 'document', 'window',
    block + `\nreturn { ${returns} };`
  )(t, esc, tip, renderDeployCell, visibilityBadgeFor, vendorOf, document, globalThis);
}

const pool = loadBlock('  function renderModelsTable', '  async function showNewModelModal',
  'renderModelsTable');


// ── fixtures（API 行不再含 rollback_enabled——声明是唯一参与信号）──
const models = [
  { id: '1', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: true, max_context_tokens: 204800, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '2', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: true, max_context_tokens: 204800, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '3', model_name: 'glm-5.3', litellm_model: 'openai/glm-5.3', enabled: false, auto_disabled: true, max_context_tokens: 1048576, quota_count_ratio: 1, timeout: 1200, source: 'db' },
  { id: '4', model_name: 'qwen3', litellm_model: 'openai/qwen3', enabled: true, quota_count_ratio: 1, timeout: 1200, source: 'db' }, // 全未声明
];

// ── 1. rowspan 分组表(在途请求风格)+ 静态状态行 ────────────────
pool.renderModelsTable(models, {});
const html = el.innerHTML;
// 开关全删:无 rb-switch/checked/任何 toggle 接线
if (html.includes('rb-switch')) fail('pool toggle must be removed');
if (html.includes('checkbox')) fail('no checkbox in the models table');
if (html.includes('_togglePoolRollback')) fail('toggle handler must be removed');
if (html.includes('pool-header')) fail('pool header rows replaced by rowspan blocks');
// 模型块:2 池 → 2 个 model-block;glm rowspan=3+1告警行=4,qwen rowspan=1
const blocks = html.match(/class="model-block[^"]*"/g) || [];
if (blocks.length !== 2) fail(`2 pools → 2 model-block cells, got ${blocks.length}`);
if (!html.includes('rowspan="4"')) fail('glm block spans 3 dep rows + 1 warning row');
if (!html.includes('rowspan="1"')) fail('qwen block spans its single row');
// 状态行已移至 Debug → Rollback(模型页不再渲染任何回退状态键)
if (html.includes('state_active') || html.includes('state_unconfigured')
  || html.includes('rollbackPoolState') || html.includes('rb-state')) {
  fail('rollback state line must live in the Debug pane, not the models table');
}
// 模型格内容存在(名称+计数),无状态行
if (!html.includes('model-name')) fail('model name span present');
if (!html.includes('model-count')) fail('instance count present');
// 斑马按块:qwen(第二块)行带 dep-alt,glm 行不带
const bRows = html.match(/class="pool-dep grp-b/g) || [];
if (bRows.length !== 1) fail(`zebra by GROUP: only qwen rows carry grp-b, got ${bRows.length}`);
const aRows = html.match(/class="pool-dep grp-a/g) || [];
if (aRows.length !== 3) fail(`first group rows carry grp-a, got ${aRows.length}`);
// 告警行跟随块色且 colspan=11
if (!html.includes('colspan="11"')) fail('warning row colspan follows 11 columns');

// 空表不崩
pool.renderModelsTable([], {});
if (!el.innerHTML.includes('models.empty')) fail('empty state renders');

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
  "identify_total": 6,
  "identify_miss": 1,
  "pool_inert": 1,
  "no_target": 1,
  "fallback_selected": 3,
  "retry_success": 1,
  "retry_error": 1,
  "guard_error": 1,
  "no_fallback_return": 1,
  "affinity_direct": 1,
  "convergence_windows": [
    {
      "hour": 498564,
      "direct": 1,
      "retry": 1
    }
  ],
  "models": {
    "glm-5.3": {
      "identify_miss": 1,
      "pool_inert": 1,
      "no_target": 1,
      "fallback_selected": 3,
      "retry_success": 1,
      "retry_error": 1,
      "guard_error": 1,
      "no_fallback_return": 1,
      "affinity_direct": 1,
      "error_codes": {
        "guard_error:flow_control_timeout": 1,
        "identify_miss:WeirdModelError": 1,
        "no_fallback_return:InputTokenExceeded": 1,
        "no_target:InputTokenExceeded": 1,
        "pool_inert:InputTokenExceeded": 1,
        "retry_error:internal HTTP(500)": 1,
        "retry_success:InputTokenExceeded": 1
      },
      "deploys": {
        "fallback_selected:|d-small→d-bad": 1,
        "fallback_selected:|d-small→d-big": 1,
        "fallback_selected:|d-small→d-queue": 1,
        "guard_error:flow_control_timeout|d-small→d-queue": 1,
        "identify_miss:WeirdModelError|d-small": 1,
        "no_fallback_return:InputTokenExceeded|d-small": 1,
        "no_target:InputTokenExceeded|d-small": 1,
        "pool_inert:InputTokenExceeded|d-small": 1,
        "retry_error:internal HTTP(500)|d-small→d-bad": 1,
        "retry_success:InputTokenExceeded|d-small→d-big": 1
      },
      "avg_retry_ms": 42
    }
  },
  "pools": [
    { "model": "glm-5.3", "state": "active", "deployments": [
      { "deployment_id": "d-200k-a", "max_context_tokens": 204800, "enabled": true },
      { "deployment_id": "d-200k-b", "max_context_tokens": 204800, "enabled": true },
      { "deployment_id": "d-1m", "max_context_tokens": 1048576, "enabled": true }
    ] },
    { "model": "qwen3.8", "state": "rescue", "deployments": [
      { "deployment_id": "d-200k", "max_context_tokens": 200000, "enabled": true },
      { "deployment_id": "d-undecl", "max_context_tokens": null, "enabled": false }
    ] }
  ]
};
rb.renderRollbackStats(rbSnapshot);
const rbHtml = el.innerHTML;
// 9 个统计 treeNode 按渲染顺序切分;池状态节(rb-tree 同款结构)在
// 其后渲染,不在计数内。
const statsHtml = rbHtml.split('debug.rollback.pools_title')[0];
const nodes = statsHtml.split('<div class="rb-tree-node">').slice(1);
if (nodes.length !== 9) fail(`9 stats tree nodes expected, got ${nodes.length}`);
// (标题, 该节点的错误码子行期望; null = 成功指标，不应有子行)
// (标题, 该节点的子行期望; null = 无子行[仅亲和直达])
// 键名=快照字段名,与后端 error_codes/deploys 前缀构造性一致,无别名层。
// 重试节点的子行 = 错误码 + from→to 边;无重试节点的子行 = 错误码 + 失败部署。
const rbExpect = [
  ['debug.rollback.miss',        'WeirdModelError'],
  ['debug.rollback.inert',       'InputTokenExceeded'],
  ['debug.rollback.no_target',   'InputTokenExceeded'],
  ['debug.rollback.selected',    'd-small→d-big'],
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

console.log('SMOKE OK: rowspan grouped table (in-flight style), state line moved to Debug pane, toggle fully removed');
// 事件表/迁移矩阵已撤销(信息并入各标题的部署子行)——断言其绝迹
if (rbHtml.includes('events_title') || rbHtml.includes('matrix_title')) fail('events/matrix sections must be gone');
if (src.includes('_rbEvData') || src.includes('_rbEventsApply')) fail('events handlers must be gone');

// 维测渲染:窗口收敛率 / 池状态 / 自检表单
if (!rbHtml.includes('debug.rollback.conv_windows')) fail('convergence windows section missing');
if (!rbHtml.includes('debug.rollback.selftest_title')) fail('self-test form missing');
if (!src.includes('window._rbSelfTest')) fail('self-test handler missing');

// 部署维度子行(事件表撤销后信息的去向):
// 无重试节记失败部署(mono d-xxx);重试节记 from→to 边
const nodeWith = (label) => nodes.find((n) => n.includes(label)) || '';
if (!nodeWith('debug.rollback.miss').includes('>d-small<')) fail('miss node: failed-deployment sub-row missing');
if (!nodeWith('debug.rollback.inert').includes('>d-small<')) fail('inert node: failed-deployment sub-row missing');
if (!nodeWith('debug.rollback.success').includes('d-small→d-big')) fail('success node: from→to edge sub-row missing');
if (!nodeWith('debug.rollback.retry_error').includes('d-small→d-bad')) fail('retry_error node: from→to edge sub-row missing');
if (!nodeWith('debug.rollback.guard_error').includes('d-small→d-queue')) fail('guard_error node: from→to edge sub-row missing');
if (!nodeWith('debug.rollback.no_fallback').includes('>d-small<')) fail('no_fallback node: failed-deployment sub-row missing');
// 嵌套层级:有码节的边带 rb-tree-edge(归属该错误码,深一层);
// 已选目标(无码决策)的边留在码层级(无 rb-tree-edge)
if (!/rb-tree-edge[^>]*>.*d-small→d-big/.test(nodeWith('debug.rollback.success'))) fail('success edge must nest under its code (rb-tree-edge)');
if (nodeWith('debug.rollback.selected').includes('rb-tree-edge')) fail('selected edges sit at code level (no rb-tree-edge)');
console.log('SMOKE OK: rollback tree — code sub-rows + deploy sub-rows (from / from→to) under all headers, alias-free keys');
// 池状态节(可折叠树):池头带 总/已配置/未配置 计数,子行按部署;
// 数值为紧凑格式(200K/1M),未配置显示 —,停用带标记
if (!rbHtml.includes('debug.rollback.pools_title')) fail('pool states section missing');
if (!rbHtml.includes('debug.rollback.state_active')) fail('active state rendered');
if (!rbHtml.includes('debug.rollback.state_rescue')) fail('rescue state rendered');
if (!rbHtml.includes('debug.rollback.pool_counts')) fail('pool counts line missing');
// 池头计数行存在(桩返回 key 原文;真实插值文案由 e2e 用真 i18n 验证)
if (!rbHtml.includes('debug.rollback.pool_counts')) fail('pool counts line rendered');
// 200K 紧凑值 + 原始行
if (!rbHtml.includes('205K') && !rbHtml.includes('200K')) fail('compact 200K value rendered');
if (!rbHtml.includes('1M') && !rbHtml.includes('1049K')) fail('compact 1M value rendered');
if (!rbHtml.includes('d-undecl')) fail('per-deployment rows rendered');
if (!rbHtml.includes('rb-field-name')) fail('field-name label missing on instance rows');
if (!rbHtml.includes('max_context_tokens</span> 200K') && !rbHtml.includes('max_context_tokens</span> 205K')) fail('field name prefixes the value');
if (!rbHtml.includes('—')) fail('unconfigured dash rendered');
if (!rbHtml.includes('debug.rollback.disabled')) fail('disabled marker rendered');
// 可折叠结构:池状态节内 rb-tree-node(树已在其它节存在,校验本节 head 含 onclick)
const poolsSeg = rbHtml.split('debug.rollback.pools_title')[1] || '';
if (!poolsSeg.includes('rb-tree-head')) fail('collapsible pool head missing');
if (!poolsSeg.includes("toggle('rb-open')") && !poolsSeg.includes('toggle(\'rb-open\')')) fail('collapse toggle wired');
console.log('SMOKE OK: diagnostics — pool states (collapsible), hourly convergence, deploy sub-rows, self-test form');

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
