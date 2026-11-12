// Run with: node --test crates/boom-dashboard/tests/frontend_keys.test.cjs
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

// Execute the complete SPA and its public pagination handler. Only the DOM
// surface and HTTP transport are replaced; api() and both renderers are real.
function dashboard() {
  const elements = new Map();
  for (const id of ["modal-overlay", "keys-table-wrap", "keys-pagination", "keys-plan-filter"]) {
    const listeners = new Map();
    elements.set(id, {
      innerHTML: "", value: "",
      addEventListener(type, listener) {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type).push(listener);
      },
      dispatch(type) { for (const listener of listeners.get(type) || []) listener(); },
    });
  }
  const requests = [];
  const window = {};
  const context = vm.createContext({
    window,
    document: {
      addEventListener() {}, // Do not start login, routing, or polling.
      getElementById: (id) => elements.get(id) || null,
      createElement: () => ({
        set textContent(value) {
          this.innerHTML = String(value ?? "").replace(/&/g, "&amp;")
            .replace(/</g, "&lt;").replace(/>/g, "&gt;");
        },
      }),
    },
    t: (key, args) => key + (args ? JSON.stringify(args) : ""),
    console: { error() {}, warn() {} },
    fetch(url) {
      return new Promise((resolve, reject) => requests.push({
        url,
        reject,
        reply(data) { resolve({ ok: true, status: 200, json: async () => data }); },
      }));
    },
  });
  const source = readFileSync(join(__dirname, "../src/frontend/app.js"), "utf8");
  // Expose the real route-entry sequence and mutation cache invalidation only
  // in the VM. Production code does not gain test exports.
  assert.match(source, /\}\)\(\);\s*$/);
  vm.runInContext(source.replace(/\}\)\(\);\s*$/, `
    window.keysTest = {
      enter() { setupKeysSearch(); return loadKeys(); },
      invalidateCaches,
    };
  })();`), context);
  return {
    window, requests, planFilter: elements.get("keys-plan-filter"),
    table: () => elements.get("keys-table-wrap").innerHTML,
    pagination: () => elements.get("keys-pagination").innerHTML,
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));
const keyPage = (page) => ({
  page, per_page: 50, total: 150,
  keys: [{ token_hash: `hash-${page}`, token_prefix: `key-${page}`, key_alias: `alias-${page}` }],
});

async function startPage(app, page, excluded = []) {
  const start = app.requests.length;
  const done = app.window._loadKeysPage(page);
  assert.equal(app.requests[start].url, "/dashboard/api/admin/prompt-log/status");
  app.requests[start].reply({ excluded_keys: excluded });
  await flush();
  const request = app.requests[start + 1];
  assert.equal(request.url, `/dashboard/api/admin/keys?page=${page}&per_page=50`);
  return { done, request };
}

test("late success cannot replace the latest key page or pagination", async () => {
  const app = dashboard();
  const older = await startPage(app, 1);
  const newer = await startPage(app, 2);
  newer.request.reply(keyPage(2));
  await newer.done;
  const table = app.table();
  const pagination = app.pagination();
  assert.match(table, /alias-2/);
  assert.match(pagination, /"page":2/);
  older.request.reply(keyPage(1));
  await older.done;
  assert.equal(app.table(), table);
  assert.equal(app.pagination(), pagination);
});

test("late error cannot replace a successfully loaded newer page", async () => {
  const app = dashboard();
  const older = await startPage(app, 1);
  const newer = await startPage(app, 2);
  newer.request.reply(keyPage(2));
  await newer.done;
  const table = app.table();
  older.request.reject(new Error("old request failed"));
  await older.done;
  assert.equal(app.table(), table);
});

for (const fail of [false, true]) {
  test(`superseded prompt-log ${fail ? "failure" : "success"} cannot change state or start another key request`, async () => {
    const app = dashboard();
    const older = app.window._loadKeysPage(1);
    const oldStatus = app.requests[0];
    const newer = await startPage(app, 2, ["hash-2"]);
    newer.request.reply(keyPage(2));
    await newer.done;
    const count = app.requests.length;
    if (fail) oldStatus.reject(new Error("old status failed"));
    else oldStatus.reply({ excluded_keys: ["hash-1"] });
    await flush();
    // Settle any erroneous extra request so the baseline test leaves no work pending.
    for (const request of app.requests.slice(count)) request.reply(keyPage(2));
    await older;
    assert.deepEqual(Array.from(app.window._promptLogExcludedKeys), ["hash-2"]);
    assert.equal(app.requests.length, count);
  });
}

test("prompt-log failure still allows the current key page to load", async () => {
  const app = dashboard();
  const done = app.window._loadKeysPage(3);
  app.requests[0].reject(new Error("status unavailable"));
  await flush();
  assert.equal(app.requests[1].url, "/dashboard/api/admin/keys?page=3&per_page=50");
  app.requests[1].reply(keyPage(3));
  await done;
  assert.match(app.table(), /alias-3/);
  assert.deepEqual(Array.from(app.window._promptLogExcludedKeys), []);
});

test("the current key request failure remains visible", async () => {
  const app = dashboard();
  const current = await startPage(app, 1);
  current.request.reject(new Error("current request failed"));
  await current.done;
  assert.match(app.table(), /current request failed/);
});

for (const plansFirst of [true, false]) {
  test(`a removed plan reloads all keys when plan options arrive ${plansFirst ? "before" : "after"} the key request`, async () => {
    const app = dashboard();
    const initial = app.window.keysTest.enter();
    app.requests.find((r) => r.url === "/dashboard/api/admin/plans")
      .reply({ plans: [{ name: "retired-plan" }] });
    app.requests.find((r) => r.url === "/dashboard/api/admin/prompt-log/status")
      .reply({ excluded_keys: [] });
    await flush();
    app.requests.at(-1).reply(keyPage(1));
    await initial;

    // Select the plan through the real change handler, then leave the list
    // on page 3. A plan/model mutation invalidates the cached plan names.
    app.planFilter.value = "retired-plan";
    app.planFilter.dispatch("change");
    app.requests.at(-1).reply({ excluded_keys: [] });
    await flush();
    assert.match(app.requests.at(-1).url, /plan=retired-plan/);
    app.requests.at(-1).reply(keyPage(1));
    await flush();
    const page3 = app.window._loadKeysPage(3);
    app.requests.at(-1).reply({ excluded_keys: [] });
    await flush();
    app.requests.at(-1).reply(keyPage(3));
    await page3;
    app.window.keysTest.invalidateCaches();

    const start = app.requests.length;
    const oldLoad = app.window.keysTest.enter();
    const plans = app.requests[start];
    const status = app.requests[start + 1];
    let staleKeys;
    if (!plansFirst) {
      status.reply({ excluded_keys: ["old-hash"] });
      await flush();
      staleKeys = app.requests.at(-1);
      assert.match(staleKeys.url, /page=3.*plan=retired-plan/);
    }
    plans.reply({ plans: [] });
    await flush();
    assert.equal(app.planFilter.value, "");
    // A reset must start a new load and supersede the old query, regardless
    // of whether the old load is awaiting prompt-log status or key rows.
    const resetStatus = app.requests.at(-1);
    assert.notEqual(resetStatus, status);
    assert.equal(resetStatus.url, "/dashboard/api/admin/prompt-log/status");
    resetStatus.reply({ excluded_keys: ["new-hash"] });
    await flush();
    const resetKeys = app.requests.at(-1);
    assert.equal(resetKeys.url, "/dashboard/api/admin/keys?page=1&per_page=50");
    resetKeys.reply(keyPage(1));
    await flush();
    const table = app.table();
    assert.match(table, /alias-1/);
    assert.match(app.pagination(), /"page":1/);
    const count = app.requests.length;
    if (plansFirst) status.reply({ excluded_keys: ["old-hash"] });
    else staleKeys.reply(keyPage(3));
    await oldLoad;
    assert.equal(app.requests.length, count);
    assert.equal(app.table(), table);
    assert.deepEqual(Array.from(app.window._promptLogExcludedKeys), ["new-hash"]);
  });
}
