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
  return new Function('t', 'esc', 'tip', 'renderDeployCell', 'visibilityBadgeFor', 'document',
    block + `\nreturn { ${returns} };`
  )(t, esc, tip, renderDeployCell, visibilityBadgeFor, document);
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

// ── 1. rollback 列（模型列后，每池一个 rowspan 勾选框居中）────────
pool.renderModelsTable(models, {});
const html = el.innerHTML;
// 列序：模型列在 rollback 列之前
const hdrModel = html.indexOf('models.col.model');
const hdrRollback = html.indexOf('models.col.rollback');
if (!(hdrModel >= 0 && hdrRollback > hdrModel)) fail('rollback column must come AFTER the model column');
// 每池一个勾选框（rowspan 居中），4 行部署只有 2 个
const cbs = html.match(/_togglePoolRollback\('([^']+)', this\.checked\)/g) || [];
if (cbs.length !== 2) fail(`one checkbox per pool, got ${cbs.length}`);
if (html.includes('_toggleRowRollback')) fail('per-row toggle should be removed');
// 每行自己的 rollback td（跟随行底色）——无 rowspan、无 span 类
if (html.includes('rowspan=')) fail('rowspan removed: per-row cells keep row stripes');
if (html.includes('cell-rollback-span')) fail('span class removed');
if ((html.match(/class="cell-rollback"/g) || []).length !== 4) fail('every row keeps its own rollback td');
// 勾选框在池的中间行：glm 池（3 行，奇数）→ id=2 那行，input 在 badge 前
const trs = html.split('<tr');
const glmCbRow = trs.find((x) => x.includes("_togglePoolRollback('glm-5.3'"));
if (!glmCbRow || !glmCbRow.includes("_editModel('2'")) fail('3-row pool: checkbox on the MIDDLE row (id 2)');
if (glmCbRow && !(glmCbRow.indexOf('<input type="checkbox"') < glmCbRow.indexOf('badge'))) fail('odd pool: input before badge');
const qwCbRow = trs.find((x) => x.includes("_togglePoolRollback('qwen3'"));
if (!qwCbRow || !qwCbRow.includes("_editModel('4'")) fail('1-row pool: checkbox on its only row');

// 偶数池（2 行）：勾选框底边对齐首行底边（cell-rollback-bottom），badge 在 input 上方
pool.renderModelsTable([models[0], models[1]], {});
const evenHtml = el.innerHTML;
const evenRow = evenHtml.split('<tr').find((x) => x.includes("_togglePoolRollback('glm-5.3'"));
if (!evenRow) fail('2-row pool: checkbox present');
if (!evenRow.includes('cell-rollback-bottom')) fail('even pool: bottom-aligned cell class');
if (!(evenRow.indexOf('<input type="checkbox"') < evenRow.indexOf('badge'))) fail('uniform order: input before badge (even pool)');
// glm 池全开 → 勾选；徽标在 rowspan 格内
if (!html.includes("checked onchange=\"window._togglePoolRollback('glm-5.3'")) fail('glm pool (all on) checked');
if (!html.includes('models.rollback.active')) fail('glm-5.3 should be active');
if (html.includes('models.rollback.no_target')) fail('qwen3 off → no badge');
// 黑框分隔：非首池首行
if ((html.match(/pool-start/g) || []).length !== 1) fail('only the 2nd pool gets the separator border');

// 开启 + 混配 → mixed（tooltip）
pool.renderModelsTable([models[0], { ...models[3], rollback_enabled: true }], {});
if (!el.innerHTML.includes('models.rollback.mixed')) fail('on + unconfigured → mixed');
if (!el.innerHTML.includes('tip.badge.rb_mixed')) fail('mixed badge carries tooltip');

// 开启 + 全配 + 单层 → no_target（tooltip）
pool.renderModelsTable([models[0], models[1]], {});
if (!el.innerHTML.includes('models.rollback.no_target')) fail('on + full + single tier → no_target');
if (!el.innerHTML.includes('tip.badge.rb_no_target')) fail('no_target badge carries tooltip');

// 价格列头不再右对齐
if (html.includes('<th class="num">models.col.cost</th>')) fail('cost header must not carry .num');

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

console.log('SMOKE OK: per-row cells, middle-row checkbox (bottom-aligned on even pools), badges, master toggle');
