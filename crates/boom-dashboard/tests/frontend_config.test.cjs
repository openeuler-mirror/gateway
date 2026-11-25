// Run with: node --test crates/boom-dashboard/tests/frontend_config.test.cjs
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

function dashboard() {
  const elements = new Map();
  elements.set("modal-overlay", { addEventListener() {} });
  const window = {};
  const storage = { getItem() { return null; }, setItem() {}, removeItem() {} };
  const context = vm.createContext({
    window,
    document: {
      addEventListener() {},
      getElementById: (id) => elements.get(id) || null,
      createElement: () => ({
        set textContent(value) {
          this.innerHTML = String(value ?? "").replace(/&/g, "&amp;")
            .replace(/</g, "&lt;").replace(/>/g, "&gt;");
        },
      }),
    },
    localStorage: storage,
    sessionStorage: storage,
    t: (key) => key,
    console: { error() {}, warn() {} },
  });
  const source = readFileSync(join(__dirname, "../src/frontend/app.js"), "utf8");
  vm.runInContext(source.replace(/\}\)\(\);\s*$/, `
    window.configTest = {
      renderCardTrace,
      syncTraceFields: typeof syncTraceFields === "function" ? syncTraceFields : null,
    };
  })();`), context);
  return { window, elements };
}

function trace(overrides = {}) {
  return {
    enabled: true,
    capture_body: true,
    max_body_bytes: 131072,
    propagate_only: true,
    export_without_traceparent: false,
    metrics: { enabled: false },
    report_filter: {},
    otlp: {},
    ...overrides,
  };
}

test("trace options only show fields that apply to the selected switches", () => {
  const app = dashboard();
  const enabledWithoutBody = app.window.configTest.renderCardTrace(trace({ capture_body: false }));
  assert.match(enabledWithoutBody, /<div id="cfg-tr-capture-body-field">/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-max-body-field" class="field-full" hidden>/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-propagate-only-field" hidden>/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-export-without-traceparent-field">/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-metrics-field" class="field-full trace-metrics-row">/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-filter-section">/);
  assert.match(enabledWithoutBody, /<div id="cfg-tr-otlp-section">/);

  const disabledWithBody = app.window.configTest.renderCardTrace(trace({
    enabled: false,
    export_without_traceparent: true,
    metrics: { enabled: true },
  }));
  assert.match(disabledWithBody, /<div id="cfg-tr-capture-body-field" hidden>/);
  assert.match(disabledWithBody, /<div id="cfg-tr-max-body-field" class="field-full" hidden>/);
  assert.match(disabledWithBody, /<div id="cfg-tr-propagate-only-field">/);
  assert.match(disabledWithBody, /<div id="cfg-tr-export-without-traceparent-field" hidden>/);
  assert.match(disabledWithBody, /<div id="cfg-tr-metrics-field" class="field-full trace-metrics-row" hidden>/);
  assert.match(disabledWithBody, /<div id="cfg-tr-filter-section" hidden>/);
  assert.match(disabledWithBody, /<div id="cfg-tr-otlp-section" hidden>/);
  assert.doesNotMatch(disabledWithBody, /class="cfg-warn-note"/);
});

test("trace switch changes synchronize every conditional field and restore visible values", () => {
  const app = dashboard();
  for (const [id, checked] of [["cfg-tr-enabled", true], ["cfg-tr-capture-body", false]]) {
    app.elements.set(id, { checked });
  }
  for (const id of [
    "cfg-tr-capture-body-field",
    "cfg-tr-max-body-field",
    "cfg-tr-propagate-only-field",
    "cfg-tr-export-without-traceparent-field",
    "cfg-tr-metrics-field",
    "cfg-tr-filter-section",
    "cfg-tr-otlp-section",
  ]) {
    app.elements.set(id, { hidden: false });
  }
  assert.equal(typeof app.window.configTest.syncTraceFields, "function");

  app.window.configTest.syncTraceFields();
  assert.equal(app.elements.get("cfg-tr-capture-body-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-max-body-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-propagate-only-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-export-without-traceparent-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-metrics-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-filter-section").hidden, false);
  assert.equal(app.elements.get("cfg-tr-otlp-section").hidden, false);

  app.elements.get("cfg-tr-enabled").checked = false;
  app.elements.get("cfg-tr-capture-body").checked = true;
  app.window.configTest.syncTraceFields();
  assert.equal(app.elements.get("cfg-tr-capture-body-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-max-body-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-propagate-only-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-export-without-traceparent-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-metrics-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-filter-section").hidden, true);
  assert.equal(app.elements.get("cfg-tr-otlp-section").hidden, true);

  app.elements.get("cfg-tr-enabled").checked = true;
  app.window.configTest.syncTraceFields();
  assert.equal(app.elements.get("cfg-tr-capture-body-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-max-body-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-propagate-only-field").hidden, true);
  assert.equal(app.elements.get("cfg-tr-export-without-traceparent-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-metrics-field").hidden, false);
  assert.equal(app.elements.get("cfg-tr-filter-section").hidden, false);
  assert.equal(app.elements.get("cfg-tr-otlp-section").hidden, false);
});

test("trace checkboxes share a row while metrics stays separate", () => {
  const app = dashboard();
  const html = app.window.configTest.renderCardTrace(trace());
  assert.match(html, /<div class="field-full trace-checkbox-row">/);
  assert.match(html, /<div id="cfg-tr-metrics-field" class="field-full trace-metrics-row">/);
});
