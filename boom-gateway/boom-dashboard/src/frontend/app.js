// ── BooMGateway Dashboard SPA ────────────────────────────
(function () {
  "use strict";

  const API = "/dashboard/api";
  let currentUser = null;
  let usageRefreshTimer = null;

  // ── Tooltip helper ────────────────────────────────────
  // Usage: tip("description text") → returns HTML string with ? icon
  function tip(text) {
    const safe = esc(text).replace(/"/g, "&quot;");
    return `<span class="field-tip" data-tip="${safe}">?</span>`;
  }

  // ── Cached data for dropdowns ──────────────────────────
  // Populated lazily when modals need them.
  let cachedModelNames = null;
  let cachedPlanNames = null;

  async function getModelNames() {
    if (cachedModelNames) return cachedModelNames;
    try {
      const data = await api("/admin/models");
      cachedModelNames = (data.models || []).map((m) => m.model_name);
      // deduplicate
      cachedModelNames = [...new Set(cachedModelNames)];
    } catch { cachedModelNames = []; }
    return cachedModelNames;
  }

  async function getPlanNames() {
    if (cachedPlanNames) return cachedPlanNames;
    try {
      const data = await api("/admin/plans");
      cachedPlanNames = (data.plans || []).map((p) => p.name);
    } catch { cachedPlanNames = []; }
    return cachedPlanNames;
  }

  // Invalidate caches after mutations
  function invalidateCaches() { cachedModelNames = null; cachedPlanNames = null; }

  // ── Init ──────────────────────────────────────────────
  document.addEventListener("DOMContentLoaded", () => {
    setupLogin();
    setupLogout();
    setupAdminButtons();
    window.addEventListener("hashchange", () => { onRoute(); onUserRoute(); });
    checkSession();
  });

  // ── API helpers ───────────────────────────────────────
  async function api(path, opts = {}) {
    const res = await fetch(API + path, {
      headers: { "Content-Type": "application/json", ...opts.headers },
      ...opts,
    });
    if (res.status === 401) { showLogin(); throw new Error("unauthorized"); }
    if (res.status === 204) return null;
    const data = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(data.error || data.message || res.statusText);
    return data;
  }

  // ── Session ───────────────────────────────────────────
  async function checkSession() {
    try {
      const me = await api("/auth/me");
      currentUser = me;
      navigateToDashboard(me.role);
    } catch {
      showLogin();
    }
  }

  function showLogin() {
    currentUser = null;
    clearUsageRefresh();
    document.querySelectorAll(".page").forEach((p) => p.classList.remove("active"));
    document.getElementById("page-login").classList.add("active");
  }

  function navigateToDashboard(role) {
    document.querySelectorAll(".page").forEach((p) => p.classList.remove("active"));
    if (role === "admin") {
      document.getElementById("page-admin").classList.add("active");
      onRoute();
    } else {
      const titleEl = document.getElementById("user-sidebar-title");
      if (titleEl && currentUser) titleEl.textContent = currentUser.user_id || "Dashboard";
      document.getElementById("page-dashboard").classList.add("active");
      loadUserData();
      startUsageRefresh();
      onUserRoute();
    }
  }

  // ── Login ─────────────────────────────────────────────
  function setupLogin() {
    document.getElementById("login-form").addEventListener("submit", async (e) => {
      e.preventDefault();
      const errEl = document.getElementById("login-error");
      errEl.classList.add("hidden");
      const btn = document.getElementById("login-btn");
      btn.disabled = true;
      btn.textContent = "Logging in...";
      try {
        const userId = document.getElementById("user_id").value.trim();
        const res = await fetch(API + "/auth/login", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            user_id: userId || "",
            api_key: document.getElementById("api_key").value,
          }),
        });
        if (!res.ok) {
          const data = await res.json().catch(() => ({}));
          throw new Error(data.error || data.message || "Login failed");
        }
        const data = await res.json();
        currentUser = data;
        navigateToDashboard(data.role);
      } catch (err) {
        errEl.textContent = err.message;
        errEl.classList.remove("hidden");
      } finally {
        btn.disabled = false;
        btn.textContent = "Login";
      }
    });
  }

  // ── Logout ────────────────────────────────────────────
  function setupLogout() {
    document.getElementById("logout-btn").addEventListener("click", doLogout);
    document.getElementById("logout-btn-admin").addEventListener("click", doLogout);
  }

  async function doLogout() {
    await fetch(API + "/auth/logout", { method: "POST" }).catch(() => {});
    showLogin();
  }

  // ── Routing (admin) ───────────────────────────────────
  function onRoute() {
    const hash = location.hash || "#/admin/stats";
    document.querySelectorAll("#page-admin .nav-link").forEach((a) => {
      a.classList.toggle("active", a.getAttribute("href") === hash);
    });
    document.querySelectorAll("#page-admin .section").forEach((s) => {
      s.classList.toggle("active", s.id === sectionFromHash(hash));
    });
    const section = sectionFromHash(hash);
    if (section === "admin-stats") {
      loadStats();
      startInflightPoll();
    } else {
      stopInflightPoll();
    }
    if (section === "admin-models") loadModels();
    else if (section === "admin-aliases") loadAliases();
    else if (section === "admin-plans") loadPlans();
    else if (section === "admin-keys") { setupKeysSearch(); loadKeys(); }
    else if (section === "admin-assignments") loadAssignments();
    else if (section === "admin-teams") loadTeams();
    else if (section === "admin-logs") { setupLogsFilters(); loadLogs(); }
    else if (section === "admin-config") loadConfig();
  }

  function sectionFromHash(hash) {
    if (hash.includes("/admin/stats")) return "admin-stats";
    if (hash.includes("/admin/models")) return "admin-models";
    if (hash.includes("/admin/aliases")) return "admin-aliases";
    if (hash.includes("/admin/plans")) return "admin-plans";
    if (hash.includes("/admin/keys")) return "admin-keys";
    if (hash.includes("/admin/teams")) return "admin-teams";
    if (hash.includes("/admin/assignments")) return "admin-assignments";
    if (hash.includes("/admin/logs")) return "admin-logs";
    if (hash.includes("/admin/config")) return "admin-config";
    return "admin-models";
  }

  // ── Stats ─────────────────────────────────────────────
  async function loadStats() {
    try {
      const data = await api("/admin/stats/models");
      renderStatsTable(data.models || []);
    } catch (err) {
      console.error("loadStats error:", err);
    }
    loadInflight();
  }

  // ── In-Flight ─────────────────────────────────────────
  let inflightTimer = null;

  async function loadInflight() {
    try {
      const data = await api("/admin/stats/inflight");
      renderInflightTable(data);
    } catch (err) {
      console.error("loadInflight error:", err);
    }
  }

  function renderInflightTable(data) {
    const wrap = document.getElementById("inflight-table-wrap");
    var deployments = data.deployments || [];

    if (!deployments.length) {
      wrap.innerHTML = "<p>No in-flight requests.</p>";
      return;
    }

    wrap.innerHTML =
      '<table class="data-table"><thead><tr>' +
      "<th>Deployment</th><th>FC QUEUE</th><th>IN-MODEL REQS</th><th>IN-MODEL CONTEXT</th>" +
      "</tr></thead><tbody>" +
      deployments
        .map(function (d) {
          var reqsDisplay = d.in_reqs_max > 0 ? d.in_reqs + " / " + d.in_reqs_max : String(d.in_reqs);
          var ctxDisplay = d.in_context_max > 0 ? d.in_context.toLocaleString() + " / " + d.in_context_max.toLocaleString() : d.in_context.toLocaleString();

          // FC QUEUE tooltip — show queued key aliases (VIP first).
          var fcQueueHtml = String(d.fc_queue);
          if (d.fc_queue > 0 && d.queued_keys && d.queued_keys.length > 0) {
            var items = d.queued_keys.map(function (k) {
              var prefix = k.is_vip ? "[VIP] " : "";
              return prefix + esc(k.key_alias || "?");
            });
            fcQueueHtml = '<span class="cell-tip" data-tip="' + items.join("&#10;").replace(/"/g, "&quot;") + '">' + d.fc_queue + '</span>';
          }

          // IN-MODEL REQS tooltip — show per-key request counts.
          var reqsHtml = reqsDisplay;
          if (d.in_reqs > 0 && d.key_stats && d.key_stats.length > 0) {
            var reqItems = d.key_stats.map(function (k) {
              return esc(k.key_alias || "?") + ": " + k.request_count;
            });
            reqsHtml = '<span class="cell-tip" data-tip="' + reqItems.join("&#10;").replace(/"/g, "&quot;") + '">' + reqsDisplay + '</span>';
          }

          return (
            "<tr>" +
            "<td>" + esc(d.deployment_id ? d.model + ":" + d.deployment_id : d.model) + "</td>" +
            "<td>" + fcQueueHtml + "</td>" +
            "<td>" + reqsHtml + "</td>" +
            "<td>" + ctxDisplay + "</td>" +
            "</tr>"
          );
        })
        .join("") +
      "</tbody></table>";
  }

  function startInflightPoll() {
    stopInflightPoll();
    inflightTimer = setInterval(loadInflight, 3000);
  }

  function stopInflightPoll() {
    if (inflightTimer) {
      clearInterval(inflightTimer);
      inflightTimer = null;
    }
  }

  function renderStatsTable(models) {
    const wrap = document.getElementById("stats-table-wrap");
    if (!models.length) {
      wrap.innerHTML = "<p>No data yet.</p>";
      return;
    }
    wrap.innerHTML =
      '<table class="data-table"><thead><tr>' +
      "<th>Model</th><th>Requests</th><th>Success</th><th>Errors</th>" +
      "<th>Input Tokens</th><th>Output Tokens</th><th>Avg Duration (ms)</th>" +
      "<th>Last Request</th>" +
      "</tr></thead><tbody>" +
      models
        .map(function (m) {
          return (
            "<tr>" +
            "<td>" + esc(m.model) + "</td>" +
            "<td>" + m.total_requests + "</td>" +
            "<td>" + m.success_count + "</td>" +
            "<td>" + m.error_count + "</td>" +
            "<td>" + m.total_input_tokens.toLocaleString() + "</td>" +
            "<td>" + m.total_output_tokens.toLocaleString() + "</td>" +
            "<td>" + m.avg_duration_ms + "</td>" +
            "<td>" + (m.last_request_at || "-") + "</td>" +
            "</tr>"
          );
        })
        .join("") +
      "</tbody></table>";
  }

  // ── User Dashboard ────────────────────────────────────

  let userLogsPage = 1;

  function onUserRoute() {
    const hash = location.hash || "#/dashboard";
    document.querySelectorAll("#page-dashboard .nav-link").forEach((a) => {
      a.classList.toggle("active", a.getAttribute("href") === hash);
    });
    document.querySelectorAll("#page-dashboard .section").forEach((s) => {
      s.classList.toggle("active", s.id === userSectionFromHash(hash));
    });
    const section = userSectionFromHash(hash);
    if (section === "user-logs") loadUserLogs();
  }

  function userSectionFromHash(hash) {
    if (hash.includes("/dashboard/logs")) return "user-logs";
    return "user-overview";
  }

  async function loadUserLogs(page) {
    if (page !== undefined) userLogsPage = page;
    const wrap = document.getElementById("user-logs-table-wrap");
    if (!wrap) return;
    try {
      const data = await api(`/user/logs?page=${userLogsPage}&per_page=50`);
      renderUserLogsTable(data.logs || []);
      renderUserLogsPagination(data);
    } catch (err) {
      wrap.innerHTML = `<p class="error-msg">Failed to load logs: ${esc(err.message)}</p>`;
    }
  }

  function renderUserLogsTable(logs) {
    const wrap = document.getElementById("user-logs-table-wrap");
    if (logs.length === 0) {
      wrap.innerHTML = "<p>No request logs found.</p>";
      return;
    }
    wrap.innerHTML = `<table>
      <tr><th>Time</th><th>Model</th><th>Path</th><th>Status</th><th>Stream</th><th>Input</th><th>Output</th><th>Duration</th><th>Error</th></tr>
      ${logs.map((l) => `<tr>
        <td class="mono">${formatTimestamp(l.created_at)}</td>
        <td class="mono">${esc(l.model)}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? "Yes" : "No"}</td>
        <td>${l.input_tokens != null ? formatNumber(l.input_tokens) : "-"}</td>
        <td>${l.output_tokens != null ? formatNumber(l.output_tokens) : "-"}</td>
        <td>${l.duration_ms != null ? l.duration_ms + "ms" : "-"}</td>
        <td>${l.error_message ? '<span style="color:var(--danger)" title="' + esc(l.error_message) + '">' + esc((l.error_type || "").substring(0, 20)) + '</span>' : "-"}</td>
      </tr>`).join("")}
    </table>`;
  }

  function renderUserLogsPagination(data) {
    const el = document.getElementById("user-logs-pagination");
    if (!el) return;
    const pages = Math.ceil(data.total / data.per_page);
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._loadUserLogsPage(${data.page - 1})">&lt;</button>
      <span>Page ${data.page} of ${pages} (${data.total} logs)</span>
      <button ${data.page >= pages ? "disabled" : ""} onclick="window._loadUserLogsPage(${data.page + 1})">&gt;</button>
    `;
  }

  window._loadUserLogsPage = (p) => loadUserLogs(p);
  async function loadUserData() {
    try {
      const [plan, usage, keyInfo] = await Promise.all([
        api("/user/plan"),
        api("/user/usage"),
        api("/user/key-info"),
      ]);
      renderPlan(plan);
      renderUsage(usage);
      renderTokenInfo(keyInfo);
      renderKeyInfo(keyInfo);
    } catch (err) {
      console.error("Failed to load user data:", err);
    }
  }

  function renderPlan(plan) {
    const el = document.getElementById("plan-info");
    if (!plan.plan_name) {
      el.innerHTML = "<p>No plan assigned. Using default limits.</p>";
      return;
    }
    const limits = [];
    if (plan.concurrency_limit) limits.push(`Concurrency: ${plan.concurrency_limit}`);
    if (plan.rpm_limit) limits.push(`RPM: ${plan.rpm_limit}`);
    if (plan.window_limits && plan.window_limits.length > 0) {
      plan.window_limits.forEach(([l, w]) => limits.push(`${l} requests / ${formatDuration(w)}`));
    }
    el.innerHTML = `
      <p><strong>${esc(plan.plan_name)}</strong></p>
      <ul>${limits.map((l) => `<li>${esc(l)}</li>`).join("")}</ul>
    `;
  }

  function renderUsage(usage) {
    const el = document.getElementById("usage-info");
    const concLimit = usage.concurrency_limit || "-";
    let html = `<p>Current concurrency: <strong>${usage.concurrency}</strong> / ${concLimit}</p>`;
    if (usage.windows.length === 0) {
      html += "<p>No active rate limit windows.</p>";
    } else {
      html += '<table><tr><th>Plan</th><th>Window</th><th>Count</th><th>Progress</th></tr>';
      const planName = usage.plan_name || "-";
      usage.windows.forEach((w) => {
        const windowLabel = formatDuration(w.window_secs);
        const limit = w.limit;
        const countLabel = limit != null ? `${w.count} / ${limit}` : `${w.count}`;
        const pct = w.count > 0 && w.window_secs > 0 ? Math.min((w.elapsed_secs / w.window_secs) * 100, 100) : 0;
        const fillClass = limit != null && w.count >= limit ? "danger" : limit != null && w.count / limit >= 0.8 ? "warn" : "";
        const isRpm = w.window_secs === 60;
        const label = isRpm ? "rpm" : planName;
        let planCell = esc(label);
        if (limit != null && w.count >= limit) {
          const remaining = Math.max(0, w.window_secs - w.elapsed_secs);
          planCell += `<br><span style="color:var(--text3);font-size:0.85em">配额已用完，${formatHMS(remaining)}后重置</span>`;
        }
        html += `<tr>
          <td>${planCell}</td>
          <td>${esc(windowLabel)}</td>
          <td>${countLabel}</td>
          <td><div class="progress-bar"><div class="progress-fill ${fillClass}" style="width:${pct}%"></div></div></td>
        </tr>`;
      });
      html += "</table>";
    }
    el.innerHTML = html;
  }

  function renderTokenInfo(info) {
    const el = document.getElementById("token-info");
    const input = info.total_input_tokens;
    const output = info.total_output_tokens;
    // If both are null the SpendLogs table doesn't exist — hide the card.
    if (input == null && output == null) {
      el.innerHTML = '<p style="color:var(--text3)">Token usage data not available.</p>';
      return;
    }
    const total = (input || 0) + (output || 0);
    const inputPct = total > 0 ? ((input || 0) / total * 100).toFixed(1) : 0;
    const outputPct = total > 0 ? ((output || 0) / total * 100).toFixed(1) : 0;
    el.innerHTML = `
      <div class="token-stats">
        <div class="token-stat">
          <div class="token-stat-label">Input Tokens</div>
          <div class="token-stat-value">${formatNumber(input || 0)}</div>
          <div class="token-stat-pct">${inputPct}%</div>
        </div>
        <div class="token-stat">
          <div class="token-stat-label">Output Tokens</div>
          <div class="token-stat-value">${formatNumber(output || 0)}</div>
          <div class="token-stat-pct">${outputPct}%</div>
        </div>
        <div class="token-stat token-stat-total">
          <div class="token-stat-label">Total</div>
          <div class="token-stat-value">${formatNumber(total)}</div>
        </div>
      </div>
    `;
  }

  function renderKeyInfo(info) {
    const el = document.getElementById("key-info");
    if (info.error) { el.innerHTML = `<p>${esc(info.error)}</p>`; return; }
    const rows = [
      ["Key Alias", info.key_alias || "-"],
      ["Token", info.token_prefix],
      ["Key Name", info.key_name || "-"],
      ["Spend", "$" + (info.spend || 0).toFixed(4)],
      ["Max Budget", info.max_budget != null ? "$" + info.max_budget : "Unlimited"],
      ["Blocked", info.blocked ? "Yes" : "No"],
      ["RPM Limit", info.rpm_limit || "Default"],
      ["Expires", info.expires || "Never"],
      ["Created", info.created_at || "-"],
    ];
    el.innerHTML = `<table>${rows.map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(String(v))}</td></tr>`).join("")}</table>`;
  }

  function startUsageRefresh() {
    clearUsageRefresh();
    usageRefreshTimer = setInterval(async () => {
      try {
        const usage = await api("/user/usage");
        renderUsage(usage);
      } catch {}
    }, 5000);
  }

  function clearUsageRefresh() {
    if (usageRefreshTimer) { clearInterval(usageRefreshTimer); usageRefreshTimer = null; }
  }

  // ── Admin: Plans ──────────────────────────────────────
  async function loadPlans() {
    try {
      const data = await api("/admin/plans");
      renderPlansTable(data.plans || []);
    } catch {}
  }

  function renderPlansTable(plans) {
    const wrap = document.getElementById("plans-table-wrap");
    if (plans.length === 0) { wrap.innerHTML = "<p>No plans defined.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Name</th><th>Concurrency</th><th>RPM</th><th>Windows</th><th>Actions</th></tr>
      ${plans.map((p) => `<tr>
        <td><strong>${esc(p.name)}</strong></td>
        <td>${p.concurrency_limit || "-"}</td>
        <td>${p.rpm_limit || "-"}</td>
        <td>${(p.window_limits || []).map(([l, w]) => `${l}/${formatDuration(w)}`).join(", ") || "-"}</td>
        <td>
          <button class="btn-small" onclick="window._editPlan('${esc(p.name)}')">Edit</button>
          <button class="btn-danger" onclick="window._deletePlan('${esc(p.name)}')">Delete</button>
        </td>
      </tr>`).join("")}
    </table>`;
  }

  window._deletePlan = async (name) => {
    if (!confirm(`Delete plan "${name}"?`)) return;
    await api(`/admin/plans/${encodeURIComponent(name)}`, { method: "DELETE" });
    loadPlans();
  };

  // ── Admin: Keys ───────────────────────────────────────
  let keysPage = 1;
  let keysSearch = "";
  let keysSearchTimer = null;
  let keysVipOnly = false;
  let keysDataCache = [];

  function setupKeysSearch() {
    const el = document.getElementById("keys-search");
    if (!el) return;
    el.value = keysSearch;
    el.addEventListener("input", () => {
      clearTimeout(keysSearchTimer);
      keysSearchTimer = setTimeout(() => {
        keysSearch = el.value.trim();
        keysPage = 1;
        loadKeys();
      }, 300);
    });
  }

  async function loadKeys(page) {
    if (page !== undefined) keysPage = page;
    try {
      // Load prompt log excluded keys list.
      try {
        const plData = await api("/admin/prompt-log/status");
        window._promptLogExcludedKeys = plData.excluded_keys || [];
      } catch { window._promptLogExcludedKeys = []; }
      let url = `/admin/keys?page=${keysPage}&per_page=50`;
      if (keysSearch) url += `&search=${encodeURIComponent(keysSearch)}`;
      if (keysVipOnly) url += "&vip_only=true";
      const data = await api(url);
      keysDataCache = data.keys || [];
      renderKeysTable(keysDataCache);
      renderKeysPagination(data);
    } catch (err) {
      const wrap = document.getElementById("keys-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">Failed to load keys: ${esc(err.message)}</p>`;
      console.error("loadKeys error:", err);
    }
  }

  function renderKeysTable(keys) {
    const wrap = document.getElementById("keys-table-wrap");
    if (keys.length === 0) { wrap.innerHTML = "<p>No keys found.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Token</th><th>Alias</th><th>User</th><th>Plan</th><th>Usage</th><th>Reset</th><th>Spend</th><th>Budget</th><th>Status</th><th>Actions</th></tr>
      ${keys.map((k) => `<tr>
        <td class="mono">${esc(k.token_prefix)}</td>
        <td>${esc(k.key_alias || "-")}</td>
        <td>${esc(k.user_id || "-")}</td>
        <td>${esc(k.plan_name || "-")}</td>
        <td>${k.usage_count || 0}</td>
        <td>${formatCountdown(k.usage_reset_secs || 0)}</td>
        <td>$${(k.spend || 0).toFixed(4)}</td>
        <td>${k.max_budget != null ? "$" + k.max_budget : "-"}</td>
        <td>${k.blocked
              ? '<span style="color:var(--danger)">Blocked</span>'
              : (k.metadata && k.metadata.vip === true)
                ? '<span class="badge badge-vip">Active(VIP)</span>'
                : "Active"}</td>
        <td>
          <button class="btn-small" onclick="window._editKey('${esc(k.token_hash)}')">Edit</button>
          <button class="btn-small" onclick="window._resetKeyLimits('${esc(k.token_hash)}')">Reset Limits</button>
          ${k.blocked
            ? `<button class="btn-small" onclick="window._unblockKey('${esc(k.token_hash)}')">Unblock</button>`
            : `<button class="btn-danger" onclick="window._blockKey('${esc(k.token_hash)}')">Block</button>`}
        </td>
      </tr>`).join("")}
    </table>`;
  }

  function renderKeysPagination(data) {
    const el = document.getElementById("keys-pagination");
    const pages = Math.ceil(data.total / data.per_page);
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._loadKeysPage(${data.page - 1})">&lt;</button>
      <span>Page ${data.page} of ${pages} (${data.total} keys)</span>
      <button ${data.page >= pages ? "disabled" : ""} onclick="window._loadKeysPage(${data.page + 1})">&gt;</button>
    `;
  }

  window._loadKeysPage = (p) => loadKeys(p);
  window._editKey = (tokenHash) => {
    const key = keysDataCache.find((k) => k.token_hash === tokenHash);
    if (key) showEditKeyModal(key);
  };
  window._blockKey = async (hash) => {
    await api(`/admin/keys/${encodeURIComponent(hash)}/block`, { method: "POST" });
    loadKeys();
  };
  window._unblockKey = async (hash) => {
    await api(`/admin/keys/${encodeURIComponent(hash)}/unblock`, { method: "POST" });
    loadKeys();
  };
  window._resetKeyLimits = async (hash) => {
    if (!confirm("Reset all rate limit windows for this key?")) return;
    const r = await api(`/admin/limits/reset/${encodeURIComponent(hash)}`, { method: "POST" });
    alert(r.message || "Done");
  };

  // ── Admin: Assignments ────────────────────────────────
  async function loadAssignments() {
    try {
      const data = await api("/admin/assignments");
      renderAssignmentsTable(data.assignments || []);
    } catch {}
  }

  function renderAssignmentsTable(assignments) {
    const wrap = document.getElementById("assignments-table-wrap");
    if (assignments.length === 0) { wrap.innerHTML = "<p>No assignments.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Key Hash</th><th>Plan</th><th>Actions</th></tr>
      ${assignments.map((a) => `<tr>
        <td class="mono">${esc(a.key_hash.substring(0, 16))}...</td>
        <td>${esc(a.plan_name)}</td>
        <td><button class="btn-danger" onclick="window._unassignKey('${esc(a.key_hash)}')">Remove</button></td>
      </tr>`).join("")}
    </table>`;
  }

  window._unassignKey = async (hash) => {
    await api(`/admin/assignments/${encodeURIComponent(hash)}`, { method: "DELETE" });
    loadAssignments();
  };

  // ── Admin: Models ─────────────────────────────────────
  async function loadModels() {
    try {
      const data = await api("/admin/models");
      renderModelsTable(data.models || []);
    } catch (err) {
      const wrap = document.getElementById("models-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">Failed to load models: ${esc(err.message)}</p>`;
    }
  }

  function renderModelsTable(models) {
    const wrap = document.getElementById("models-table-wrap");
    if (models.length === 0) { wrap.innerHTML = "<p>No model deployments.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Model Name</th><th>LiteLLM Model</th><th>Base URL</th><th>Quota Ratio</th><th>RPM</th><th>Timeout</th><th>Enabled</th><th>Source</th><th>Actions</th></tr>
      ${models.map((m) => {
        const isAutoDisabled = !m.enabled && m.auto_disabled;
        const enabledBadge = m.enabled
          ? '<span class="badge badge-active">Yes</span>'
          : isAutoDisabled
            ? '<span class="badge badge-blocked">No</span><br><span style="color:var(--danger);font-size:0.8em">Auto-disabled</span>'
            : '<span class="badge badge-blocked">No</span>';
        const warningRow = isAutoDisabled
          ? `<tr style="background:rgba(255,80,80,0.08)"><td colspan="9" style="padding:4px 8px;font-size:0.85em;color:var(--danger)">Fault auto-disabled: this deployment was automatically disabled due to consecutive failures. Please fix the upstream issue and re-enable it.</td></tr>`
          : '';
        return `<tr${isAutoDisabled ? ' style="background:rgba(255,80,80,0.04)"' : ''}>
        <td><strong>${esc(m.model_name)}</strong></td>
        <td class="mono">${esc(m.litellm_model)}</td>
        <td class="mono">${esc(m.api_base || "-")}</td>
        <td>${m.quota_count_ratio && m.quota_count_ratio !== 1 ? '<span class="badge badge-plan">x' + m.quota_count_ratio + '</span>' : 'x1'}</td>
        <td>${m.rpm || "-"}</td>
        <td>${m.timeout}s</td>
        <td>${enabledBadge}</td>
        <td><span class="badge badge-plan">${esc(m.source || "-")}</span></td>
        <td>
          <button class="btn-small" onclick="window._editModel('${m.id}')">Edit</button>
          <button class="btn-danger" onclick="window._deleteModel('${m.id}','${esc(m.model_name)}')">Delete</button>
        </td>
      </tr>${warningRow}`;
      }).join("")}
    </table>`;
  }

  function showNewModelModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.id ? "Edit" : "Create"} Model Deployment</h3>
      <div class="form-group"><label>Model Name * ${tip("Client-visible model name. Multiple deployments can share the same name for load balancing.")}</label><input id="m-model-name" value="${esc(p.model_name || "")}" required></div>
      <div class="form-group"><label>Provider * ${tip("Upstream provider type. Determines API format and authentication.")}</label><select id="m-model-provider"><option value="">-- select --</option><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="azure">Azure OpenAI</option><option value="gemini">Google Gemini</option><option value="bedrock">AWS Bedrock</option></select></div>
      <div class="form-group"><label>Model ID * ${tip("Actual model ID at the provider, e.g. gpt-4o, claude-sonnet-4-20250514. Auto-combined with Provider as provider/model-id.")}</label><input id="m-model-id" value="${esc((p.litellm_model || "").includes("/") ? p.litellm_model.split("/").slice(1).join("/") : p.litellm_model || "")}" required></div>
      <div class="form-group"><label>API Key ${tip("Provider API key. Use os.environ/VAR_NAME for env reference.")}</label><input id="m-model-key" type="password" value="${esc(p.api_key || "")}" placeholder="sk-... or os.environ/VAR"></div>
      <div class="form-group"><label>API Key is env reference ${tip("Enable if the API Key field contains an environment variable reference like os.environ/VAR_NAME.")}</label><select id="m-model-key-env"><option value="false">No</option><option value="true" ${(p.api_key_env) ? "selected" : ""}>Yes</option></select></div>
      <div class="form-group"><label>API Base URL ${tip("Override the default provider endpoint, e.g. https://api.openai.com/v1")}</label><input id="m-model-base" value="${esc(p.api_base || "")}" placeholder="https://api.openai.com/v1"></div>
      <div class="form-group"><label>API Version (Azure) ${tip("Required for Azure OpenAI deployments, e.g. 2024-02-01")}</label><input id="m-model-version" value="${esc(p.api_version || "")}"></div>
      <div class="form-group"><label>Quota Ratio ${tip("Quota consumption multiplier. Each request counts as this many units against rate limits. E.g. 3 means one request consumes 3 quota. Default: 1.")}</label><input id="m-model-ratio" type="number" min="1" step="1" value="${p.quota_count_ratio || 1}"></div>
      <div class="form-group"><label>RPM Limit ${tip("Per-deployment RPM limit. Leave empty for unlimited.")}</label><input id="m-model-rpm" type="number" value="${p.rpm || ""}"></div>
      <div class="form-group"><label>Timeout (seconds) ${tip("Request timeout. Default: 120s.")}</label><input id="m-model-timeout" type="number" value="${p.timeout || 120}"></div>
      <div class="form-group"><label>Temperature ${tip("Sampling temperature override (0.0-2.0). Leave empty to use provider default.")}</label><input id="m-model-temp" type="number" step="0.1" value="${p.temperature || ""}"></div>
      <div class="form-group"><label>Max Tokens ${tip("Maximum output tokens. Leave empty for provider default.")}</label><input id="m-model-maxtok" type="number" value="${p.max_tokens || ""}"></div>
      <div class="form-group"><label>Max Inflight ${tip("Max concurrent in-flight requests for this deployment. 0 or empty = unlimited.")}</label><input id="m-model-maxinflight" type="number" min="0" value="${p.max_inflight_queue_len || ""}"></div>
      <div class="form-group"><label>Max Context ${tip("Max total input characters across all in-flight requests. 0 or empty = unlimited.")}</label><input id="m-model-maxctx" type="number" min="0" value="${p.max_context_len || ""}"></div>
      <div class="form-group"><label>Enabled ${tip("Disabled deployments are ignored in routing.")}</label><select id="m-model-enabled"><option value="true" ${p.enabled !== false ? "selected" : ""}>Yes</option><option value="false" ${p.enabled === false ? "selected" : ""}>No</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-model-submit">${p.id ? "Update" : "Create"}</button>
      </div>
    `);
    // Pre-select provider dropdown from litellm_model
    if (p.litellm_model && p.litellm_model.includes("/")) {
      const prov = p.litellm_model.split("/")[0];
      const sel = document.getElementById("m-model-provider");
      if (sel.querySelector(`option[value="${prov}"]`)) sel.value = prov;
    }
    document.getElementById("m-model-submit").addEventListener("click", async () => {
      try {
        const providerVal = document.getElementById("m-model-provider").value;
        const modelIdVal = document.getElementById("m-model-id").value.trim();
        const litellmModel = providerVal ? providerVal + "/" + modelIdVal : modelIdVal;
        const body = {
          model_name: document.getElementById("m-model-name").value,
          litellm_model: litellmModel,
          api_key: document.getElementById("m-model-key").value || null,
          api_key_env: document.getElementById("m-model-key-env").value === "true",
          api_base: document.getElementById("m-model-base").value || null,
          api_version: document.getElementById("m-model-version").value || null,
          quota_count_ratio: Number(document.getElementById("m-model-ratio").value) || 1,
          rpm: document.getElementById("m-model-rpm").value ? Number(document.getElementById("m-model-rpm").value) : null,
          timeout: Number(document.getElementById("m-model-timeout").value) || 120,
          temperature: document.getElementById("m-model-temp").value ? Number(document.getElementById("m-model-temp").value) : null,
          max_tokens: document.getElementById("m-model-maxtok").value ? Number(document.getElementById("m-model-maxtok").value) : null,
          max_inflight_queue_len: document.getElementById("m-model-maxinflight").value ? Number(document.getElementById("m-model-maxinflight").value) : null,
          max_context_len: document.getElementById("m-model-maxctx").value ? Number(document.getElementById("m-model-maxctx").value) : null,
          enabled: document.getElementById("m-model-enabled").value === "true",
          headers: {},
        };
        const url = p.id ? `/admin/models/${p.id}` : "/admin/models";
        const method = p.id ? "PUT" : "POST";
        await api(url, { method, body: JSON.stringify(body) });
        hideModal();
        invalidateCaches();
        loadModels();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  window._editModel = async (id) => {
    try {
      const data = await api("/admin/models");
      const m = (data.models || []).find((x) => x.id === id);
      if (!m) return;
      showNewModelModal(m);
    } catch (err) { alert("Error: " + err.message); }
  };

  window._deleteModel = async (id, name) => {
    if (!confirm(`Delete model deployment "${name}"?`)) return;
    await api(`/admin/models/${encodeURIComponent(id)}`, { method: "DELETE" });
    loadModels();
  };

  // ── Admin: Aliases ────────────────────────────────────
  async function loadAliases() {
    try {
      const data = await api("/admin/aliases");
      renderAliasesTable(data.aliases || []);
    } catch (err) {
      const wrap = document.getElementById("aliases-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">Failed to load aliases: ${esc(err.message)}</p>`;
    }
  }

  function renderAliasesTable(aliases) {
    const wrap = document.getElementById("aliases-table-wrap");
    if (aliases.length === 0) { wrap.innerHTML = "<p>No aliases defined.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Alias</th><th>Target Model</th><th>Hidden</th><th>Source</th><th>Actions</th></tr>
      ${aliases.map((a) => `<tr>
        <td><strong>${esc(a.alias_name)}</strong></td>
        <td class="mono">${esc(a.target_model)}</td>
        <td>${a.hidden ? "Yes" : "No"}</td>
        <td><span class="badge badge-plan">${esc(a.source || "-")}</span></td>
        <td>
          <button class="btn-small" onclick="window._editAlias('${esc(a.alias_name)}')">Edit</button>
          <button class="btn-danger" onclick="window._deleteAlias('${esc(a.alias_name)}')">Delete</button>
        </td>
      </tr>`).join("")}
    </table>`;
  }

  function showNewAliasModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.alias_name ? "Edit" : "Create"} Alias</h3>
      <div class="form-group"><label>Alias Name * ${tip("The name clients will use in their request. E.g. 'gpt-4' → routes to 'gpt-4o'.")}</label><input id="m-alias-name" value="${esc(p.alias_name || "")}" ${p.alias_name ? "readonly" : ""}></div>
      <div class="form-group"><label>Target Model * ${tip("The actual model name to route to. Must match an existing model deployment name.")}</label><input id="m-alias-target" value="${esc(p.target_model || "")}" required list="alias-target-list"><datalist id="alias-target-list"></datalist></div>
      <div class="form-group"><label>Hidden ${tip("Hidden aliases work for routing but are not listed to users in model discovery endpoints.")}</label><select id="m-alias-hidden"><option value="false" ${!p.hidden ? "selected" : ""}>No</option><option value="true" ${p.hidden ? "selected" : ""}>Yes</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-alias-submit">${p.alias_name ? "Update" : "Create"}</button>
      </div>
    `);
    // Populate datalist with existing model names
    getModelNames().then((names) => {
      const dl = document.getElementById("alias-target-list");
      if (dl) names.forEach((n) => { const o = document.createElement("option"); o.value = n; dl.appendChild(o); });
    });
    document.getElementById("m-alias-submit").addEventListener("click", async () => {
      try {
        const body = {
          alias_name: document.getElementById("m-alias-name").value,
          target_model: document.getElementById("m-alias-target").value,
          hidden: document.getElementById("m-alias-hidden").value === "true",
        };
        const url = p.alias_name ? `/admin/aliases/${encodeURIComponent(p.alias_name)}` : "/admin/aliases";
        const method = p.alias_name ? "PUT" : "POST";
        await api(url, { method, body: JSON.stringify(body) });
        hideModal();
        invalidateCaches();
        loadAliases();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  window._editAlias = async (name) => {
    try {
      const data = await api("/admin/aliases");
      const a = (data.aliases || []).find((x) => x.alias_name === name);
      if (!a) return;
      showNewAliasModal(a);
    } catch (err) { alert("Error: " + err.message); }
  };

  window._deleteAlias = async (name) => {
    if (!confirm(`Delete alias "${name}"?`)) return;
    await api(`/admin/aliases/${encodeURIComponent(name)}`, { method: "DELETE" });
    loadAliases();
  };

  // ── Admin: Config ─────────────────────────────────────
  async function loadConfig() {
    try {
      const data = await api("/admin/config");
      renderConfigTable(data.config || {});
    } catch (err) {
      const wrap = document.getElementById("config-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">Failed to load config: ${esc(err.message)}</p>`;
    }
  }

  function renderConfigTable(config) {
    const wrap = document.getElementById("config-table-wrap");
    const keys = Object.keys(config);
    if (keys.length === 0) { wrap.innerHTML = "<p>No configuration entries.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Key</th><th>Value</th><th>Actions</th></tr>
      ${keys.map((k) => `<tr>
        <td><strong>${esc(k)}</strong></td>
        <td class="mono" style="max-width:400px;word-break:break-all;white-space:pre-wrap">${esc(JSON.stringify(config[k], null, 2))}</td>
        <td><button class="btn-small" onclick="window._editConfig('${esc(k)}')">Edit</button></td>
      </tr>`).join("")}
    </table>`;
  }

  function showNewConfigModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.key ? "Edit" : "Set"} Configuration</h3>
      <div class="form-group"><label>Key * ${tip("Configuration key name, e.g. 'general_settings' or a custom key.")}</label><input id="m-config-key" value="${esc(p.key || "")}" ${p.key ? "readonly" : ""}></div>
      <div class="form-group"><label>Value (JSON) * ${tip("Configuration value as valid JSON. E.g. {\"store_model_in_db\": true}")}</label><textarea id="m-config-value" rows="6">${esc(p.value ? JSON.stringify(p.value, null, 2) : "")}</textarea></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-config-submit">Save</button>
      </div>
    `);
    document.getElementById("m-config-submit").addEventListener("click", async () => {
      try {
        const value = JSON.parse(document.getElementById("m-config-value").value);
        await api("/admin/config", {
          method: "PATCH",
          body: JSON.stringify({
            key: document.getElementById("m-config-key").value,
            value: value,
          }),
        });
        hideModal();
        loadConfig();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  window._editConfig = async (key) => {
    try {
      const data = await api("/admin/config");
      const config = data.config || {};
      if (config[key] !== undefined) {
        showNewConfigModal({ key, value: config[key] });
      }
    } catch (err) { alert("Error: " + err.message); }
  };

  // ── Admin: Debug Error Recording ─────────────────────
  let debugEnabled = false;

  async function loadDebugStatus() {
    const btn = document.getElementById("btn-debug-toggle");
    if (!btn) return;
    try {
      const data = await api("/admin/debug/status");
      debugEnabled = data.enabled;
      updateDebugButton(btn);
    } catch {}
  }

  function updateDebugButton(btn) {
    if (!btn) return;
    btn.textContent = debugEnabled ? "Debug: ON" : "Debug: OFF";
    btn.style.background = debugEnabled ? "var(--danger)" : "";
    btn.style.color = debugEnabled ? "#fff" : "";
  }

  async function toggleDebug() {
    const btn = document.getElementById("btn-debug-toggle");
    if (!btn) return;
    try {
      const data = await api("/admin/debug/toggle", {
        method: "POST",
        body: JSON.stringify({ enabled: !debugEnabled }),
      });
      debugEnabled = data.enabled;
      updateDebugButton(btn);
    } catch (err) { alert("Error: " + err.message); }
  }

  async function showDebugError(requestId) {
    try {
      const data = await api("/admin/debug/errors/" + requestId);
      showModal("<pre style='max-height:60vh;overflow:auto'>" + esc(JSON.stringify(data, null, 2)) + "</pre>");
    } catch (err) { alert("Error: " + err.message); }
  }

  // ── Prompt Log toggle (same pattern as Debug toggle) ──

  let promptLogEnabled = false;

  async function loadPromptLogStatus() {
    const btn = document.getElementById("btn-prompt-log-toggle");
    if (!btn) return;
    try {
      const data = await api("/admin/prompt-log/status");
      promptLogEnabled = data.enabled;
      updatePromptLogButton(btn);
    } catch {}
  }

  function updatePromptLogButton(btn) {
    if (!btn) return;
    btn.textContent = promptLogEnabled ? "Prompt Log: ON" : "Prompt Log: OFF";
    btn.style.background = promptLogEnabled ? "#2563eb" : "";
    btn.style.color = promptLogEnabled ? "#fff" : "";
  }

  async function togglePromptLog() {
    const btn = document.getElementById("btn-prompt-log-toggle");
    if (!btn) return;
    try {
      const data = await api("/admin/prompt-log/toggle", {
        method: "POST",
        body: JSON.stringify({ enabled: !promptLogEnabled }),
      });
      promptLogEnabled = data.enabled;
      updatePromptLogButton(btn);
    } catch (err) { alert("Error: " + err.message); }
  }

  // Check if a team is excluded from prompt logging.
  let promptLogExcludedTeams = [];
  async function loadPromptLogExcludedTeams() {
    try {
      const data = await api("/admin/prompt-log/status");
      // We don't get the full list from status; load from config if needed.
      // For now, we'll just track via the team toggle state per-row.
    } catch {}
  }

  async function toggleTeamPromptLog(teamId, excluded) {
    try {
      await api("/admin/prompt-log/team", {
        method: "POST",
        body: JSON.stringify({ team_id: teamId, excluded: !excluded }),
      });
      loadTeams();
    } catch (err) { alert("Error: " + err.message); }
  }

  async function showDebugError(requestId) {
    try {
      const data = await api("/admin/debug/errors/" + encodeURIComponent(requestId));
      const e = data.debug_error;
      if (!e) { alert("Debug entry not found"); return; }

      let upstreamHtml = "";
      if (e.upstream_status != null) {
        upstreamHtml = `
          <div class="debug-section">
            <h4>Upstream Response</h4>
            <table>
              <tr><td style="width:120px">Status</td><td>${e.upstream_status}</td></tr>
              <tr><td>Body</td><td><pre class="debug-json">${esc(formatJson(e.upstream_body || "-"))}</pre></td></tr>
            </table>
          </div>`;
      }

      let requestHtml = "";
      if (e.request_body) {
        requestHtml = `
          <div class="debug-section">
            <h4>Original Request</h4>
            <pre class="debug-json">${esc(formatJson(e.request_body))}</pre>
          </div>`;
      }

      showModal(`
        <h3>Debug: ${esc(e.error_type)}</h3>
        <table>
          <tr><td style="width:120px">Request ID</td><td class="mono">${esc(e.request_id)}</td></tr>
          <tr><td>Key</td><td>${esc(e.key_alias || e.key_hash.substring(0, 12) + "...")}</td></tr>
          <tr><td>Model</td><td class="mono">${esc(e.model)}</td></tr>
          <tr><td>Path</td><td class="mono">${esc(e.api_path)}</td></tr>
          <tr><td>Stream</td><td>${e.is_stream ? "Yes" : "No"}</td></tr>
          <tr><td>Time</td><td>${formatTimestamp(e.created_at)}</td></tr>
          <tr><td>Status</td><td>${e.status_code}</td></tr>
          <tr><td>Error</td><td>${esc(e.error_message)}</td></tr>
        </table>
        ${upstreamHtml}
        ${requestHtml}
        <div class="modal-actions">
          <button class="btn-secondary" onclick="hideModal()" style="width:auto">Close</button>
        </div>
      `);
    } catch (err) { alert("Error: " + err.message); }
  }

  window._showDebugError = showDebugError;

  function formatJson(str) {
    try { return JSON.stringify(JSON.parse(str), null, 2); } catch { return str; }
  }

  // ── Admin: Modals ─────────────────────────────────────
  function setupAdminButtons() {
    document.getElementById("btn-new-plan").addEventListener("click", showNewPlanModal);
    const btnResetAll = document.getElementById("btn-reset-all-limits");
    if (btnResetAll) btnResetAll.addEventListener("click", async () => {
      if (!confirm("Reset ALL rate limit windows for ALL keys?")) return;
      const r = await api("/admin/limits/reset", { method: "POST" });
      alert(r.message || "Done");
    });
    document.getElementById("btn-new-key").addEventListener("click", showNewKeyModal);
    const btnVipFilter = document.getElementById("btn-vip-filter");
    if (btnVipFilter) btnVipFilter.addEventListener("click", () => {
      keysVipOnly = !keysVipOnly;
      btnVipFilter.style.background = keysVipOnly ? "var(--primary)" : "";
      btnVipFilter.style.color = keysVipOnly ? "#fff" : "";
      keysPage = 1;
      loadKeys();
    });
    document.getElementById("btn-new-assignment").addEventListener("click", showNewAssignmentModal);
    const btnModel = document.getElementById("btn-new-model");
    if (btnModel) btnModel.addEventListener("click", showNewModelModal);
    const btnAlias = document.getElementById("btn-new-alias");
    if (btnAlias) btnAlias.addEventListener("click", showNewAliasModal);
    const btnConfig = document.getElementById("btn-new-config");
    if (btnConfig) btnConfig.addEventListener("click", showNewConfigModal);
    const btnReload = document.getElementById("btn-reload-config");
    if (btnReload) btnReload.addEventListener("click", async () => {
      if (!confirm("Hot-reload config.yaml without restart?")) return;
      btnReload.disabled = true;
      btnReload.textContent = "Reloading...";
      try {
        const data = await api("/admin/config/reload", { method: "POST" });
        alert(data.message || "Config reloaded successfully");
        onRoute();
      } catch (err) { alert("Reload error: " + err.message); }
      finally {
        btnReload.disabled = false;
        btnReload.textContent = "Reload Config";
      }
    });
    const btnDebug = document.getElementById("btn-debug-toggle");
    if (btnDebug) btnDebug.addEventListener("click", toggleDebug);
    loadDebugStatus();
    const btnPromptLog = document.getElementById("btn-prompt-log-toggle");
    if (btnPromptLog) btnPromptLog.addEventListener("click", togglePromptLog);
    loadPromptLogStatus();
  }

  function showModal(html) {
    document.getElementById("modal-content").innerHTML = html;
    document.getElementById("modal-overlay").classList.remove("hidden");
  }

  function hideModal() {
    document.getElementById("modal-overlay").classList.add("hidden");
  }
  window.hideModal = hideModal;

  document.getElementById("modal-overlay").addEventListener("click", (e) => {
    if (e.target === e.currentTarget) hideModal();
  });

  function showNewPlanModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.name ? "Edit" : "Create"} Plan</h3>
      <div class="form-group"><label>Name ${tip("Unique plan name. Used when assigning keys to plans.")}</label><input id="m-plan-name" value="${esc(p.name || "")}" ${p.name ? "readonly" : ""} required></div>
      <div class="form-group"><label>Concurrency Limit ${tip("Maximum simultaneous requests per key in this plan. Leave empty for unlimited.")}</label><input id="m-plan-concurrency" type="number" value="${p.concurrency_limit || ""}"></div>
      <div class="form-group"><label>RPM Limit ${tip("Maximum requests per minute per key. Leave empty for unlimited.")}</label><input id="m-plan-rpm" type="number" value="${p.rpm_limit || ""}"></div>
      <div class="form-group"><label>Window Limits ${tip("Custom time windows as JSON array: [[count, seconds], ...]. E.g. [[100,18000]] = 100 requests per 5 hours. Each request's quota consumption is multiplied by the model's Quota Ratio.")}</label><textarea id="m-plan-windows" rows="2">${JSON.stringify(p.window_limits || [])}</textarea></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-plan-submit">${p.name ? "Update" : "Create"}</button>
      </div>
    `);
    document.getElementById("m-plan-submit").addEventListener("click", async () => {
      try {
        const windows = JSON.parse(document.getElementById("m-plan-windows").value || "[]");
        await api("/admin/plans", {
          method: "PUT",
          body: JSON.stringify({
            name: document.getElementById("m-plan-name").value,
            concurrency_limit: document.getElementById("m-plan-concurrency").value ? Number(document.getElementById("m-plan-concurrency").value) : null,
            rpm_limit: document.getElementById("m-plan-rpm").value ? Number(document.getElementById("m-plan-rpm").value) : null,
            window_limits: windows,
          }),
        });
        hideModal();
        invalidateCaches();
        loadPlans();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  window._editPlan = async (name) => {
    try {
      const data = await api("/admin/plans");
      const p = (data.plans || []).find((x) => x.name === name);
      if (!p) return;
      showNewPlanModal(p);
    } catch (err) { alert("Error: " + err.message); }
  };

  function showNewKeyModal() {
    showModal(`
      <h3>Create API Key</h3>
      <div class="form-group"><label>Key Alias ${tip("Short unique identifier for this key, e.g. 'alice' or 'team-api'. Used for display in dashboard and debug logging.")}</label><input id="m-key-alias"></div>
      <div class="form-group"><label>Key Name ${tip("Human-readable name for this key.")}</label><input id="m-key-name"></div>
      <div class="form-group"><label>User ID ${tip("Optional user identifier for tracking.")}</label><input id="m-key-user"></div>
      <div class="form-group"><label>Team ID ${tip("Optional team identifier.")}</label><input id="m-key-team"></div>
      <div class="form-group"><label>Models ${tip("Select model access. Check 'all-team-models' for full access, or pick specific models.")}</label><div class="model-check-combo" id="m-key-models-combo"></div></div>
      <div class="form-group"><label>Max Budget ${tip("Maximum budget in USD. Leave empty for unlimited.")}</label><input id="m-key-budget" type="number" step="0.01"></div>
      <div class="form-group"><label>RPM Limit ${tip("Per-key RPM override. Leave empty to use plan or default limits.")}</label><input id="m-key-rpm" type="number"></div>
      <div class="form-group"><label>Plan ${tip("Assign this key to a rate limit plan. Leave empty for default plan.")}</label><input id="m-key-plan" list="key-plan-list"><datalist id="key-plan-list"></datalist></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-key-submit">Create</button>
      </div>
    `);
    // Populate model checkbox combo
    getModelNames().then((names) => {
      const container = document.getElementById("m-key-models-combo");
      if (container) initModelCombo(container, [], names);
    });
    getPlanNames().then((names) => {
      const dl = document.getElementById("key-plan-list");
      if (dl) names.forEach((n) => { const o = document.createElement("option"); o.value = n; dl.appendChild(o); });
    });
    document.getElementById("m-key-submit").addEventListener("click", async () => {
      try {
        const modelsVal = getComboModels("m-key-models-combo");
        const data = await api("/admin/keys", {
          method: "POST",
          body: JSON.stringify({
            key_alias: document.getElementById("m-key-alias").value.trim() || null,
            key_name: document.getElementById("m-key-name").value || null,
            user_id: document.getElementById("m-key-user").value || null,
            team_id: document.getElementById("m-key-team").value || null,
            models: modelsVal,
            max_budget: document.getElementById("m-key-budget").value ? Number(document.getElementById("m-key-budget").value) : null,
            rpm_limit: document.getElementById("m-key-rpm").value ? Number(document.getElementById("m-key-rpm").value) : null,
            plan_name: document.getElementById("m-key-plan").value || null,
          }),
        });
        hideModal();
        showModal(`
          <h3>Key Created</h3>
          <p class="key-warning">Copy this key now. It will NOT be shown again.</p>
          <div class="key-display">${esc(data.key)}</div>
          <p>Key Name: ${esc(data.key_name || "-")}</p>
          <div class="modal-actions">
            <button class="btn-primary" onclick="hideModal(); window._loadKeysPage();">Done</button>
          </div>
        `);
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  function showEditKeyModal(key) {
    const existingModels = Array.isArray(key.models) ? key.models : [];
    const isVip = key.metadata && key.metadata.vip === true;
    const isPromptLogExcluded = (window._promptLogExcludedKeys || []).includes(key.token_hash);
    showModal(`
      <h3>Edit Key</h3>
      <div class="form-group"><label>Alias ${tip("Short unique identifier for this key, e.g. 'alice'. Used for dashboard display and debug logging.")}</label><input id="m-edit-alias" value="${esc(key.key_alias || "")}"></div>
      <div class="form-group"><label>User ID ${tip("Optional user identifier.")}</label><input id="m-edit-user" value="${esc(key.user_id || "")}"></div>
      <div class="form-group"><label>Models ${tip("Select model access. Check 'all-team-models' for full access, or pick specific models.")}</label><div class="model-check-combo" id="m-edit-models-combo"></div></div>
      <div class="form-group"><label>Max Budget ${tip("Maximum budget in USD. Leave empty for unlimited.")}</label><input id="m-edit-budget" type="number" step="0.01" value="${key.max_budget != null ? key.max_budget : ""}"></div>
      <div class="form-group"><label>RPM Limit ${tip("Per-key RPM override. Leave empty to use plan limits.")}</label><input id="m-edit-rpm" type="number" value="${key.rpm_limit || ""}"></div>
      <div class="form-group"><label>Plan ${tip("Rate limit plan assigned to this key. Change via Assignments page.")}</label><input value="${esc(key.plan_name || "Default")}" readonly style="background:var(--surface3);cursor:not-allowed"></div>
      <div class="form-group"><label>VIP ${tip("VIP keys get priority in flow control queues when deployments are at capacity.")}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-vip" ${isVip ? "checked" : ""}><span style="font-weight:600;color:#b45309;white-space:nowrap">Priority queue access</span></div></div>
      <div class="form-group"><label>Prompt Log ${tip("Disable prompt logging for this key. When the global prompt log switch is ON, this key will be excluded from capture.")}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-no-prompt-log" ${isPromptLogExcluded ? "checked" : ""}><span style="font-weight:600;color:#dc2626;white-space:nowrap">Disable prompt logging</span></div></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-edit-submit">Save</button>
      </div>
    `);
    // Populate model checkbox combo with existing models pre-checked
    getModelNames().then((names) => {
      const container = document.getElementById("m-edit-models-combo");
      if (container) initModelCombo(container, existingModels, names);
    });
    document.getElementById("m-edit-submit").addEventListener("click", async () => {
      try {
        const aliasVal = document.getElementById("m-edit-alias").value.trim();
        const userVal = document.getElementById("m-edit-user").value.trim();
        const modelsVal = getComboModels("m-edit-models-combo");
        const vipChecked = document.getElementById("m-edit-vip").checked;
        // Preserve existing metadata fields, only update vip flag.
        const existingMeta = key.metadata && typeof key.metadata === "object" ? key.metadata : {};
        const body = {
          key_alias: aliasVal || null,
          user_id: userVal || null,
          models: modelsVal,
          max_budget: document.getElementById("m-edit-budget").value ? Number(document.getElementById("m-edit-budget").value) : null,
          rpm_limit: document.getElementById("m-edit-rpm").value ? Number(document.getElementById("m-edit-rpm").value) : null,
          metadata: Object.assign({}, existingMeta, { vip: vipChecked }),
        };
        await api(`/admin/keys/${encodeURIComponent(key.token_hash)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        });
        // Update prompt log exclusion for this key.
        const noPromptLog = document.getElementById("m-edit-no-prompt-log").checked;
        if (noPromptLog !== isPromptLogExcluded) {
          await api("/admin/prompt-log/key", {
            method: "POST",
            body: JSON.stringify({ key_hash: key.token_hash, excluded: noPromptLog }),
          });
        }
        hideModal();
        loadKeys();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  function showNewAssignmentModal() {
    showModal(`
      <h3>Assign Key to Plan</h3>
      <div class="form-group"><label>Key Hash ${tip("The full key hash of the API key to assign.")}</label><input id="m-asgn-hash" required list="asgn-hash-list"><datalist id="asgn-hash-list"></datalist></div>
      <div class="form-group"><label>Plan ${tip("Select an existing plan to assign this key to.")}</label><input id="m-asgn-plan" required list="asgn-plan-list"><datalist id="asgn-plan-list"></datalist></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-asgn-submit">Assign</button>
      </div>
    `);
    // Populate plan datalist
    getPlanNames().then((names) => {
      const dl = document.getElementById("asgn-plan-list");
      if (dl) names.forEach((n) => { const o = document.createElement("option"); o.value = n; dl.appendChild(o); });
    });
    // Populate key hash datalist from existing keys
    (async () => {
      try {
        const data = await api("/admin/keys");
        const dl = document.getElementById("asgn-hash-list");
        if (dl) (data.keys || []).forEach((k) => {
          const o = document.createElement("option");
          o.value = k.token_hash;
          o.label = k.key_alias || k.token_hash.substring(0, 12) + "...";
          dl.appendChild(o);
        });
      } catch {}
    })();
    document.getElementById("m-asgn-submit").addEventListener("click", async () => {
      try {
        await api("/admin/assignments", {
          method: "POST",
          body: JSON.stringify({
            key_hash: document.getElementById("m-asgn-hash").value,
            plan_name: document.getElementById("m-asgn-plan").value,
          }),
        });
        hideModal();
        loadAssignments();
      } catch (err) { alert("Error: " + err.message); }
    });
  }

  // ── Admin: Teams ──────────────────────────────────────
  async function loadTeams() {
    try {
      // Load prompt log status alongside teams to know excluded teams.
      try {
        const plData = await api("/admin/prompt-log/status");
        window._promptLogExcludedTeams = plData.excluded_teams || [];
      } catch { window._promptLogExcludedTeams = []; }
      const data = await api("/admin/teams");
      renderTeamsTable(data.teams || []);
    } catch (err) {
      const wrap = document.getElementById("teams-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">Failed to load teams: ${esc(err.message)}</p>`;
    }
  }

  function renderTeamsTable(teams) {
    const wrap = document.getElementById("teams-table-wrap");
    if (teams.length === 0) { wrap.innerHTML = "<p>No teams found.</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>Team Alias</th><th>Team ID</th><th>Models</th><th>Keys</th><th>Requests</th><th>Input Tokens</th><th>Output Tokens</th><th>Total Tokens</th><th>Prompt Log</th><th>Actions</th></tr>
      ${teams.map((t) => {
        const isExcluded = (window._promptLogExcludedTeams || []).includes(t.team_id);
        const logBtnClass = isExcluded ? "btn-secondary" : "btn-primary";
        const logBtnText = isExcluded ? "OFF" : "ON";
        return `<tr>
        <td>${esc(t.team_alias || "-")}</td>
        <td class="mono" title="${esc(t.team_id)}">${esc((t.team_id || "").substring(0, 12))}</td>
        <td class="mono">${esc(formatTeamModels(t.models))}</td>
        <td>${t.key_count}</td>
        <td>${formatNumber(t.request_count)}</td>
        <td>${formatNumber(t.total_input_tokens || 0)}</td>
        <td>${formatNumber(t.total_output_tokens || 0)}</td>
        <td>${formatNumber((t.total_input_tokens || 0) + (t.total_output_tokens || 0))}</td>
        <td><button class="${logBtnClass} btn-sm" onclick='window._toggleTeamPromptLog(${JSON.stringify(t.team_id)}, ${isExcluded})'>${logBtnText}</button></td>
        <td>
          <button class="btn-secondary btn-sm" onclick='window._editTeam(${JSON.stringify(t.team_id)})'>Edit</button>
          <button class="btn-danger btn-sm" onclick='window._deleteTeam(${JSON.stringify(t.team_id)}, ${t.key_count})'>Delete</button>
        </td>
      </tr>`;
      }).join("")}
    </table>`;
  }

  function formatTeamModels(models) {
    if (!models || models.length === 0) return "all-team-models";
    if (models.includes("all-team-models")) return "all-team-models";
    return models.join(", ");
  }

  window.showCreateTeamModal = function(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.team_id ? "Edit" : "Create"} Team</h3>
      <div class="form-group"><label>Team ID ${tip("Unique identifier for this team. Cannot be changed after creation.")}</label><input id="m-team-id" value="${esc(p.team_id || "")}" ${p.team_id ? "readonly" : ""} required></div>
      <div class="form-group"><label>Team Alias ${tip("Display name for this team. Can be non-unique.")}</label><input id="m-team-alias" value="${esc(p.team_alias || "")}"></div>
      <div class="form-group"><label>Models ${tip("Select model access for this team. Check 'all-team-models' for full access to all current and future models, or pick specific models.")}</label><div class="model-check-combo" id="m-team-models-combo"></div></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">Cancel</button>
        <button class="btn-primary" id="m-team-submit">${p.team_id ? "Update" : "Create"}</button>
      </div>
    `);
    getModelNames().then((names) => {
      const container = document.getElementById("m-team-models-combo");
      if (container) initModelCombo(container, p.models || [], names);
    });
    document.getElementById("m-team-submit").addEventListener("click", async () => {
      try {
        const modelsVal = getComboModels("m-team-models-combo");
        const body = {
          team_id: document.getElementById("m-team-id").value.trim(),
          team_alias: document.getElementById("m-team-alias").value.trim() || null,
          models: modelsVal || ["all-team-models"],
        };
        if (p.team_id) {
          await api("/admin/teams/" + encodeURIComponent(p.team_id), {
            method: "PUT",
            body: JSON.stringify({
              team_alias: body.team_alias,
              models: body.models,
            }),
          });
        } else {
          await api("/admin/teams", { method: "POST", body: JSON.stringify(body) });
        }
        hideModal();
        loadTeams();
      } catch (err) { alert("Error: " + err.message); }
    });
  };

  window._editTeam = async (teamId) => {
    try {
      const data = await api("/admin/teams");
      const t = (data.teams || []).find((x) => x.team_id === teamId);
      if (!t) return;
      // Fetch full team record with models from DB.
      // list_teams doesn't return models, so we pass what we have.
      showCreateTeamModal(t);
    } catch (err) { alert("Error: " + err.message); }
  };

  window._deleteTeam = async (teamId, keyCount) => {
    if (keyCount > 0) {
      alert(`Cannot delete team: ${keyCount} key(s) still assigned.`);
      return;
    }
    if (!confirm(`Delete team "${teamId}"?`)) return;
    try {
      await api("/admin/teams/" + encodeURIComponent(teamId), { method: "DELETE" });
      loadTeams();
    } catch (err) { alert("Error: " + err.message); }
  };


  window._toggleTeamPromptLog = async (teamId, isExcluded) => {
    try {
      await api('/admin/prompt-log/team', {
        method: 'POST',
        body: JSON.stringify({ team_id: teamId, excluded: !isExcluded }),
      });
      loadTeams();
    } catch (err) { alert('Error: ' + err.message); }
  };

  // ── Admin: Logs ──────────────────────────────────────
  let logsPage = 1;
  let logsFilters = {};
  let logsFiltersTimer = null;
  let logsFiltersSetup = false;

  function setupLogsFilters() {
    if (logsFiltersSetup) return;
    logsFiltersSetup = true;
    const table = document.getElementById("logs-table");
    if (!table) return;
    // Event delegation on the static table — filter inputs are in <thead>.
    table.addEventListener("input", (e) => {
      if (!e.target.classList.contains("col-filter")) return;
      clearTimeout(logsFiltersTimer);
      logsFiltersTimer = setTimeout(() => {
        const col = e.target.dataset.col;
        const val = e.target.value.trim();
        if (val) { logsFilters[col] = val; } else { delete logsFilters[col]; }
        logsPage = 1;
        loadLogs();
      }, 400);
    });
    const resetBtn = document.getElementById("btn-reset-logs-filters");
    if (resetBtn) {
      resetBtn.addEventListener("click", () => {
        logsFilters = {};
        logsPage = 1;
        // Clear all filter input values.
        table.querySelectorAll(".col-filter").forEach((inp) => { inp.value = ""; });
        loadLogs();
      });
    }
  }

  async function loadLogs(page) {
    if (page !== undefined) logsPage = page;
    try {
      let url = `/admin/logs?page=${logsPage}&per_page=50`;
      for (const [k, v] of Object.entries(logsFilters)) {
        url += `&${encodeURIComponent(k)}=${encodeURIComponent(v)}`;
      }
      const data = await api(url);
      renderLogsTable(data.logs || []);
      renderLogsPagination(data);
    } catch (err) {
      const tbody = document.getElementById("logs-tbody");
      if (tbody) tbody.innerHTML = `<tr><td colspan="11" class="no-results">Failed to load logs: ${esc(err.message)}</td></tr>`;
    }
  }

  function renderLogsTable(logs) {
    const tbody = document.getElementById("logs-tbody");
    if (!tbody) return;
    if (logs.length === 0) {
      tbody.innerHTML = '<tr><td colspan="12" class="no-results">No matching logs found.</td></tr>';
      return;
    }
    tbody.innerHTML = logs.map((l) => {
        const etype = l.error_type || "";
        const isDebuggable = debugEnabled && l.request_id && (
          etype === "upstream_error" || etype === "provider_error" || etype === "timeout"
        );
        const errorCell = l.error_message
          ? (isDebuggable
            ? '<a href="#" onclick="event.preventDefault();window._showDebugError(\'' + esc(l.request_id) + '\')" style="color:var(--primary);text-decoration:underline;cursor:pointer" title="' + esc(l.error_message) + '">' + esc(etype.substring(0, 20)) + '</a>'
            : '<span style="color:var(--danger)" title="' + esc(l.error_message) + '">' + esc(etype.substring(0, 20)) + '</span>')
          : "-";
        const detailCell = promptLogEnabled && l.request_id
          ? '<button class="btn-small" onclick="window._viewPromptLog(\'' + esc(l.request_id) + '\',\'' + esc(l.key_hash) + '\',\'' + esc(l.team_alias || "") + '\')">View</button>'
          : "-";
        return `<tr>
        <td class="mono">${formatTimestamp(l.created_at)}</td>
        <td>${esc(l.team_alias || l.team_id || "-")}</td>
        <td>${esc(l.key_alias || l.key_name || "-")}</td>
        <td class="mono">${esc(l.model)}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? "Yes" : "No"}</td>
        <td>${l.input_tokens != null ? formatNumber(l.input_tokens) : "-"}</td>
        <td>${l.output_tokens != null ? formatNumber(l.output_tokens) : "-"}</td>
        <td>${l.duration_ms != null ? l.duration_ms + "ms" : "-"}</td>
        <td>${errorCell}</td>
        <td>${detailCell}</td>
      </tr>`;
    }).join("");
  }

  function renderLogsPagination(data) {
    const el = document.getElementById("logs-pagination");
    const pages = Math.ceil(data.total / data.per_page);
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._loadLogsPage(${data.page - 1})">&lt;</button>
      <span>Page ${data.page} of ${pages} (${data.total} logs)</span>
      <button ${data.page >= pages ? "disabled" : ""} onclick="window._loadLogsPage(${data.page + 1})">&gt;</button>
    `;
  }

  window._loadLogsPage = (p) => loadLogs(p);

  // ── Prompt Log Entry Viewer ──────────────────────────
  window._viewPromptLog = async function(requestId, keyHash, teamAlias) {
    const overlay = document.getElementById("modal-overlay");
    const modalEl = overlay ? overlay.querySelector(".modal") : null;
    // Widen modal for JSON viewing.
    if (modalEl) modalEl.style.width = "90vw";
    showModal('<div style="text-align:center;padding:40px">Loading...</div>');
    try {
      const params = new URLSearchParams({ key_hash: keyHash });
      if (teamAlias) params.set("team_alias", teamAlias);
      const data = await api("/admin/prompt-log/entry/" + encodeURIComponent(requestId) + "?" + params);
      const json = JSON.stringify(data, null, 2);
      showModal(
        '<h3 style="margin-bottom:12px">Prompt Log Detail</h3>' +
        '<div style="max-height:72vh;overflow:auto;background:#1e1e2e;color:#cdd6f4;padding:16px;border-radius:8px;font-size:13px;line-height:1.5;white-space:pre-wrap;word-break:break-word;font-family:Menlo,Consolas,monospace">' +
        esc(json) + '</div>'
      );
    } catch (err) {
      showModal('<div style="padding:20px;color:var(--danger)">Failed to load prompt log: ' + esc(err.message) + '</div>');
    }
    // Restore modal width on close.
    const restoreWidth = () => {
      if (modalEl) modalEl.style.width = "";
      overlay.removeEventListener("click", restoreWidth);
    };
    overlay.addEventListener("click", restoreWidth);
  };

  // ── Model Checkbox Combo ──────────────────────────────
  // Renders a custom multi-select dropdown with checkboxes.
  // - "all-team-models" option: when checked, overrides to full access
  // - Individual model checkboxes for fine-grained control
  // - Shows currently selected models in a display area

  function initModelCombo(container, existingModels, allNames) {
    const checked = new Set(existingModels || []);
    const isFullAccess = checked.size === 0 || checked.has("all-team-models");

    // Build HTML
    container.innerHTML = `
      <div class="mcc-display">${isFullAccess ? "all-team-models (Full Access)" : (existingModels || []).map((m) => esc(m)).join(", ") || "No models"}</div>
      <div class="mcc-dropdown hidden">
        <label class="mcc-item mcc-item-all"><input type="checkbox" value="all-team-models" ${isFullAccess ? "checked" : ""}> all-team-models (Full Access)</label>
        <div class="mcc-divider"></div>
        ${allNames.map((n) => `<label class="mcc-item"><input type="checkbox" value="${esc(n)}" ${!isFullAccess && checked.has(n) ? "checked" : ""}> ${esc(n)}</label>`).join("")}
      </div>
    `;

    const display = container.querySelector(".mcc-display");
    const dropdown = container.querySelector(".mcc-dropdown");
    const allCb = container.querySelector('.mcc-item-all input[type="checkbox"]');
    const modelCbs = container.querySelectorAll('.mcc-item:not(.mcc-item-all) input[type="checkbox"]');

    // Toggle dropdown
    display.addEventListener("click", (e) => {
      e.stopPropagation();
      // Close other combos
      document.querySelectorAll(".mcc-dropdown").forEach((d) => {
        if (d !== dropdown) d.classList.add("hidden");
      });
      dropdown.classList.toggle("hidden");
    });

    // Close on outside click
    const closeHandler = (e) => {
      if (!container.contains(e.target)) dropdown.classList.add("hidden");
    };
    document.addEventListener("click", closeHandler);

    // all-team-models checkbox: toggles full access
    allCb.addEventListener("change", () => {
      if (allCb.checked) {
        modelCbs.forEach((cb) => { cb.checked = false; cb.disabled = true; });
      } else {
        modelCbs.forEach((cb) => { cb.disabled = false; });
      }
      refreshDisplay();
    });

    // Individual model checkbox
    modelCbs.forEach((cb) => {
      cb.addEventListener("change", () => {
        // If any individual model is checked, uncheck all-team-models
        const anyChecked = Array.from(modelCbs).some((c) => c.checked);
        if (anyChecked) {
          allCb.checked = false;
          modelCbs.forEach((c) => { c.disabled = false; });
        }
        refreshDisplay();
      });
    });

    // If full access initially, disable individual checkboxes
    if (isFullAccess) {
      modelCbs.forEach((cb) => { cb.disabled = true; });
    }

    function refreshDisplay() {
      if (allCb.checked) {
        display.textContent = "all-team-models (Full Access)";
      } else {
        const selected = Array.from(modelCbs).filter((c) => c.checked).map((c) => c.value);
        display.textContent = selected.length > 0 ? selected.join(", ") : "No models selected";
      }
    }
  }

  // Read the final models selection from a combo container.
  // Returns null (unrestricted) if all-team-models is checked,
  // array of model names if specific models are checked,
  // null if nothing is checked.
  function getComboModels(containerId) {
    const container = document.getElementById(containerId);
    if (!container) return null;
    const allCb = container.querySelector('.mcc-item-all input[type="checkbox"]');
    const modelCbs = container.querySelectorAll('.mcc-item:not(.mcc-item-all) input[type="checkbox"]');
    if (allCb && allCb.checked) return null; // full access = null/unrestricted
    const selected = Array.from(modelCbs).filter((c) => c.checked).map((c) => c.value);
    return selected.length > 0 ? selected : null;
  }

  // ── Helpers ───────────────────────────────────────────
  function esc(s) {
    const d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  function formatDuration(secs) {
    if (secs < 60) return secs + "s";
    if (secs < 3600) return (secs / 60) + "min";
    if (secs < 86400) return (secs / 3600) + "h";
    return (secs / 86400) + "d";
  }

  function formatHMS(secs) {
    secs = Math.round(secs);
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    if (h > 0) return h + "时" + m + "分" + s + "秒";
    if (m > 0) return m + "分" + s + "秒";
    return s + "秒";
  }

  function formatCountdown(secs) {
    if (secs <= 0) return "-";
    secs = Math.round(secs);
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    return h + ":" + String(m).padStart(2, "0") + ":" + String(s).padStart(2, "0");
  }

  function formatNumber(n) {
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + "M";
    if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
    return String(n);
  }

  function formatTimestamp(iso) {
    if (!iso) return "-";
    const d = new Date(iso);
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
           `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }
})();
