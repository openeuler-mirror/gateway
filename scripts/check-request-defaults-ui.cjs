// Real-browser check against a local development console. Creates and removes
// one temporary deployment. Install playwright-core outside the repository.
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || "playwright-core");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

(async () => {
  const base = process.env.GATEWAY_URL || "http://127.0.0.1:4000";
  const browser = await chromium.launch({ channel: "msedge", headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1100 } });
  const page = await context.newPage();
  page.on("pageerror", error => console.error("Browser error:", error.message));
  let expectInvalid = false;
  page.on("dialog", dialog => {
    if (dialog.type() === "confirm") {
      dialog.accept();
      return;
    }
    if (!expectInvalid) {
      console.error("Unexpected browser dialog:", dialog.message());
      dialog.dismiss();
    }
  });
  const checks = [];
  let id;
  const api = async (suffix, method = "GET", data) => {
    const response = await context.request.fetch(base + "/dashboard/api" + suffix, { method, data });
    assert.equal(response.status(), 200);
    const body = await response.json();
    assert.ok(!body.error, JSON.stringify(body));
    return body;
  };
  const passed = (name) => { checks.push({ name, passed: true }); console.log("PASS " + name); };
  const output = path.join(__dirname, "../docs/analysis/20261026");
  try {
    await api("/auth/login", "POST", { user_id: "admin", api_key: process.env.MASTER_KEY || "sk-local-dev-master" });
    const created = await api("/admin/models", "POST", {
      model_name: "ui-defaults-probe-" + Date.now(), litellm_model: "openai/mock-gpt-4o",
      api_base: "http://127.0.0.1:9000/v1", request_defaults: { reasoning_effort: "medium" },
    });
    id = created.id;
    await page.goto(base + "/dashboard");
    await page.waitForFunction(() => typeof window._editModel === "function");
    const open = async () => {
      await page.evaluate((value) => window._editModel(value), id);
      await page.locator("#m-model-request-defaults").waitFor({ state: "visible" });
    };
    const save = async (text) => {
      await page.locator("#m-model-request-defaults").fill(text);
      const response = page.waitForResponse(r => r.request().method() === "PUT" && r.url().endsWith("/admin/models/" + id));
      await page.locator("#m-model-submit").click();
      assert.ok(!(await (await response).json()).error);
      await page.locator("#m-model-request-defaults").waitFor({ state: "hidden" });
    };
    await open();
    assert.deepEqual(JSON.parse(await page.locator("#m-model-request-defaults").inputValue()), { reasoning_effort: "medium" });
    passed("edit form displays stored defaults");
    await save('{"reasoning_effort":"high"}');
    await open();
    assert.deepEqual(JSON.parse(await page.locator("#m-model-request-defaults").inputValue()), { reasoning_effort: "high" });
    passed("browser submit and reopen retain changed defaults");

    for (const invalid of ["{broken", "[]"]) {
      expectInvalid = true;
      await page.locator("#m-model-request-defaults").fill(invalid);
      const dialog = page.waitForEvent("dialog");
      const click = page.locator("#m-model-submit").click();
      const alert = await dialog;
      assert.equal(alert.type(), "alert");
      await alert.accept();
      await click;
      expectInvalid = false;
      const row = (await api("/admin/models")).models.find(r => r.id === id);
      assert.deepEqual(row.request_defaults, { reasoning_effort: "high" });
      passed("invalid input rejected without changing saved defaults: " + invalid);
    }

    await save('{"chat_template_kwargs":{"enable_thinking":true}}');
    await open();
    assert.deepEqual(JSON.parse(await page.locator("#m-model-request-defaults").inputValue()), { chat_template_kwargs: { enable_thinking: true } });
    fs.mkdirSync(output, { recursive: true });
    await page.screenshot({ path: path.join(output, "request-defaults-console.png"), fullPage: true });
    passed("nested Qwen parameters render and persist");
    await save("");
    await open();
    assert.equal(await page.locator("#m-model-request-defaults").inputValue(), "");
    assert.deepEqual((await api("/admin/models")).models.find(r => r.id === id).request_defaults, {});
    passed("blank input clears defaults");
    fs.writeFileSync(path.join(output, "request-defaults-ui.json"), JSON.stringify({ passed: checks.length, checks }, null, 2) + "\n");
  } finally {
    if (id) await api("/admin/models/" + id, "DELETE");
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
