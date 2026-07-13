// ── BooMGateway Dashboard SPA ────────────────────────────
(function () {
  "use strict";

  const API = "/dashboard/api";
  let currentUser = null;
  let usageRefreshTimer = null;

  // ── Theme ─────────────────────────────────────────────
  function getTheme() { return document.documentElement.dataset.theme || "light"; }
  function setTheme(t) {
    document.documentElement.dataset.theme = t;
    localStorage.setItem("boom-theme", t);
    updateThemeIcons();
  }
  function toggleTheme() { setTheme(getTheme() === "dark" ? "light" : "dark"); }
  function updateThemeIcons() {
    var dark = getTheme() === "dark";
    document.querySelectorAll(".theme-toggle").forEach(function(btn) {
      var sun = btn.querySelector(".icon-sun");
      var moon = btn.querySelector(".icon-moon");
      if (sun) sun.style.display = dark ? "none" : "block";
      if (moon) moon.style.display = dark ? "block" : "none";
    });
  }
  function isDark() { return getTheme() === "dark"; }

  // ── Toast ─────────────────────────────────────────────
  function showToast(msg, duration) {
    duration = duration || 2500;
    var container = document.getElementById("toast-container");
    if (!container) return;
    var el = document.createElement("div");
    el.className = "toast";
    el.textContent = msg;
    container.appendChild(el);
    setTimeout(function() {
      el.classList.add("toast-out");
      setTimeout(function() { el.remove(); }, 200);
    }, duration);
  }

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

  // Return plan names filtered to type=team (for team modal dropdown).
  async function getTeamPlanNames() {
    try {
      const data = await api("/admin/plans");
      return (data.plans || [])
        .filter((p) => !p.type || p.type === "team")
        .map((p) => p.name);
    } catch { return []; }
  }

  // Invalidate caches after mutations
  function invalidateCaches() { cachedModelNames = null; cachedPlanNames = null; }

  // ── Init ──────────────────────────────────────────────
  document.addEventListener("DOMContentLoaded", () => {
    setupLogin();
    setupLogout();
    setupAdminButtons();
    setupThemeToggle();
    setupLangToggle();
    updateLangToggle();
    updateThemeIcons();
    setupViewportTooltip();
    bindRangeControls();
    initSidebarVersion();
    window.addEventListener("hashchange", () => { onRoute(); onUserRoute(); });
    document.addEventListener("languagechange", () => {
      // Re-render visible dynamic content so t() picks up the new language.
      onRoute();
      onUserRoute();
      // User sidebar title is dynamic (key alias), not in the i18n dict —
      // re-apply after applyI18n() so it isn't stuck on the default label.
      applyUserSidebarTitle();
    });
    checkSession();
  });

  // Fetch /health once on load and inject the version into both sidebars.
  // /health is unauthenticated (liveness endpoint), so this works pre-login.
  async function initSidebarVersion() {
    try {
      const res = await fetch("/health");
      if (!res.ok) return;
      const data = await res.json().catch(() => null);
      if (!data || !data.version) return;
      const v = "v" + String(data.version);
      document.getElementById("sidebar-version-user").textContent = v;
      document.getElementById("sidebar-version-admin").textContent = v;
    } catch {
      // Network/probe failure — leave the placeholder; not worth surfacing.
    }
  }

  // ── Language toggle ───────────────────────────────────
  function setupLangToggle() {
    document.querySelectorAll(".lang-toggle").forEach(function (btn) {
      btn.addEventListener("click", toggleLang);
    });
  }

  function toggleLang() {
    const next = (window.__i18n.currentLang() === "en") ? "zh" : "en";
    window.__i18n.setLang(next);
    updateLangToggle();
  }

  function updateLangToggle() {
    // Show the *other* language's code on the button (the one you'll switch to).
    const current = window.__i18n.currentLang();
    const label = current === "en" ? "中" : "EN";
    document.querySelectorAll(".lang-toggle .lang-current").forEach(function (el) {
      el.textContent = label;
    });
    document.documentElement.setAttribute("lang", current === "en" ? "en" : "zh-CN");
  }

  // ── Viewport-aware tooltip for .cell-tip ──────────────
  // Positions tooltip above or below the element depending on available space.
  function setupViewportTooltip() {
    var tip = document.getElementById("vtip");
    if (!tip) return;
    document.addEventListener("mouseover", function(e) {
      var el = e.target.closest(".cell-tip");
      if (!el || !el.dataset.tip) { tip.classList.remove("show"); return; }
      tip.textContent = el.dataset.tip;
      tip.classList.add("show");
      // Measure after adding to DOM
      var r = el.getBoundingClientRect();
      var tw = tip.offsetWidth;
      var th = tip.offsetHeight;
      var vw = window.innerWidth;
      var vh = window.innerHeight;
      // Default: above, centered
      var top = r.top - th - 8;
      var left = r.left + r.width / 2 - tw / 2;
      // Not enough space above → flip below
      if (top < 4) top = r.bottom + 8;
      // Clamp horizontal
      if (left < 4) left = 4;
      if (left + tw > vw - 4) left = vw - tw - 4;
      // If still off-screen bottom, just clamp
      if (top + th > vh - 4) top = vh - th - 4;
      tip.style.top = top + "px";
      tip.style.left = left + "px";
    });
    document.addEventListener("mouseout", function(e) {
      var el = e.target.closest(".cell-tip");
      if (el) tip.classList.remove("show");
    });
  }

  function setupThemeToggle() {
    document.querySelectorAll(".theme-toggle").forEach(function(btn) {
      btn.addEventListener("click", toggleTheme);
    });
  }

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
      applyUserSidebarTitle();
      document.getElementById("page-dashboard").classList.add("active");
      loadUserData();
      startUsageRefresh();
      onUserRoute();
    }
  }

  // User sidebar title shows the logged-in user's key alias (fallback user_id),
  // served by the backend as currentUser.user_id. It is NOT in the i18n
  // dictionary — this helper is the single source of truth so language
  // switches don't clobber it.
  function applyUserSidebarTitle() {
    const titleEl = document.getElementById("user-sidebar-title");
    if (!titleEl || !currentUser) return;
    titleEl.textContent = currentUser.user_id || t("user.sidebar_title");
  }

  // ── Login ─────────────────────────────────────────────
  let isAdminMode = false;
  function setAdminMode(admin) {
    isAdminMode = admin;
    const userIdGroup = document.getElementById("user-id-group");
    const userIdInput = document.getElementById("user_id");
    const hint = document.getElementById("login-hint");
    const apiKeyInput = document.getElementById("api_key");
    const toggle = document.getElementById("admin-toggle");
    if (admin) {
      userIdInput.value = "admin";
      userIdGroup.classList.remove("hidden");
      hint.textContent = t("login.master_subtitle");
      apiKeyInput.placeholder = t("login.master_key");
      toggle.textContent = t("login.user_link");
    } else {
      userIdInput.value = "";
      userIdGroup.classList.add("hidden");
      hint.textContent = t("login.subtitle");
      apiKeyInput.placeholder = t("login.api_key_placeholder");
      toggle.textContent = t("login.admin_link");
    }
  }

  function setupLogin() {
    document.getElementById("admin-toggle").addEventListener("click", (e) => {
      e.preventDefault();
      setAdminMode(!isAdminMode);
    });
    setAdminMode(false);
    document.getElementById("login-form").addEventListener("submit", async (e) => {
      e.preventDefault();
      const errEl = document.getElementById("login-error");
      errEl.classList.add("hidden");
      const btn = document.getElementById("login-btn");
      btn.disabled = true;
      btn.textContent = t("login.logging_in");
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
          throw new Error(data.error || data.message || t("login.failed"));
        }
        const data = await res.json();
        currentUser = data;
        if (data.api_key) sessionStorage.setItem("boom_chat_api_key", data.api_key);
        navigateToDashboard(data.role);
      } catch (err) {
        errEl.textContent = err.message;
        errEl.classList.remove("hidden");
      } finally {
        btn.disabled = false;
        btn.textContent = t("login.submit");
      }
    });
  }

  // ── Logout ────────────────────────────────────────────
  function setupLogout() {
    document.getElementById("logout-btn").addEventListener("click", doLogout);
    document.getElementById("logout-btn-admin").addEventListener("click", doLogout);
  }

  async function doLogout() {
    sessionStorage.removeItem("boom_chat_api_key");
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
    else if (section === "admin-plans") loadPlans();
    else if (section === "admin-keys") { setupKeysSearch(); loadKeys(); }
    else if (section === "admin-assignments") loadAssignments();
    else if (section === "admin-quota") loadQuota();
    else if (section === "admin-logs") { setupLogsFilters(); loadLogs(); }
    else if (section === "admin-debug") { loadAgentStats(); loadRebalanceMoves(); }
  }

  function sectionFromHash(hash) {
    if (hash.includes("/admin/stats")) return "admin-stats";
    if (hash.includes("/admin/models")) return "admin-models";
    if (hash.includes("/admin/plans")) return "admin-plans";
    if (hash.includes("/admin/keys")) return "admin-keys";
    if (hash.includes("/admin/quota")) return "admin-quota";
    if (hash.includes("/admin/assignments")) return "admin-assignments";
    if (hash.includes("/admin/logs")) return "admin-logs";
    if (hash.includes("/admin/debug")) return "admin-debug";
    return "admin-models";
  }

  // ── Stats ─────────────────────────────────────────────
  function loadStats() {
    loadInflight();
    loadRequestRateStats();
    loadDeployment24hSummary();
  }

  // ── In-Flight ─────────────────────────────────────────
  let inflightTimer = null;

  // 24h per-deployment aggregates — populated on page load and Refresh button
  // only; the 3s setInterval auto-poll does NOT touch this. renderInflightTable
  // reads from this cache so the table shows the last on-demand snapshot.
  let deployment24hSummary = {};

  async function loadDeployment24hSummary() {
    try {
      const data = await api("/admin/stats/deployments/summary");
      if (data && data.error) {
        console.error("loadDeployment24hSummary backend error:", data.error);
        deployment24hSummary = { __error: data.error };
      } else {
        const map = {};
        (data.deployments || []).forEach((d) => { map[d.deployment_id] = d; });
        deployment24hSummary = map;
      }
      loadInflight();
    } catch (err) {
      console.error("loadDeployment24hSummary error:", err);
      deployment24hSummary = { __error: String(err.message || err) };
      loadInflight();
    }
  }

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
      var emptyMsg = t("stats.inflight.no_inflight");
      if (deployment24hSummary && deployment24hSummary.__error) {
        emptyMsg += ' <span style="color:#c00">' + t("stats.inflight.24h_load_failed", { error: esc(String(deployment24hSummary.__error)) }) + "</span>";
      }
      wrap.innerHTML = "<p>" + emptyMsg + "</p>";
      return;
    }

    var errorBanner = "";
    if (deployment24hSummary && deployment24hSummary.__error) {
      errorBanner = '<p style="color:#c00;margin:0 0 6px">' + t("stats.inflight.24h_load_failed", { error: esc(String(deployment24hSummary.__error)) }) + "</p>";
    }

    wrap.innerHTML =
      errorBanner +
      '<table class="data-table"><thead><tr>' +
      "<th>" + t("stats.inflight.col.deployment") + "</th><th>" + t("stats.inflight.col.fc_queue") + "</th><th>" + t("stats.inflight.col.in_reqs") + "</th><th>" + t("stats.inflight.col.in_context") + "</th>" +
      "<th>" + t("stats.inflight.col.24h_reqs") + "</th><th>" + t("stats.inflight.col.avg_in") + "</th><th>" + t("stats.inflight.col.avg_out") + "</th><th>" + t("stats.inflight.col.avg_ttft") + "</th><th>" + t("stats.inflight.col.avg_prefix_hit_rate") + "</th>" +
      "</tr></thead><tbody>" +
      deployments
        .map(function (d) {
          var reqsDisplay = d.in_reqs_max > 0 ? d.in_reqs + " / " + d.in_reqs_max : String(d.in_reqs);
          var ctxDisplay = d.in_context_max > 0 ? d.in_context.toLocaleString() + " / " + d.in_context_max.toLocaleString() : d.in_context.toLocaleString();

          // FC QUEUE tooltip — show queued key aliases (VIP first).
          var fcQueueHtml = String(d.fc_queue);
          if (d.fc_queue > 0 && d.queued_keys && d.queued_keys.length > 0) {
            var items = d.queued_keys.map(function (k) {
              var vipTag = k.is_vip ? "★ " : "";
              return vipTag + esc(k.key_alias || "?");
            });
            fcQueueHtml = '<span class="cell-tip" data-tip="' + items.join("&#10;").replace(/"/g, "&quot;") + '">' + d.fc_queue + '</span>';
          }

          // IN-MODEL REQS tooltip — show per-key request counts.
          var reqsHtml = reqsDisplay;
          if (d.in_reqs > 0 && d.key_stats && d.key_stats.length > 0) {
            var reqItems = d.key_stats.map(function (k) {
              var vipTag = k.is_vip ? "★ " : "";
              return vipTag + esc(k.key_alias || "?") + ": " + k.request_count;
            });
            reqsHtml = '<span class="cell-tip" data-tip="' + reqItems.join("&#10;").replace(/"/g, "&quot;") + '">' + reqsDisplay + '</span>';
          }

          var deployCell = renderDeployCell(d.model, d.deployment_id);

          // 24h aggregates — read from cache populated on page load / Refresh button.
          var s = d.deployment_id ? deployment24hSummary[d.deployment_id] : null;
          function fmtInt(v) { return (v == null) ? "-" : Math.round(v).toLocaleString(); }
          function fmtToken(v) { return (v == null) ? "-" : Math.round(v).toLocaleString(); }
          function fmtTtft(v) { return (v == null) ? "-" : Math.round(v) + "ms"; }

          return (
            "<tr>" +
            "<td>" + deployCell + "</td>" +
            "<td>" + fcQueueHtml + "</td>" +
            "<td>" + reqsHtml + "</td>" +
            "<td>" + ctxDisplay + "</td>" +
            "<td>" + fmtInt(s ? s.total_requests : null) + "</td>" +
            "<td>" + fmtToken(s ? s.avg_input_tokens : null) + "</td>" +
            "<td>" + fmtToken(s ? s.avg_output_tokens : null) + "</td>" +
            "<td>" + fmtTtft(s ? s.avg_ttft_ms : null) + "</td>" +
            "<td>" + fmtPrefixHit(s ? s.avg_prefix_hit_rate : null) + "</td>" +
            "</tr>"
          );
        })
        .join("") +
      "</tbody></table>";
  }

  function startInflightPoll() {
    stopInflightPoll();
    inflightTimer = setInterval(() => {
      loadInflight();
      // Only poll stats that are in 1h mode — non-1h ranges are DB-backed and
      // would be needlessly re-queried every 3s otherwise.
      if (rangeState.rate.range === "1h") loadRequestRateStats();
    }, 3000);
  }

  function stopInflightPoll() {
    if (inflightTimer) {
      clearInterval(inflightTimer);
      inflightTimer = null;
    }
  }

  // Throughput chart colors — cool cyan/teal palette suggesting high traffic
  function throughputBarColor(pct) {
    if (pct > 75) return "linear-gradient(180deg, #06b6d4, #0891b2)"; // cyan-500 → cyan-600
    if (pct > 40) return "linear-gradient(180deg, #22d3ee, #06b6d4)"; // cyan-400 → cyan-500
    return "linear-gradient(180deg, #67e8f9, #22d3ee)";                // cyan-300 → cyan-400
  }

  // ── Range controls (Agent Statistics + Request Rate) ─────
  // Each stats chart remembers its own range + custom-window. Only `range=1h`
  // is served from the in-memory tracker; everything else hits the DB and is
  // excluded from the 3-second polling loop.
  const rangeState = {
    agent: { range: "1h", from: null, to: null },
    rate:  { range: "1h", from: null, to: null },
  };

  function buildStatsUrl(base, target) {
    const s = rangeState[target];
    if (s.range === "custom" && s.from && s.to) {
      return `${base}?range=custom&from=${encodeURIComponent(s.from)}&to=${encodeURIComponent(s.to)}`;
    }
    return `${base}?range=${s.range}`;
  }

  function bindRangeControls() {
    document.querySelectorAll(".range-controls").forEach((controls) => {
      const target = controls.dataset.target;
      controls.querySelectorAll(".btn-range").forEach((btn) => {
        btn.addEventListener("click", () => onRangePick(controls, target, btn.dataset.range));
      });
      const apply = controls.querySelector(".range-apply");
      if (apply) apply.addEventListener("click", () => onRangeApply(controls, target));
    });
  }

  function onRangePick(controls, target, range) {
    const custom = controls.querySelector(".range-custom");
    const note = controls.querySelector(".range-note");
    if (range === "custom") {
      custom.classList.remove("hidden");
      // Pre-fill inputs with last-1h window if empty (local time, matching datetime-local format).
      const fromInput = controls.querySelector(".range-from");
      const toInput = controls.querySelector(".range-to");
      if (!fromInput.value || !toInput.value) {
        const now = new Date();
        const earlier = new Date(now.getTime() - 60 * 60 * 1000);
        fromInput.value = toLocalDatetimeLocal(earlier);
        toInput.value = toLocalDatetimeLocal(now);
      }
      return;
    }
    custom.classList.add("hidden");
    if (note) note.classList.add("hidden");
    controls.querySelectorAll(".btn-range").forEach((b) => {
      b.classList.toggle("active", b.dataset.range === range);
    });
    rangeState[target] = { range, from: null, to: null };
    if (target === "agent") loadAgentStats(); else loadRequestRateStats();
  }

  function onRangeApply(controls, target) {
    const fromInput = controls.querySelector(".range-from").value;
    const toInput = controls.querySelector(".range-to").value;
    const note = controls.querySelector(".range-note");
    if (!fromInput || !toInput) {
      if (note) { note.textContent = t("range.pick_both"); note.classList.remove("hidden"); }
      return;
    }
    const fromMs = new Date(fromInput).getTime();
    const toMs = new Date(toInput).getTime();
    if (!(fromMs > 0 && toMs > 0) || toMs <= fromMs) {
      if (note) { note.textContent = t("range.to_after_from"); note.classList.remove("hidden"); }
      return;
    }
    if (note) note.classList.add("hidden");
    rangeState[target] = {
      range: "custom",
      from: new Date(fromMs).toISOString(),
      to: new Date(toMs).toISOString(),
    };
    controls.querySelectorAll(".btn-range").forEach((b) => {
      b.classList.toggle("active", b.dataset.range === "custom");
    });
    if (target === "agent") loadAgentStats(); else loadRequestRateStats();
  }

  function toLocalDatetimeLocal(d) {
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  // Show ~12 evenly-spaced x-axis labels regardless of bucket count; last bucket is always labeled.
  function shouldShowLabel(events, idx) {
    if (idx === events.length - 1) return true;
    const stride = Math.max(1, Math.ceil(events.length / 12));
    return idx % stride === 0;
  }

  // Format a bucket start timestamp for the x-axis. Backend sends UTC ISO 8601;
  // Date() converts it to the viewer's local timezone, which is what they expect.
  // Bucket size picks the granularity: ≤1h = "HH:MM", longer = "MM-DD HH:MM".
  function formatBucketLabel(isoTs, bucketSecs) {
    const d = new Date(isoTs);
    if (isNaN(d.getTime())) return "?";
    const pad = (n) => String(n).padStart(2, "0");
    const hhmm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
    if (!bucketSecs || bucketSecs <= 3600) return hhmm;
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hhmm}`;
  }

  // ── Request Rate Charts ──────────────────────────────────
  async function loadRequestRateStats() {
    try {
      const data = await api(buildStatsUrl("/admin/stats/request_rate", "rate"));
      renderRequestRateCharts(data.charts || [], data.window);
    } catch (err) {
      console.error("loadRequestRateStats error:", err);
    }
  }

  function renderRequestRateCharts(charts, window) {
    const wrap = document.getElementById("request-rate-wrap");
    if (!wrap) return;
    setRangeWindowNote("rate", window && window.from, window && window.to);
    if (!charts.length) { wrap.innerHTML = "<p>" + t("common.no_records") + "</p>"; return; }

    var html = "";
    charts.forEach(function (chart) {
      var events = chart.events || [];
      if (!events.length) return;
      var isTotal = chart.deployment_id === "_total" || chart.model === "ALL";
      var label = isTotal ? t("stats.rate.all_models") : esc(chart.model);
      // Fixed segment order from the backend (alphabetical deployment_id).
      var segmentOrder = chart.deployments || [];

      if (isTotal) {
        // No segments — render a single-color bar chart.
        var maxCount = Math.max(1, ...events.map(function (e) { return e.count || 0; }));
        var bars = events.map(function (e, idx) {
          var count = e.count || 0;
          var pct = (count / maxCount) * 100;
          var showLabel = shouldShowLabel(events, idx);
          var lbl = formatBucketLabel(e.ts, window ? window.bucket_secs : 0);
          var title = lbl + ": " + t("stats.rebalance.req_count", { n: count });
          return '<div class="rb-bar-col" title="' + esc(title) + '">' +
            '<div class="rb-bar-value' + (count === 0 ? " rb-bar-value-zero" : "") + '">' + count + '</div>' +
            '<div class="rb-bar" style="height:' + Math.max(pct, 1) + '%;background:' + throughputBarColor(pct) + '"></div>' +
            '<div class="rb-bar-label' + (showLabel ? "" : " rb-label-hidden") + '">' + esc(lbl) + '</div>' +
            '</div>';
        }).join("");
        html += '<div class="rb-chart-card rb-chart-card--single">' +
          '<div class="rb-chart-card__label">' + label + '</div>' +
          '<div class="rebalance-chart">' +
          '<div class="rb-y-axis"><span>' + maxCount + '</span><span>0</span></div>' +
          '<div class="rb-bars">' + bars + '</div>' +
          '</div></div>';
      } else {
        // Per-model stacked bar: each bucket is a column; segments stack from
        // bottom (first in segmentOrder) upward. Bar height is the bucket's
        // total relative to the model's max bucket total — so visually taller
        // = busier bucket. Segment height within a bar = count / bucketTotal.
        var bucketTotals = events.map(function (e) {
          return (e.segments || []).reduce(function (s, seg) { return s + (seg.count || 0); }, 0);
        });
        var maxTotal = Math.max(1, ...bucketTotals);

        var bars = events.map(function (e, idx) {
          var segs = e.segments || [];
          var total = bucketTotals[idx];
          var heightPct = (total / maxTotal) * 100;
          var showLabel = shouldShowLabel(events, idx);
          var lbl = formatBucketLabel(e.ts, window ? window.bucket_secs : 0);

          // Tooltip: bucket label, total, then per-deployment lines in the
          // FIXED segment order (not sorted by count — order matches the
          // colored stack so the user can map a color to its deployment).
          var tipLines = [lbl + " · total " + total];
          segmentOrder.forEach(function (depId) {
            var seg = segs.find(function (s) { return s.deployment_id === depId; });
            var c = seg ? (seg.count || 0) : 0;
            var share = total > 0 ? (c / total * 100) : 0;
            tipLines.push(depId + " — " + c + " (" + share.toFixed(1) + "%)");
          });
          var title = esc(tipLines.join("&#10;"));

          // Stack segments bottom-up: reverse so first in order sits at bottom.
          var stackHtml = segmentOrder.slice().reverse().map(function (depId) {
            var seg = segs.find(function (s) { return s.deployment_id === depId; });
            var c = seg ? (seg.count || 0) : 0;
            var segPct = total > 0 ? (c / total * 100) : 0;
            var color = deploymentColor(depId);
            // 0-count segments still render a 1px sliver so the stack ordering
            // is visually consistent across buckets.
            var segHeight = c === 0 ? "1px" : (segPct + "%");
            return '<div class="rb-bar-seg" style="height:' + segHeight + ';background:' + color + '"></div>';
          }).join("");

          return '<div class="rb-bar-col" title="' + title + '">' +
            '<div class="rb-bar-value' + (total === 0 ? " rb-bar-value-zero" : "") + '">' + total + '</div>' +
            '<div class="rb-bar rb-bar-stacked" style="height:' + Math.max(heightPct, 1) + '%">' + stackHtml + '</div>' +
            '<div class="rb-bar-label' + (showLabel ? "" : " rb-label-hidden") + '">' + esc(lbl) + '</div>' +
            '</div>';
        }).join("");

        html += '<div class="rb-chart-card rb-chart-card--stacked">' +
          '<div class="rb-chart-card__label">' + label + '</div>' +
          '<div class="rebalance-chart">' +
          '<div class="rb-y-axis"><span>' + maxTotal + '</span><span>0</span></div>' +
          '<div class="rb-bars">' + bars + '</div>' +
          '</div>' +
          '<div class="rb-legend">' +
            segmentOrder.map(function (depId) {
              return '<span class="rb-legend-item">' +
                '<span class="rb-legend-swatch" style="background:' + deploymentColor(depId) + '"></span>' +
                esc(depId) +
              '</span>';
            }).join("") +
          '</div>' +
          '</div>';
      }
    });

    wrap.innerHTML = html || ("<p>" + t("common.no_records") + "</p>");
  }

  // ── Rebalance Moves (per deployment, lifetime cumulative) ──
  async function loadRebalanceMoves() {
    try {
      const data = await api("/admin/stats/rebalance-moves");
      renderRebalanceMovesChart(data.moves || []);
    } catch (err) {
      console.error("loadRebalanceMoves error:", err);
    }
  }

  function renderRebalanceMovesChart(moves) {
    const wrap = document.getElementById("rebalance-moves-wrap");
    if (!wrap) return;
    if (!moves.length) { wrap.innerHTML = "<p>" + t("debug.rebalance_moves.no_data") + "</p>"; return; }

    moves.sort((a, b) => a.deployment_id.localeCompare(b.deployment_id));

    const maxCount = Math.max(1, ...moves.flatMap((m) => [m.in_count, m.out_count]));
    const cols = moves.map((m) => {
      const outPct = (m.out_count / maxCount) * 100;
      const inPct = (m.in_count / maxCount) * 100;
      const title = esc(m.deployment_id) + " — " +
        t("debug.rebalance_moves.out") + ": " + m.out_count + ", " +
        t("debug.rebalance_moves.in") + ": " + m.in_count;
      return '<div class="rbm-col" title="' + title + '">' +
        '<div class="rbm-bars">' +
        '<div class="rbm-bar rbm-bar-out" style="height:' + Math.max(outPct, 1) + '%"></div>' +
        '<div class="rbm-bar rbm-bar-in"  style="height:' + Math.max(inPct, 1) + '%"></div>' +
        '</div>' +
        '<div class="rbm-label">' + esc(m.deployment_id) + '</div>' +
        '</div>';
    }).join("");

    wrap.innerHTML =
      '<div class="rebalance-moves-chart">' +
      '<div class="rbm-y-axis"><span>' + maxCount + '</span><span>0</span></div>' +
      '<div class="rbm-cols">' + cols + '</div>' +
      '</div>' +
      '<div class="rbm-legend">' +
      '<span class="rbm-legend-item"><span class="rbm-swatch rbm-bar-out"></span>' + t("debug.rebalance_moves.out") + '</span>' +
      '<span class="rbm-legend-item"><span class="rbm-swatch rbm-bar-in"></span>'  + t("debug.rebalance_moves.in")  + '</span>' +
      '</div>';
  }

  // ── Agent Statistics (anthropic share stacked bar) ───────
  async function loadAgentStats() {
    try {
      const data = await api(buildStatsUrl("/admin/stats/agents", "agent"));
      renderAgentStats(data || {});
    } catch (err) {
      console.error("loadAgentStats error:", err);
    }
  }

  function renderAgentStats(data) {
    const wrap = document.getElementById("agent-stats-wrap");
    if (!wrap) return;

    const events = data.events || [];
    const summary = data.summary || {
      total: 0, anthropic: 0, ratio: 0,
      input_tokens_total: 0, input_tokens_anthropic: 0, input_token_ratio: 0,
      output_tokens_total: 0, output_tokens_anthropic: 0, output_token_ratio: 0,
    };
    const window = data.window || null;
    setRangeWindowNote("agent", window && window.from, window && window.to);
    const rangeLabel = rangeState.agent.range === "custom" ? "custom" : rangeState.agent.range;

    const ratioPct = (summary.ratio * 100).toFixed(1);
    const inputRatioPct = (summary.input_token_ratio * 100).toFixed(1);
    const outputRatioPct = (summary.output_token_ratio * 100).toFixed(1);

    // Five summary cards — Requests (Total / Anthropic / Ratio) + Tokens (Input / Output anthropic share).
    const summaryHtml =
      '<div class="agent-summary">' +
        '<div class="agent-summary-card">' +
          '<div class="agent-summary-label">' + t("stats.agent.summary.total") + ' (' + esc(rangeLabel) + ')</div>' +
          '<div class="agent-summary-value">' + summary.total.toLocaleString() + '</div>' +
        '</div>' +
        '<div class="agent-summary-card">' +
          '<div class="agent-summary-label">' + t("stats.agent.summary.anthropic") + '</div>' +
          '<div class="agent-summary-value" style="color:#10b981">' + summary.anthropic.toLocaleString() + '</div>' +
        '</div>' +
        '<div class="agent-summary-card">' +
          '<div class="agent-summary-label">' + t("stats.agent.summary.share") + '</div>' +
          '<div class="agent-summary-value" style="color:#10b981">' + ratioPct + '%</div>' +
        '</div>' +
        '<div class="agent-summary-card">' +
          '<div class="agent-summary-label">' + t("stats.agent.summary.input_anthropic") + '</div>' +
          '<div class="agent-summary-value" style="color:#10b981">' + summary.input_tokens_anthropic.toLocaleString() +
            ' <span style="font-size:0.7em;color:#6b7280">/ ' + summary.input_tokens_total.toLocaleString() + ' (' + inputRatioPct + '%)</span></div>' +
        '</div>' +
        '<div class="agent-summary-card">' +
          '<div class="agent-summary-label">' + t("stats.agent.summary.output_anthropic") + '</div>' +
          '<div class="agent-summary-value" style="color:#10b981">' + summary.output_tokens_anthropic.toLocaleString() +
            ' <span style="font-size:0.7em;color:#6b7280">/ ' + summary.output_tokens_total.toLocaleString() + ' (' + outputRatioPct + '%)</span></div>' +
        '</div>' +
      '</div>';

    if (!events.length || summary.total === 0) {
      wrap.innerHTML = summaryHtml + '<p class="loading" style="margin-top:1rem">' + t("common.no_records") + '</p>';
      return;
    }

    const bucketSecs = window ? window.bucket_secs : 0;
    const requestChart = renderAgentBarChart(events, "total", "anthropic", t("stats.agent.chart.requests"), bucketSecs, (v) => v.toLocaleString());
    const inputChart = renderAgentBarChart(events, "input_tokens_total", "input_tokens_anthropic", t("stats.agent.chart.input_tokens"), bucketSecs, (v) => v.toLocaleString());
    const outputChart = renderAgentBarChart(events, "output_tokens_total", "output_tokens_anthropic", t("stats.agent.chart.output_tokens"), bucketSecs, (v) => v.toLocaleString());

    const legendHtml =
      '<div class="agent-legend">' +
        '<span class="agent-legend-item"><span class="agent-legend-swatch agent-legend-anthropic"></span>Anthropic (/v1/messages)</span>' +
        '<span class="agent-legend-item"><span class="agent-legend-swatch agent-legend-other"></span>Other (/v1/chat/completions, etc.)</span>' +
      '</div>';

    wrap.innerHTML = summaryHtml +
      '<div style="margin-top:1rem"><div style="font-weight:600;margin-bottom:0.25rem">' + t("stats.agent.chart.requests") + '</div>' + requestChart + '</div>' +
      '<div style="margin-top:1.5rem"><div style="font-weight:600;margin-bottom:0.25rem">' + t("stats.agent.chart.input_tokens") + '</div>' + inputChart + '</div>' +
      '<div style="margin-top:1.5rem"><div style="font-weight:600;margin-bottom:0.25rem">' + t("stats.agent.chart.output_tokens") + '</div>' + outputChart + '</div>' +
      legendHtml;
  }

  // Render one stacked-bar chart (green anthropic segment over gray other segment).
  // `totalKey` / `anthropicKey` select which fields to read from each event.
  // `fmt` formats the raw number for the bar value label and tooltip.
  function renderAgentBarChart(events, totalKey, anthropicKey, label, bucketSecs, fmt) {
    const maxTotal = Math.max(1, ...events.map((e) => Number(e[totalKey] || 0)));
    const bars = events.map((e, idx) => {
      const total = Number(e[totalKey] || 0);
      const anthropic = Number(e[anthropicKey] || 0);
      const other = total - anthropic;
      const totalPct = (total / maxTotal) * 100;
      const anthropicPctOfTotal = total > 0 ? (anthropic / total) * 100 : 0;
      const showLabel = shouldShowLabel(events, idx);
      const lbl = formatBucketLabel(e.ts, bucketSecs);
      const ratioTxt = total > 0 ? ((anthropic / total) * 100).toFixed(0) : "0";
      const title =
        lbl + " — " + label + ": " + fmt(total) +
        ", anthropic: " + fmt(anthropic) + " (" + ratioTxt + "%), other: " + fmt(other);
      return '<div class="rb-bar-col" title="' + esc(title) + '">' +
        '<div class="rb-bar-value' + (total === 0 ? " rb-bar-value-zero" : "") + '">' + fmt(total) + '</div>' +
        '<div class="agent-bar" style="height:' + Math.max(totalPct, 1) + '%">' +
          (other > 0 ? '<div class="agent-bar-other" style="flex: ' + (100 - anthropicPctOfTotal) + '"></div>' : '') +
          (anthropic > 0 ? '<div class="agent-bar-anthropic" style="flex: ' + anthropicPctOfTotal + '"></div>' : '') +
        '</div>' +
        '<div class="rb-bar-label' + (showLabel ? "" : " rb-label-hidden") + '">' +
          esc(lbl) +
        '</div>' +
      '</div>';
    }).join("");

    return '<div class="rebalance-chart">' +
      '<div class="rb-y-axis"><span>' + fmt(maxTotal) + '</span><span>0</span></div>' +
      '<div class="rb-bars">' + bars + '</div>' +
    '</div>';
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
    else if (section === "user-chat") initChatPage();
  }

  function userSectionFromHash(hash) {
    if (hash.includes("/dashboard/chat")) return "user-chat";
    if (hash.includes("/dashboard/logs")) return "user-logs";
    return "user-overview";
  }

  // ── User: Chat ───────────────────────────────────────
  const CHAT_KEY_STORAGE = "boom_chat_api_key";
  const CHAT_PROMPT_STORAGE = "boom_chat_system_prompt";
  const CHAT_TEMPERATURE_STORAGE = "boom_chat_temperature";
  const CHAT_MAX_TOKENS_STORAGE = "boom_chat_max_tokens";
  const CHAT_TOOL_FILTER_STORAGE = "boom_chat_tool_filter";
  const DEFAULT_TEMPERATURE = 0.7;
  const DEFAULT_MAX_TOKENS = 32768;
  let chatModelsLoaded = false;
  let chatHistory = []; // [{role, content}]
  let chatQueueTimer = null;
  let chatAbort = null;
  let chatFirstTokenArrived = false;

  function getChatKey() { return sessionStorage.getItem(CHAT_KEY_STORAGE); }

  // Per-key chat history in localStorage. Keyed by a short prefix of the API
  // key so different keys' histories don't bleed into each other. Never sent
  // to the server.
  function chatHistoryStorageKey() {
    const k = getChatKey() || "anon";
    return "boom_chat_history_" + k.slice(0, 12);
  }
  function loadChatHistory() {
    try {
      const raw = localStorage.getItem(chatHistoryStorageKey());
      if (!raw) return [];
      const arr = JSON.parse(raw);
      return Array.isArray(arr) ? arr.filter((m) => m && m.role && typeof m.content === "string") : [];
    } catch { return []; }
  }
  function saveChatHistory() {
    try {
      localStorage.setItem(chatHistoryStorageKey(), JSON.stringify(chatHistory));
    } catch {} // quota exceeded etc — silently drop
  }
  function clearChatHistory() {
    chatHistory = [];
    try { localStorage.removeItem(chatHistoryStorageKey()); } catch {}
  }
  function getChatPrompt() { return localStorage.getItem(CHAT_PROMPT_STORAGE) || ""; }
  function getChatTemperature() {
    const v = parseFloat(localStorage.getItem(CHAT_TEMPERATURE_STORAGE));
    return Number.isFinite(v) ? v : DEFAULT_TEMPERATURE;
  }
  function getChatMaxTokens() {
    const v = parseInt(localStorage.getItem(CHAT_MAX_TOKENS_STORAGE), 10);
    return Number.isFinite(v) && v > 0 ? v : DEFAULT_MAX_TOKENS;
  }
  function getChatToolFilter() {
    const v = localStorage.getItem(CHAT_TOOL_FILTER_STORAGE);
    return v === null ? true : v === "1"; // default ON
  }

  function initChatPage() {
    const sendBtn = document.getElementById("chat-send-btn");
    const input = document.getElementById("chat-input");
    const settingsBtn = document.getElementById("chat-settings-btn");
    if (!sendBtn.dataset.wired) {
      sendBtn.dataset.wired = "1";
      sendBtn.addEventListener("click", sendChatMessage);
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && !e.shiftKey) {
          e.preventDefault();
          sendChatMessage();
        }
      });
      settingsBtn.addEventListener("click", showChatSettingsModal);
      // Auto-grow textarea.
      input.addEventListener("input", () => {
        input.style.height = "auto";
        input.style.height = Math.min(input.scrollHeight, 160) + "px";
      });
    }
    if (!chatModelsLoaded) loadChatModels();
    // Load persisted history from localStorage (per-key).
    if (chatHistory.length === 0) chatHistory = loadChatHistory();
    loadChatPlan();
    renderChatHistory();
    updateParamChip();
  }

  async function loadChatModels() {
    const sel = document.getElementById("chat-model");
    if (!sel) return;
    const key = getChatKey();
    if (!key) {
      sel.innerHTML = `<option value="">${t("chat.no_key")}</option>`;
      return;
    }
    try {
      const res = await fetch("/v1/models", { headers: { Authorization: "Bearer " + key } });
      if (res.status === 401) { showLogin(); return; }
      if (!res.ok) throw new Error("HTTP " + res.status);
      const data = await res.json();
      const names = (data.data || []).map((m) => m.id);
      const prev = sel.value;
      sel.innerHTML = names.length
        ? names.map((n) => `<option value="${esc(n)}">${esc(n)}</option>`).join("")
        : `<option value="">${t("chat.no_models")}</option>`;
      if (prev && names.includes(prev)) sel.value = prev;
      else if (names.length) sel.value = names[0];
      chatModelsLoaded = true;
    } catch (err) {
      sel.innerHTML = `<option value="">${t("chat.models_failed", { error: esc(String(err.message || err)) })}</option>`;
    }
  }

  // ── Plan + Usage card ─────────────────────────────────
  async function loadChatPlan() {
    const el = document.getElementById("chat-plan-bar");
    if (!el) return;
    try {
      const [plan, usage] = await Promise.all([api("/user/plan"), api("/user/usage")]);
      const planName = plan.plan_name || t("chat.no_plan");
      const concUsed = usage.concurrency || 0;
      const concLimit = plan.concurrency_limit;
      const windows = (usage.windows || []).slice().sort((a, b) => a.window_secs - b.window_secs);
      // RPM is the 60s window if present.
      const rpmWindow = windows.find((w) => w.window_secs === 60);

      const fmtLimit = (l) => (l == null || l === 0 ? "∞" : String(l));
      const pct = (u, l) => (l == null || l === 0 ? 0 : Math.min(100, Math.round((u / l) * 100)));

      // Concurrency meter.
      const concHtml = meterHtml(
        t("chat.concurrency"),
        `${concUsed} / ${fmtLimit(concLimit)}`,
        pct(concUsed, concLimit)
      );
      // RPM meter (60s counts dim — backend folds plan.rpm_limit into the
      // 60s window's counts limit. Old code read rpmWindow.count / plan.rpm_limit
      // which no longer exist; both come from dims.counts now.
      const rpmCounts = rpmWindow && rpmWindow.dims && rpmWindow.dims.counts;
      const rpmUsed = rpmCounts ? Number(rpmCounts.current || 0) : 0;
      const rpmLimit = rpmCounts ? Number(rpmCounts.limit || 0) : null;
      const rpmHtml = meterHtml(
        "RPM",
        `${rpmUsed} / ${fmtLimit(rpmLimit)}`,
        pct(rpmUsed, rpmLimit)
      );
      // Other windows (skip 60s — already shown as RPM). Each window may have
      // multiple dims (counts/tokens/costs); show the configured one. If
      // multiple, render them stacked — but for the chat meter strip we pick
      // the most informative one per window (tokens > costs > counts).
      const otherWindows = windows.filter((w) => w.window_secs !== 60);
      const otherHtml = otherWindows.map((w) => {
        const label = formatDuration(w.window_secs);
        const dims = w.dims || {};
        // Prefer tokens for TPM-style windows, then costs, then counts.
        const dimKey = dims.tokens ? "tokens"
                     : dims.costs  ? "costs"
                     : dims.counts ? "counts"
                     : null;
        if (!dimKey) return "";
        const d = dims[dimKey];
        let used, limit;
        if (dimKey === "costs") {
          used = Number(d.current_micros || 0);
          limit = Number(d.limit_micros || 0);
          const valTxt = "$" + (d.current || "0") + " / " + (limit > 0 ? "$" + (d.limit || "0") : "∞");
          return meterHtml(label, valTxt, pct(used, limit));
        }
        used = Number(d.current || 0);
        limit = Number(d.limit || 0);
        return meterHtml(label, `${used} / ${fmtLimit(limit)}`, pct(used, limit));
      }).join("");

      el.innerHTML = `
        <div class="chat-plan-head">
          <span class="chat-plan-label">${t("chat.plan_label")}</span>
          <span class="chat-plan-name">${esc(planName)}</span>
        </div>
        <div class="chat-plan-meters">${concHtml}${rpmHtml}${otherHtml}</div>
      `;
      el.classList.remove("hidden");
    } catch {
      el.innerHTML = `<span class="chat-plan-error">${t("chat.plan_load_failed")}</span>`;
      el.classList.remove("hidden");
    }
  }

  function meterHtml(label, valueText, percent) {
    const tone = percent >= 90 ? "danger" : (percent >= 70 ? "warn" : "ok");
    return `
      <div class="chat-meter">
        <div class="chat-meter-head"><span class="chat-meter-label">${esc(label)}</span><span class="chat-meter-value">${esc(valueText)}</span></div>
        <div class="chat-meter-track"><div class="chat-meter-fill tone-${tone}" style="width:${percent}%"></div></div>
      </div>`;
  }

  function updateParamChip() {
    const el = document.getElementById("chat-param-chip");
    if (!el) return;
    const temp = getChatTemperature();
    const mt = getChatMaxTokens();
    const tempStr = Number.isInteger(temp) ? String(temp) : temp.toFixed(2).replace(/0$/, "");
    el.textContent = `T=${tempStr} · max=${mt >= 1024 ? (mt / 1024) + "k" : mt}`;
    el.title = t("chat.param_chip_tip");
  }

  function renderChatHistory() {
    const box = document.getElementById("chat-messages");
    if (!box) return;
    if (chatHistory.length === 0) {
      box.innerHTML = `<p class="chat-empty">${t("chat.empty")}</p>`;
      return;
    }
    box.innerHTML = chatHistory.map((m) => chatBubbleHtml(m.role, m.content)).join("");
    box.scrollTop = box.scrollHeight;
  }

  function chatBubbleHtml(role, content) {
    const cls = role === "user" ? "chat-bubble-user" : "chat-bubble-assistant";
    const aligned = role === "user" ? "chat-row-user" : "chat-row-assistant";
    const shown = role === "assistant" ? renderAssistantContent(content) : esc(content);
    return `<div class="${aligned}"><div class="chat-bubble ${cls}">${shown}</div></div>`;
  }

  // Render assistant content based on the tool-filter toggle:
  //   - ON  : strip tool-call tags entirely (filterToolTags).
  //   - OFF : keep them but wrap in a muted .chat-tool-block so they read as
  //           dimmed metadata, like Claude Code / Cursor style.
  function renderAssistantContent(raw) {
    if (!raw) return "";
    return getChatToolFilter() ? esc(filterToolTags(raw)) : highlightToolTags(raw);
  }

  // Stream-time variant: same rendering rules, but during streaming an opening
  // tag may not yet have its closer. When filter is ON we strip the partial
  // opener so it never flashes; when OFF we still dim it (it'll be rewrapped
  // cleanly once the closer arrives).
  function renderStreamContent(buf) {
    if (getChatToolFilter()) {
      return esc(filterToolTags(buf));
    }
    return highlightToolTags(buf);
  }

  // Escape, then wrap tool-call tag blocks in a muted span. Operates on the
  // escaped string so no raw HTML from the model ever reaches the DOM.
  function highlightToolTags(raw) {
    const e = esc(raw || "");
    const wrap = (s) => '<span class="chat-tool-block">' + s + "</span>";
    return e
      .replace(/&lt;tool&gt;[\s\S]*?&lt;\/tool&gt;/gi, wrap)
      .replace(/&lt;function_calls&gt;[\s\S]*?&lt;\/function_calls&gt;/gi, wrap)
      .replace(/&lt;antml:invoke[\s\S]*?&lt;\/antml:invoke&gt;/gi, wrap)
      .replace(/&lt;antml:parameter[\s\S]*?&lt;\/antml:parameter&gt;/gi, wrap)
      .replace(/(&lt;\/?antml:[^&]*?&gt;)/gi, wrap)
      // Unclosed opener at end-of-buffer (streaming): dim it too.
      .replace(/&lt;tool(\s[^&]*?)?&gt;[^&]*$/gi, wrap)
      .replace(/&lt;function_calls(\s[^&]*?)?&gt;[^&]*$/gi, wrap)
      .replace(/&lt;antml:invoke(\s[^&]*?)?&gt;[^&]*$/gi, wrap)
      .replace(/&lt;antml:parameter(\s[^&]*?)?&gt;[^&]*$/gi, wrap)
      .replace(/\n{3,}/g, "\n\n")
      .trim();
  }

  async function sendChatMessage() {
    const input = document.getElementById("chat-input");
    const sendBtn = document.getElementById("chat-send-btn");
    const sendLabel = sendBtn.querySelector("span");
    const text = (input.value || "").trim();
    if (!text) return;
    const key = getChatKey();
    if (!key) { showLogin(); return; }
    const model = document.getElementById("chat-model").value;
    if (!model) { showToast(t("chat.no_model_selected")); return; }

    input.value = "";
    input.style.height = "";
    sendBtn.disabled = true;
    if (sendLabel) sendLabel.textContent = t("chat.sending");

    // Build messages: optional system prompt + history + new user message.
    const sysPrompt = getChatPrompt();
    const messages = [];
    if (sysPrompt) messages.push({ role: "system", content: sysPrompt });
    for (const m of chatHistory) messages.push(m);
    messages.push({ role: "user", content: text });
    chatHistory.push({ role: "user", content: text });
    chatHistory.push({ role: "assistant", content: "" });
    saveChatHistory();

    const body = {
      model,
      messages,
      stream: true,
      temperature: getChatTemperature(),
      max_tokens: getChatMaxTokens(),
    };

    const box = document.getElementById("chat-messages");
    if (box.querySelector(".chat-empty")) box.innerHTML = "";
    const row = document.createElement("div");
    row.className = "chat-row-user";
    row.innerHTML = `<div class="chat-bubble chat-bubble-user">${esc(text)}</div>`;
    box.appendChild(row);
    const aRow = document.createElement("div");
    aRow.className = "chat-row-assistant";
    const aBubble = document.createElement("div");
    aBubble.className = "chat-bubble chat-bubble-assistant";
    aBubble.innerHTML = `<span class="chat-cursor">▌</span>`;
    aRow.appendChild(aBubble);
    box.appendChild(aRow);
    box.scrollTop = box.scrollHeight;

    chatFirstTokenArrived = false;
    startQueuePolling(model);
    chatAbort = new AbortController();
    let buf = "";
    try {
      const res = await fetch("/v1/chat/completions", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: "Bearer " + key,
        },
        body: JSON.stringify(body),
        signal: chatAbort.signal,
      });
      if (!res.ok) {
        const errBody = await res.json().catch(() => ({}));
        throw new Error(errBody.error?.message || errBody.error || errBody.message || ("HTTP " + res.status));
      }
      if (!res.body) throw new Error("No response body");
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let acc = "";
      let done = false;
      while (!done) {
        const { value, done: rDone } = await reader.read();
        if (rDone) break;
        acc += decoder.decode(value, { stream: true });
        const lines = acc.split("\n");
        acc = lines.pop() || "";
        for (const raw of lines) {
          const line = raw.trim();
          if (!line || !line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (payload === "[DONE]") { done = true; break; }
          try {
            const evt = JSON.parse(payload);
            const delta = evt.choices?.[0]?.delta?.content;
            if (delta) {
              if (!chatFirstTokenArrived) {
                chatFirstTokenArrived = true;
                stopQueuePolling();
              }
              buf += delta;
              // Re-render full buffer on each delta — tags can span chunks,
              // and when filter is OFF we want the dimmed style to apply live.
              aBubble.innerHTML = renderStreamContent(buf) + '<span class="chat-cursor">▌</span>';
              box.scrollTop = box.scrollHeight;
            }
          } catch {}
        }
      }
      // Stream ended — finalize. Store RAW in history (so toggling filter re-renders).
      chatHistory[chatHistory.length - 1].content = buf;
      aBubble.innerHTML = renderAssistantContent(buf);
    } catch (err) {
      if (err.name === "AbortError") {
        // Keep partial — finalize what we have.
        chatHistory[chatHistory.length - 1].content = buf;
        aBubble.innerHTML = renderAssistantContent(buf);
      } else {
        aBubble.classList.add("chat-bubble-error");
        aBubble.textContent = String(err.message || err);
        showToast(String(err.message || err));
        if (chatHistory.length && chatHistory[chatHistory.length - 1].content === "") {
          chatHistory.pop();
        }
      }
    } finally {
      stopQueuePolling();
      sendBtn.disabled = false;
      if (sendLabel) sendLabel.textContent = t("chat.send");
      saveChatHistory();
      loadChatPlan();
      chatAbort = null;
    }
  }

  function filterToolTags(s) {
    let r = s
      .replace(/<tool>[\s\S]*?<\/tool>/gi, "")
      .replace(/<function_calls>[\s\S]*?<\/function_calls>/gi, "")
      .replace(/<antml:invoke[\s\S]*?<\/antml:invoke>/gi, "")
      .replace(/<antml:parameter[\s\S]*?<\/antml:parameter>/gi, "")
      .replace(/<\/?antml:[^>]*>/gi, "");
    // During streaming, an opening tag may have arrived without its closer yet.
    // Strip from the last unclosed opener to end-of-buffer so partial tag text
    // never flashes in the bubble. Once the closer arrives, the rule above
    // removes the whole block cleanly.
    r = r.replace(/<tool(\s[^>]*)?>[^<>]*$/gi, "")
          .replace(/<function_calls(\s[^>]*)?>[^<>]*$/gi, "")
          .replace(/<antml:invoke(\s[^>]*)?>[^<>]*$/gi, "")
          .replace(/<antml:parameter(\s[^>]*)?>[^<>]*$/gi, "");
    return r.replace(/\n{3,}/g, "\n\n").trim();
  }

  function startQueuePolling(model) {
    stopQueuePolling();
    const el = document.getElementById("chat-queue-status");
    if (!el) return;
    el.classList.remove("hidden");
    const tick = async () => {
      try {
        const data = await api("/user/request-status");
        const reqs = (data.requests || []).filter((r) => r.model === model || !r.model);
        if (reqs.length === 0) {
          el.classList.add("hidden");
          return;
        }
        el.classList.remove("hidden");
        const r = reqs[0];
        if (r.status === "waiting") {
          el.innerHTML = `<span class="chat-queue-dot"></span>${esc(t("chat.queue_waiting", { ahead: r.ahead || 0, secs: (r.wait_time_secs || 0).toFixed(1) }))}`;
        } else {
          el.innerHTML = `<span class="chat-queue-dot dot-active"></span>${esc(t("chat.queue_processing", { parallel: r.parallel_count || 0, secs: (r.processing_secs || 0).toFixed(1) }))}`;
        }
      } catch {
        el.classList.add("hidden");
      }
    };
    tick();
    chatQueueTimer = setInterval(tick, 500);
  }

  function stopQueuePolling() {
    if (chatQueueTimer) { clearInterval(chatQueueTimer); chatQueueTimer = null; }
    const el = document.getElementById("chat-queue-status");
    if (el) el.classList.add("hidden");
  }

  // ── Settings modal (system prompt + temperature + max_tokens + tool filter) ─
  function showChatSettingsModal() {
    const currentPrompt = getChatPrompt();
    const currentTemp = getChatTemperature();
    const currentMax = getChatMaxTokens();
    const currentFilter = getChatToolFilter();
    showModal(`
      <h3>${t("chat.settings_title")}</h3>

      <div class="chat-settings-section">
        <label class="chat-settings-label" for="chat-prompt-input">${t("chat.system_prompt_title")}</label>
        <p class="modal-hint">${t("chat.system_prompt_hint")}</p>
        <textarea id="chat-prompt-input" rows="5" placeholder="${esc(t("chat.system_prompt_placeholder"))}">${esc(currentPrompt)}</textarea>
      </div>

      <div class="chat-settings-section">
        <label class="chat-settings-label" for="chat-temp-input">${t("chat.temperature_label")}</label>
        <p class="modal-hint">${t("chat.temperature_hint")}</p>
        <div class="chat-slider-row">
          <input type="range" id="chat-temp-slider" min="0" max="2" step="0.05" value="${currentTemp}">
          <input type="number" id="chat-temp-input" class="chat-num-input" min="0" max="2" step="0.05" value="${currentTemp}">
        </div>
      </div>

      <div class="chat-settings-section">
        <label class="chat-settings-label" for="chat-max-input">${t("chat.max_tokens_label")}</label>
        <p class="modal-hint">${t("chat.max_tokens_hint")}</p>
        <div class="chat-slider-row">
          <input type="range" id="chat-max-slider" min="1024" max="131072" step="1024" value="${currentMax}">
          <input type="number" id="chat-max-input" class="chat-num-input" min="1024" max="131072" step="1024" value="${currentMax}">
        </div>
      </div>

      <div class="chat-settings-section">
        <label class="chat-settings-toggle">
          <input type="checkbox" id="chat-filter-checkbox" ${currentFilter ? "checked" : ""}>
          <span class="chat-toggle-track"><span class="chat-toggle-thumb"></span></span>
          <span class="chat-toggle-label">
            <strong>${t("chat.tool_filter_label")}</strong>
            <span class="modal-hint">${t("chat.tool_filter_hint")}</span>
          </span>
        </label>
      </div>

      <div class="chat-settings-section chat-settings-danger">
        <label class="chat-settings-label">${t("chat.history_label")}</label>
        <p class="modal-hint">${t("chat.history_hint")}</p>
        <button class="btn-danger" id="chat-clear-history-btn">${t("chat.clear_history")}</button>
      </div>

      <div class="modal-actions">
        <button class="btn-danger" id="chat-reset-btn">${t("chat.settings_reset")}</button>
        <button class="btn-secondary" id="chat-cancel-btn">${t("action.cancel")}</button>
        <button class="btn-primary" id="chat-save-btn">${t("chat.settings_save")}</button>
      </div>
    `);
    // Wire slider ↔ number input sync.
    const tempSlider = document.getElementById("chat-temp-slider");
    const tempInput = document.getElementById("chat-temp-input");
    tempSlider.addEventListener("input", () => { tempInput.value = tempSlider.value; });
    tempInput.addEventListener("input", () => {
      const v = parseFloat(tempInput.value);
      if (Number.isFinite(v)) tempSlider.value = Math.max(0, Math.min(2, v));
    });
    const maxSlider = document.getElementById("chat-max-slider");
    const maxInput = document.getElementById("chat-max-input");
    maxSlider.addEventListener("input", () => { maxInput.value = maxSlider.value; });
    maxInput.addEventListener("input", () => {
      const v = parseInt(maxInput.value, 10);
      if (Number.isFinite(v) && v > 0) maxSlider.value = Math.max(1024, Math.min(131072, v));
    });

    document.getElementById("chat-cancel-btn").addEventListener("click", hideModal);
    document.getElementById("chat-save-btn").addEventListener("click", () => {
      const prompt = document.getElementById("chat-prompt-input").value;
      if (prompt && prompt.trim()) localStorage.setItem(CHAT_PROMPT_STORAGE, prompt);
      else localStorage.removeItem(CHAT_PROMPT_STORAGE);
      const temp = parseFloat(tempInput.value);
      if (Number.isFinite(temp) && temp >= 0 && temp <= 2) {
        localStorage.setItem(CHAT_TEMPERATURE_STORAGE, String(temp));
      }
      const max = parseInt(maxInput.value, 10);
      if (Number.isFinite(max) && max > 0) {
        localStorage.setItem(CHAT_MAX_TOKENS_STORAGE, String(max));
      }
      const filter = document.getElementById("chat-filter-checkbox").checked;
      localStorage.setItem(CHAT_TOOL_FILTER_STORAGE, filter ? "1" : "0");
      hideModal();
      updateParamChip();
      // Re-render existing assistant bubbles since filter toggle changes display.
      renderChatHistory();
      showToast(t("chat.settings_saved"));
    });
    document.getElementById("chat-reset-btn").addEventListener("click", () => {
      localStorage.removeItem(CHAT_PROMPT_STORAGE);
      localStorage.removeItem(CHAT_TEMPERATURE_STORAGE);
      localStorage.removeItem(CHAT_MAX_TOKENS_STORAGE);
      localStorage.removeItem(CHAT_TOOL_FILTER_STORAGE);
      document.getElementById("chat-prompt-input").value = "";
      tempSlider.value = DEFAULT_TEMPERATURE; tempInput.value = DEFAULT_TEMPERATURE;
      maxSlider.value = DEFAULT_MAX_TOKENS; maxInput.value = DEFAULT_MAX_TOKENS;
      document.getElementById("chat-filter-checkbox").checked = true;
      showToast(t("chat.settings_reset_done"));
    });
    document.getElementById("chat-clear-history-btn").addEventListener("click", () => {
      if (!confirm(t("chat.confirm_clear_history"))) return;
      clearChatHistory();
      renderChatHistory();
      hideModal();
      showToast(t("chat.history_cleared"));
    });
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
      wrap.innerHTML = `<p class="error-msg">${t("logs.failed", { message: esc(err.message) })}</p>`;
    }
  }

  function renderUserLogsTable(logs) {
    const wrap = document.getElementById("user-logs-table-wrap");
    if (logs.length === 0) {
      wrap.innerHTML = "<p>" + t("logs.user_empty") + "</p>";
      return;
    }
    wrap.innerHTML = `<table>
      <tr><th>${t("logs.col.time")}</th><th>${t("logs.col.ip")}</th><th>${t("logs.col.model")}</th><th>${t("logs.col.path")}</th><th>${t("logs.col.status")}</th><th>${t("logs.col.stream")}</th><th>${t("logs.col.in_out")}</th><th>${t("logs.col.prefix_hit_rate")}</th><th>${t("logs.col.duration")}</th><th>${t("logs.col.error")}</th></tr>
      ${logs.map((l) => {
        // Prefix hit rate = cached_tokens / input_tokens * 100, truncated to
        // 1 decimal and capped at 99.9 (no rounding to 100%). "-" if missing.
        var kvCell = (l.cached_tokens != null && l.input_tokens > 0)
          ? esc(fmtPrefixHit(l.cached_tokens / l.input_tokens * 100))
          : "-";
        var inOutCell = (l.input_tokens != null || l.output_tokens != null)
          ? formatNumber(l.input_tokens) + " / " + formatNumber(l.output_tokens)
          : "- / -";
        return `<tr>
        <td class="mono">${formatTimestamp(l.created_at)}</td>
        <td class="mono">${esc(l.client_ip || "-")}</td>
        <td class="mono">${esc(l.model)}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? t("common.yes") : t("common.no")}</td>
        <td class="mono">${inOutCell}</td>
        <td class="mono">${kvCell}</td>
        <td>${l.duration_ms != null ? l.duration_ms + "ms" : "-"}</td>
        <td>${l.error_message ? '<span style="color:var(--danger)" title="' + esc(l.error_message) + '">' + esc((l.error_type || "").substring(0, 20)) + '</span>' : "-"}</td>
      </tr>`}).join("")}
    </table>`;
  }

  function renderUserLogsPagination(data) {
    const el = document.getElementById("user-logs-pagination");
    if (!el) return;
    const pages = Math.ceil(data.total / data.per_page);
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._loadUserLogsPage(${data.page - 1})">&lt;</button>
      <span>${t("common.page_of", { page: data.page, total: pages, count: data.total, unit: t("logs.title") })}</span>
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
      el.innerHTML = "<p>" + t("plans.using_default") + "</p>";
      return;
    }
    const limits = [];
    if (plan.concurrency_limit) limits.push(t("plan.limits.concurrency", { n: plan.concurrency_limit }));
    if (plan.total_token_limit) limits.push(t("plan.limits.total_token", { n: plan.total_token_limit }));
    if (plan.total_cost_limit != null) limits.push(t("plan.limits.total_cost", { n: plan.total_cost_limit }));
    // RPM/TPM are no longer separate fields — the backend folds them into
    // window_limits' 60s counts / 60s tokens entries via effective_limits().
    // Rendering them here would either show "undefined 请求/1min" (field gone)
    // or duplicate the 60s line that the forEach below already emits.
    (plan.window_limits || []).forEach((raw) => {
      const w = normalizeWindowLimit(raw);
      if (!w) return;
      const parts = [];
      if (w.counts != null) parts.push(t("plan.dim.counts", { n: w.counts }));
      if (w.tokens != null) parts.push(t("plan.dim.tokens", { n: w.tokens }));
      if (w.costs != null) parts.push(t("plan.dim.costs", { n: w.costs }));
      if (parts.length) limits.push(t("plan.limits.window", { dims: parts.join(" / "), duration: formatDuration(w.window_secs) }));
    });
    el.innerHTML = `
      <div class="plan-card">
        <div class="plan-name-badge">${esc(plan.plan_name)}</div>
        <div class="plan-limits">${limits.map((l) => `<div class="plan-limit-row">${esc(l)}</div>`).join("") || `<div class="muted">${esc(t("common.unlimited"))}</div>`}</div>
      </div>
    `;
  }

  function renderUsage(usage) {
    const el = document.getElementById("usage-info");
    let html = '<div class="usage-grid">';

    // Concurrency card
    const concLimit = usage.concurrency_limit;
    const concCount = usage.concurrency || 0;
    if (concLimit != null) {
      const pct = concLimit > 0 ? Math.min(100, (concCount / concLimit) * 100) : 0;
      html += `<div class="usage-limit-card">
        <div class="usage-limit-title">${t("req.concurrency")}</div>
        <div class="usage-limit-count">${concCount} / ${concLimit}</div>
        <div class="progress-bar"><div class="progress-fill ${pct >= 90 ? "danger" : pct >= 70 ? "warn" : ""}" style="width:${pct}%"></div></div>
        <div class="usage-limit-reset">${t("req.simultaneous")}</div>
      </div>`;
    } else {
      html += `<div class="usage-limit-card">
        <div class="usage-limit-title">${t("req.concurrency")}</div>
        <div class="usage-limit-count">${concCount}</div>
        <div class="usage-limit-reset">${t("common.unlimited")}</div>
      </div>`;
    }

    // Window cards: one per window_secs. Each card holds multi-dim progress
    // bars (counts / tokens / costs — whichever the plan configured).
    if (!usage.windows || usage.windows.length === 0) {
      html += '<div class="usage-limit-card usage-limit-wide"><div class="usage-limit-title">' +
        t("req.rate_limits") + '</div><div class="usage-limit-reset">' +
        t("req.no_active_windows") + '</div></div>';
    } else {
      usage.windows.forEach((w) => {
        const isRpm = w.window_secs === 60;
        const label = isRpm ? "RPM (60s)" : t("plan.window_limit_label", { duration: formatDuration(w.window_secs) });
        const remaining = w.remaining_secs || 0;
        const dims = w.dims || {};
        const dimKeys = ["counts", "tokens", "costs"].filter((k) => dims[k]);
        if (dimKeys.length === 0) return; // nothing configured, skip

        let dimHtml = "";
        dimKeys.forEach((k) => {
          const d = dims[k];
          let cur, limit, display;
          if (k === "costs") {
            cur = Number(d.current_micros || 0);
            limit = Number(d.limit_micros || 0);
            display = "$" + (d.current || "0") + " / " + (limit > 0 ? "$" + (d.limit || "0") : t("common.unlimited"));
          } else {
            cur = Number(d.current || 0);
            limit = Number(d.limit || 0);
            display = formatNumber(cur) + " / " + (limit > 0 ? formatNumber(limit) : t("common.unlimited"));
          }
          const pct = limit > 0 ? Math.min(100, (cur / limit) * 100) : 0;
          const cls = pct >= 90 ? "danger" : pct >= 70 ? "warn" : "";
          dimHtml += `
            <div class="dim-row">
              <span class="dim-label">${esc(t("plan.dim." + k))}</span>
              <div class="progress-bar"><div class="progress-fill ${cls}" style="width:${pct}%"></div></div>
              <span class="dim-value">${esc(display)}</span>
            </div>`;
        });

        html += `<div class="usage-limit-card usage-limit-wide">
          <div class="usage-limit-title">${esc(label)}</div>
          ${dimHtml}
          <div class="usage-limit-reset">${t("req.resets_in", { time: formatCountdown(remaining) })}</div>
        </div>`;
      });
    }

    // Cumulative card (if plan has any total_*_limit or any cumulative usage > 0).
    const c = usage.cumulative || {};
    const hasCumLimit = c.total_token_limit != null || c.total_cost_limit != null;
    const hasCumUsage = (c.total_tokens || 0) > 0 || (c.total_cost_micros || 0) > 0;
    if (hasCumLimit || hasCumUsage) {
      let dimHtml = "";
      if (c.total_token_limit != null) {
        const cur = Number(c.total_tokens || 0);
        const limit = Number(c.total_token_limit);
        const pct = limit > 0 ? Math.min(100, (cur / limit) * 100) : 0;
        const cls = pct >= 90 ? "danger" : pct >= 70 ? "warn" : "";
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.total_tokens"))}</span>
            <div class="progress-bar"><div class="progress-fill ${cls}" style="width:${pct}%"></div></div>
            <span class="dim-value">${formatNumber(cur)} / ${formatNumber(limit)}</span>
          </div>`;
      } else if ((c.total_tokens || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.total_tokens"))}</span>
            <div class="dim-value">${formatNumber(c.total_tokens)}</div>
          </div>`;
      }
      if (c.total_cost_limit != null) {
        const cur = Number(c.total_cost_micros || 0);
        const limitMicros = Math.round(Number(c.total_cost_limit) * 1_000_000);
        const pct = limitMicros > 0 ? Math.min(100, (cur / limitMicros) * 100) : 0;
        const cls = pct >= 90 ? "danger" : pct >= 70 ? "warn" : "";
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.total_cost"))}</span>
            <div class="progress-bar"><div class="progress-fill ${cls}" style="width:${pct}%"></div></div>
            <span class="dim-value">$${esc(c.total_cost || "0")} / $${esc(c.total_cost_limit)}</span>
          </div>`;
      } else if ((c.total_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.total_cost"))}</span>
            <div class="dim-value">$${esc(c.total_cost || "0")}</div>
          </div>`;
      }
      // Cost breakdown (cached / non-cached / output). Only show when the
      // relevant component is non-zero — keeps the card minimal for fresh keys.
      if ((c.regular_input_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.regular_input_cost"))}</span>
            <div class="dim-value">$${esc(c.regular_input_cost || "0")}</div>
          </div>`;
      }
      if ((c.cached_input_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.cached_input_cost"))}</span>
            <div class="dim-value">$${esc(c.cached_input_cost || "0")}</div>
          </div>`;
      }
      if ((c.output_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.output_cost"))}</span>
            <div class="dim-value">$${esc(c.output_cost || "0")}</div>
          </div>`;
      }
      if (dimHtml) {
        html += `<div class="usage-limit-card usage-limit-wide">
          <div class="usage-limit-title">${esc(t("plan.cumulative_title"))}</div>
          ${dimHtml}
        </div>`;
      }
    }

    html += '</div>';
    el.innerHTML = html;
  }

  function renderTokenInfo(info) {
    const el = document.getElementById("token-info");
    const input = info.total_input_tokens;
    const output = info.total_output_tokens;
    // If both are null the SpendLogs table doesn't exist — hide the card.
    if (input == null && output == null) {
      el.innerHTML = '<p style="color:var(--text3)">' + t("token.usage_unavailable") + '</p>';
      return;
    }
    const total = (input || 0) + (output || 0);
    const inputPct = total > 0 ? ((input || 0) / total * 100).toFixed(1) : 0;
    const outputPct = total > 0 ? ((output || 0) / total * 100).toFixed(1) : 0;
    el.innerHTML = `
      <div class="token-stats">
        <div class="token-stat">
          <div class="token-stat-label">${t("token.input")}</div>
          <div class="token-stat-value">${formatNumber(input || 0)}</div>
          <div class="token-stat-pct">${inputPct}%</div>
        </div>
        <div class="token-stat">
          <div class="token-stat-label">${t("token.output")}</div>
          <div class="token-stat-value">${formatNumber(output || 0)}</div>
          <div class="token-stat-pct">${outputPct}%</div>
        </div>
        <div class="token-stat token-stat-total">
          <div class="token-stat-label">${t("token.total")}</div>
          <div class="token-stat-value">${formatNumber(total)}</div>
        </div>
      </div>
    `;
  }

  function renderKeyInfo(info) {
    const el = document.getElementById("key-info");
    if (info.error) { el.innerHTML = `<p>${esc(info.error)}</p>`; return; }
    const rows = [
      [t("keyinfo.alias"), info.key_alias || "-"],
      [t("keyinfo.token"), info.token_prefix],
      [t("keyinfo.name"), info.key_name || "-"],
      [t("keyinfo.spend"), "$" + (info.spend || 0).toFixed(4)],
      [t("keyinfo.max_budget"), info.max_budget != null ? "$" + info.max_budget : t("common.unlimited")],
      [t("keyinfo.blocked"), info.blocked ? t("common.yes") : t("common.no")],
      [t("keyinfo.expires"), info.expires || t("common.never")],
      [t("keyinfo.created"), info.created_at || "-"],
    ];
    el.innerHTML = `<table>${rows.map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(String(v))}</td></tr>`).join("")}</table>`;
  }

  function startUsageRefresh() {
    clearUsageRefresh();
    loadRequestStatus();
    usageRefreshTimer = setInterval(async () => {
      try {
        const usage = await api("/user/usage");
        renderUsage(usage);
      } catch {}
      loadRequestStatus();
    }, 5000);
  }

  function clearUsageRefresh() {
    if (usageRefreshTimer) { clearInterval(usageRefreshTimer); usageRefreshTimer = null; }
  }

  // ── Request Status (queue waiting info) ───────────────
  async function loadRequestStatus() {
    try {
      const data = await api("/user/request-status");
      renderRequestStatus(data.requests || []);
    } catch {
      const el = document.getElementById("request-status-info");
      if (el) el.innerHTML = '<p style="color:var(--text3)">' + t("user.usage.no_active") + '</p>';
    }
  }

  function renderRequestStatus(requests) {
    const el = document.getElementById("request-status-info");
    if (!el) return;
    if (requests.length === 0) {
      el.innerHTML = '<p style="color:var(--text3)">' + t("user.usage.no_active") + '</p>';
      return;
    }
    el.innerHTML = '<table class="data-table"><thead><tr>' +
      '<th>' + t("logs.col.model") + '</th><th>' + t("logs.col.status") + '</th><th>' + t("logs.col.detail") + '</th><th>' + t("logs.col.total_wait") + '</th>' +
      '</tr></thead><tbody>' +
      requests.map(function (r) {
        var statusBadge = r.status === "waiting"
          ? '<span class="badge badge-blocked">' + t("status.waiting") + '</span>'
          : '<span class="badge badge-active">' + t("status.processing") + '</span>';
        var detail;
        if (r.status === "waiting") {
          detail = t("req.waiting_detail", { ahead: (r.ahead || 0) });
        } else {
          var ps = (r.processing_secs || 0);
          var timeStr = ps < 60 ? ps.toFixed(1) + 's' : (ps / 60).toFixed(1) + 'min';
          detail = t("req.processing_detail", { time: timeStr, parallel: (r.parallel_count || 0) });
        }
        var waitStr = r.wait_time_secs < 60
          ? r.wait_time_secs.toFixed(1) + 's'
          : (r.wait_time_secs / 60).toFixed(1) + 'min';
        var vipTag = r.is_vip ? ' <span class="badge badge-vip" style="font-size:0.75em">VIP</span>' : '';
        return '<tr>' +
          '<td class="mono">' + esc(r.model) + vipTag + '</td>' +
          '<td>' + statusBadge + '</td>' +
          '<td>' + esc(detail) + '</td>' +
          '<td>' + waitStr + '</td>' +
          '</tr>';
      }).join('') +
      '</tbody></table>';
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
    if (plans.length === 0) { wrap.innerHTML = "<p>" + t("plans.empty") + "</p>"; return; }
    const fmtOptInt = (v) => (v == null ? "-" : Number(v).toLocaleString());
    const fmtOptCost = (v) => (v == null ? "-" : "$" + String(v));
    const fmtSchedule = (slots) => {
      if (!Array.isArray(slots) || slots.length === 0) return "-";
      return slots.map((s) => esc(s.hours || "")).join(", ");
    };
    wrap.innerHTML = `<table>
      <tr>
        <th>${t("plans.col.name")}</th>
        <th>${t("plans.col.type")}</th>
        <th>${t("plans.col.concurrent")}</th>
        <th>${t("plans.col.rpm")}</th>
        <th>${t("plans.col.tpm")}</th>
        <th>${t("plans.col.windows")}</th>
        <th>${t("plans.col.total_token")}</th>
        <th>${t("plans.col.total_cost")}</th>
        <th>${t("plans.col.member_plan")}</th>
        <th>${t("plans.col.schedule")}</th>
        <th>${t("plans.col.actions")}</th>
      </tr>
      ${plans.map((p) => {
        const wlSummary = (p.window_limits || [])
          .map((raw) => {
            const w = normalizeWindowLimit(raw);
            if (!w) return null;
            const dims = [];
            if (w.counts != null) dims.push(t("plan.dim.counts", { n: w.counts }));
            if (w.tokens != null) dims.push(t("plan.dim.tokens", { n: w.tokens }));
            if (w.costs != null) dims.push(t("plan.dim.costs", { n: w.costs }));
            return dims.length ? dims.join("/") + " / " + formatDuration(w.window_secs) : null;
          })
          .filter(Boolean)
          .join(", ");
        const typeLabel = p.type === "team" ? t("plans.type.team") : t("plans.type.key");
        return `<tr>
          <td><strong>${esc(p.name)}</strong></td>
          <td>${esc(typeLabel)}</td>
          <td>${fmtOptInt(p.concurrency_limit)}</td>
          <td>${fmtOptInt(p.rpm_limit)}</td>
          <td>${fmtOptInt(p.tpm_limit)}</td>
          <td>${wlSummary || "-"}</td>
          <td>${fmtOptInt(p.total_token_limit)}</td>
          <td>${fmtOptCost(p.total_cost_limit)}</td>
          <td>${p.member_plan ? esc(p.member_plan) : "-"}</td>
          <td>${fmtSchedule(p.schedule)}</td>
          <td>
            <button class="btn-small" onclick="window._editPlan('${esc(p.name)}')">${t("action.edit")}</button>
            <button class="btn-danger" onclick="window._deletePlan('${esc(p.name)}')">${t("action.delete")}</button>
          </td>
        </tr>`;
      }).join("")}
    </table>`;
  }

  window._deletePlan = async (name) => {
    if (!confirm(t("confirm.delete_plan", { name }))) return;
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
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("keys.title"), message: esc(err.message) })}</p>`;
      console.error("loadKeys error:", err);
    }
  }

  function renderKeysTable(keys) {
    const wrap = document.getElementById("keys-table-wrap");
    if (keys.length === 0) { wrap.innerHTML = "<p>" + t("keys.empty") + "</p>"; return; }
    const fmtTokens = (n) => {
      const v = Number(n) || 0;
      if (v >= 1e9) return (v / 1e9).toFixed(2) + "B";
      if (v >= 1e6) return (v / 1e6).toFixed(2) + "M";
      if (v >= 1e3) return (v / 1e3).toFixed(2) + "K";
      return String(v);
    };
    const fmtCost = (s) => {
      const v = Number(s) || 0;
      if (v >= 1) return "$" + v.toFixed(2);
      if (v > 0) return "$" + v.toFixed(4);
      return "$0";
    };
    wrap.innerHTML = `<table>
      <tr><th>${t("keys.col.token")}</th><th>${t("keys.col.alias")}</th><th>${t("keys.col.user")}</th><th>${t("keys.col.plan")}</th><th>${t("keys.col.usage")}</th><th>${t("keys.col.spend")}</th><th>${t("keys.col.budget")}</th><th>${t("keys.col.status")}</th><th>${t("keys.col.actions")}</th></tr>
      ${keys.map((k) => `<tr>
        <td class="mono">${esc(k.token_prefix)}</td>
        <td>${esc(k.key_alias || "-")}</td>
        <td>${esc(k.user_id || "-")}</td>
        <td>${esc(k.plan_name || "-")}</td>
        <td><span class="mono">${k.usage_count || 0}/${fmtTokens(k.usage_tokens)}/${fmtCost(k.usage_cost)}</span><br><span class="muted" style="font-size:11px">${formatCountdown(k.usage_reset_secs || 0)}</span></td>
        <td>${fmtCost(k.spend)}</td>
        <td>${k.max_budget != null ? "$" + k.max_budget : "-"}</td>
        <td>${k.blocked
              ? '<span style="color:var(--danger)">' + t("status.blocked") + '</span>'
              : (k.metadata && k.metadata.vip === true)
                ? '<span class="badge badge-vip">' + t("status.active_vip") + '</span>'
                : t("status.active")}</td>
        <td>
          <button class="btn-small" onclick="window._editKey('${esc(k.token_hash)}')">${t("action.edit")}</button>
          <button class="btn-small" onclick="window._resetKeyLimits('${esc(k.token_hash)}')">${t("action.reset_limits")}</button>
          ${k.blocked
            ? `<button class="btn-small" onclick="window._unblockKey('${esc(k.token_hash)}')">${t("action.unblock")}</button>`
            : `<button class="btn-danger" onclick="window._blockKey('${esc(k.token_hash)}')">${t("action.block")}</button>`}
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
      <span>${t("common.page_of", { page: data.page, total: pages, count: data.total, unit: t("nav.keys") })}</span>
      <button ${data.page >= pages ? "disabled" : ""} onclick="window._loadKeysPage(${data.page + 1})">&gt;</button>
    `;
  }

  window._loadKeysPage = (p) => loadKeys(p);
  window._copyText = function(btn, text) {
    const done = function() { btn.textContent = t("toast.copied"); setTimeout(function() { btn.textContent = t("action.copy"); }, 2000); };
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done).catch(function() {
        // Fallback for non-secure contexts.
        const ta = document.createElement("textarea");
        ta.value = text;
        ta.style.position = "fixed";
        ta.style.opacity = "0";
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand("copy"); } catch (e) { /* ignore */ }
        document.body.removeChild(ta);
        done();
      });
    } else {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand("copy"); } catch (e) { /* ignore */ }
      document.body.removeChild(ta);
      done();
    }
  };
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
    if (!confirm(t("confirm.reset_key"))) return;
    const r = await api(`/admin/limits/reset/${encodeURIComponent(hash)}`, { method: "POST" });
    alert(r.message || t("alert.done"));
  };

  // ── Admin: Assignments ────────────────────────────────
  let assignmentsPage = 1;
  const ASSIGNMENTS_PER_PAGE = 20;
  let assignmentsTotal = 0;
  let assignmentsData = [];

  async function loadAssignments(page) {
    try {
      assignmentsPage = page || 1;
      const data = await api(`/admin/assignments?page=${assignmentsPage}&page_size=${ASSIGNMENTS_PER_PAGE}`);
      assignmentsData = data.assignments || [];
      assignmentsTotal = data.total || 0;
      renderAssignmentsTable();
    } catch {}
  }

  function renderAssignmentsTable() {
    const wrap = document.getElementById("assignments-table-wrap");
    if (assignmentsTotal === 0) { wrap.innerHTML = "<p>" + t("assignments.empty") + "</p>"; renderAssignmentsPagination(); return; }
    wrap.innerHTML = `<table>
      <tr><th>${t("assignments.col.key")}</th><th>${t("assignments.col.plan")}</th><th>${t("assignments.col.actions")}</th></tr>
      ${assignmentsData.map((a) => `<tr>
        <td><span>${esc(a.key_alias || t("assignments.no_alias"))}</span><br><span class="mono muted">${esc(a.token_prefix || a.key_hash.substring(0, 8) + "...")}</span></td>
        <td>${esc(a.plan_name)}</td>
        <td><button class="btn-danger" onclick="window._unassignKey('${esc(a.key_hash)}')">${t("action.remove")}</button></td>
      </tr>`).join("")}
    </table>`;
    renderAssignmentsPagination();
  }

  function renderAssignmentsPagination() {
    const el = document.getElementById("assignments-pagination");
    const pages = Math.ceil(assignmentsTotal / ASSIGNMENTS_PER_PAGE);
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${assignmentsPage <= 1 ? "disabled" : ""} onclick="window._loadAssignmentsPage(${assignmentsPage - 1})">&lt;</button>
      <span>${t("common.page_of", { page: assignmentsPage, total: pages, count: assignmentsTotal, unit: t("nav.assignments") })}</span>
      <button ${assignmentsPage >= pages ? "disabled" : ""} onclick="window._loadAssignmentsPage(${assignmentsPage + 1})">&gt;</button>
    `;
  }

  window._loadAssignmentsPage = (p) => loadAssignments(p);

  window._unassignKey = async (hash) => {
    await api(`/admin/assignments/${encodeURIComponent(hash)}`, { method: "DELETE" });
    loadAssignments();
  };

  // ── Admin: Models ─────────────────────────────────────
  async function loadModels() {
    try {
      const [modelsResp, aliasesResp] = await Promise.all([
        api("/admin/models"),
        api("/admin/aliases"),
      ]);
      // Build { target_model → [alias_name, ...] } map so the models table
      // can show each model's aliases inline without a separate aliases page.
      const aliasesMap = {};
      (aliasesResp.aliases || []).forEach((a) => {
        if (!a.target_model) return;
        (aliasesMap[a.target_model] = aliasesMap[a.target_model] || []).push(a.alias_name);
      });
      renderModelsTable(modelsResp.models || [], aliasesMap);
    } catch (err) {
      const wrap = document.getElementById("models-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("models.title"), message: esc(err.message) })}</p>`;
    }
  }

  function renderModelsTable(models, aliasesMap) {
    const wrap = document.getElementById("models-table-wrap");
    if (models.length === 0) { wrap.innerHTML = "<p>" + t("models.empty") + "</p>"; return; }
    aliasesMap = aliasesMap || {};
    wrap.innerHTML = `<table>
      <tr><th>${t("models.col.model")}</th><th>${t("models.col.aliases")}</th><th>${t("models.col.litellm_model")}</th><th>${t("models.col.cost")}</th><th>${t("models.col.base_url")}</th><th>${t("models.col.ratio")}</th><th>${t("models.col.rpm")}</th><th>${t("models.col.timeout")}</th><th>${t("models.col.enabled")}</th><th>${t("models.col.source")}</th><th>${t("models.col.actions")}</th></tr>
      ${models.map((m) => {
        const isAutoDisabled = !m.enabled && m.auto_disabled;
        const enabledBadge = m.enabled
          ? '<span class="badge badge-active">' + t("common.yes") + '</span>'
          : isAutoDisabled
            ? '<span class="badge badge-blocked">' + t("common.no") + '</span><br><span style="color:var(--danger);font-size:0.8em">' + t("status.auto_disabled") + '</span>'
            : '<span class="badge badge-blocked">' + t("common.no") + '</span>';
        const warningRow = isAutoDisabled
          ? `<tr style="background:rgba(255,80,80,0.08)"><td colspan="11" style="padding:4px 8px;font-size:0.85em;color:var(--danger)">${t("models.fault_disabled")}</td></tr>`
          : '';
        // Cost cell: inline "label:$value" per line, three rows. Compact so
        // the column stays narrow even when EN headers squeeze the table.
        const c = m.cost_per_million || {};
        const fmtCost = (v) => (v == null || v === "0" || v === "") ? "-" : "$" + v;
        const costCell = '<div class="cost-cell">'
          + '<div class="cost-line"><span class="cost-label">' + esc(t("plan.dim.regular_input_cost")) + ':</span><span class="cost-value">' + fmtCost(c.input) + '</span></div>'
          + '<div class="cost-line"><span class="cost-label">' + esc(t("plan.dim.cached_input_cost")) + ':</span><span class="cost-value">' + fmtCost(c.cached_input) + '</span></div>'
          + '<div class="cost-line"><span class="cost-label">' + esc(t("plan.dim.output_cost")) + ':</span><span class="cost-value">' + fmtCost(c.output) + '</span></div>'
          + '</div>';
        // Alias cell: 0 → "-"; 1 → chip with name; ≥2 → "View N aliases" button.
        const aliases = aliasesMap[m.model_name] || [];
        let aliasCell;
        if (aliases.length === 0) {
          aliasCell = '<span class="muted">-</span>';
        } else if (aliases.length === 1) {
          aliasCell = '<span class="alias-chip">' + esc(aliases[0]) + '</span>';
        } else {
          aliasCell = '<button class="btn-small" onclick="window._showModelAliases(\'' + esc(m.model_name) + '\')">'
            + esc(t("models.aliases.view_detail", { n: aliases.length })) + '</button>';
        }
        return `<tr${isAutoDisabled ? ' style="background:rgba(255,80,80,0.04)"' : ''}>
        <td>${renderDeployCell(m.model_name, m.deployment_id)}</td>
        <td>${aliasCell}</td>
        <td class="mono">${esc(m.litellm_model)}</td>
        <td>${costCell}</td>
        <td class="mono">${esc(m.api_base || "-")}</td>
        <td>${m.quota_count_ratio && m.quota_count_ratio !== 1 ? '<span class="badge badge-plan">x' + m.quota_count_ratio + '</span>' : 'x1'}</td>
        <td>${m.rpm || "-"}</td>
        <td>${m.timeout}s</td>
        <td>${enabledBadge}</td>
        <td><span class="badge badge-plan">${esc(m.source || "-")}</span></td>
        <td>
          <button class="btn-small" onclick="window._editModel('${m.id}')">${t("action.edit")}</button>
          <button class="btn-danger" onclick="window._deleteModel('${m.id}','${esc(m.model_name)}')">${t("action.delete")}</button>
        </td>
      </tr>${warningRow}`;
      }).join("")}
    </table>`;
  }

  function showNewModelModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.id ? t("form.model.title_edit") : t("form.model.title_create")}</h3>
      <div class="form-group"><label>${t("form.model.name")} * ${tip("Client-visible model name. Multiple deployments can share the same name for load balancing.")}</label><input id="m-model-name" value="${esc(p.model_name || "")}" required></div>
      <div class="form-group"><label>${t("form.model.provider")} * ${tip("Upstream provider type. Determines API format and authentication.")}</label><select id="m-model-provider"><option value="">${t("common.select_placeholder")}</option><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="azure">Azure OpenAI</option><option value="gemini">Google Gemini</option><option value="bedrock">AWS Bedrock</option></select></div>
      <div class="form-group"><label>${t("form.model.id")} * ${tip("Actual model ID at the provider, e.g. gpt-4o, claude-sonnet-4-20250514. Auto-combined with Provider as provider/model-id.")}</label><input id="m-model-id" value="${esc((p.litellm_model || "").includes("/") ? p.litellm_model.split("/").slice(1).join("/") : p.litellm_model || "")}" required></div>
      <div class="form-group"><label>${t("form.model.api_key")} ${tip("Provider API key. Use os.environ/VAR_NAME for env reference.")}</label><input id="m-model-key" type="password" value="${esc(p.api_key || "")}" placeholder="sk-... or os.environ/VAR"></div>
      <div class="form-group"><label>${t("form.model.api_key_env")} ${tip("Enable if the API Key field contains an environment variable reference like os.environ/VAR_NAME.")}</label><select id="m-model-key-env"><option value="false">${t("common.no")}</option><option value="true" ${(p.api_key_env) ? "selected" : ""}>${t("common.yes")}</option></select></div>
      <div class="form-group"><label>${t("form.model.base")} ${tip("Override the default provider endpoint, e.g. https://api.openai.com/v1")}</label><input id="m-model-base" value="${esc(p.api_base || "")}" placeholder="https://api.openai.com/v1"></div>
      <div class="form-group"><label>${t("form.model.version")} ${tip("Required for Azure OpenAI deployments, e.g. 2024-02-01")}</label><input id="m-model-version" value="${esc(p.api_version || "")}"></div>
      <div class="form-group"><label>${t("form.model.ratio")} ${tip("Quota consumption multiplier. Each request counts as this many units against rate limits. E.g. 3 means one request consumes 3 quota. Default: 1.")}</label><input id="m-model-ratio" type="number" min="1" step="1" value="${p.quota_count_ratio || 1}"></div>
      <div class="form-group"><label>${t("form.model.rpm")} ${tip("Per-deployment RPM limit. Leave empty for unlimited.")}</label><input id="m-model-rpm" type="number" value="${p.rpm || ""}"></div>
      <div class="form-group"><label>${t("form.model.timeout")} ${tip("Request timeout. Default: 120s.")}</label><input id="m-model-timeout" type="number" value="${p.timeout || 120}"></div>
      <div class="form-group"><label>${t("form.model.temp")} ${tip("Sampling temperature override (0.0-2.0). Leave empty to use provider default.")}</label><input id="m-model-temp" type="number" step="0.1" value="${p.temperature || ""}"></div>
      <div class="form-group"><label>${t("form.model.maxtok")} ${tip("Maximum output tokens. Leave empty for provider default.")}</label><input id="m-model-maxtok" type="number" value="${p.max_tokens || ""}"></div>
      <div class="form-group"><label>${t("form.model.maxinflight")} ${tip("Max concurrent in-flight requests for this deployment. 0 or empty = unlimited.")}</label><input id="m-model-maxinflight" type="number" min="0" value="${p.max_inflight_queue_len || ""}"></div>
      <div class="form-group"><label>${t("form.model.maxctx")} ${tip("Max total input characters across all in-flight requests. 0 or empty = unlimited.")}</label><input id="m-model-maxctx" type="number" min="0" value="${p.max_context_len || ""}"></div>
      <div class="form-group"><label>${t("form.model.enabled")} ${tip("Disabled deployments are ignored in routing.")}</label><select id="m-model-enabled"><option value="true" ${p.enabled !== false ? "selected" : ""}>${t("common.yes")}</option><option value="false" ${p.enabled === false ? "selected" : ""}>${t("common.no")}</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-model-submit">${p.id ? t("action.update") : t("action.create")}</button>
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
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  window._editModel = async (id) => {
    try {
      const data = await api("/admin/models");
      const m = (data.models || []).find((x) => x.id === id);
      if (!m) return;
      showNewModelModal(m);
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  window._deleteModel = async (id, name) => {
    if (!confirm(t("confirm.delete_model", { name }))) return;
    await api(`/admin/models/${encodeURIComponent(id)}`, { method: "DELETE" });
    loadModels();
  };

  // ── Admin: Aliases ────────────────────────────────────
  function showNewAliasModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.alias_name ? t("form.alias.title_edit") : t("form.alias.title_create")}</h3>
      <div class="form-group"><label>${t("form.alias.name")} * ${tip("The name clients will use in their request. E.g. 'gpt-4' → routes to 'gpt-4o'.")}</label><input id="m-alias-name" value="${esc(p.alias_name || "")}" ${p.alias_name ? "readonly" : ""}></div>
      <div class="form-group"><label>${t("form.alias.target")} * ${tip("The actual model name to route to. Must match an existing model deployment name.")}</label><input id="m-alias-target" value="${esc(p.target_model || "")}" required list="alias-target-list"><datalist id="alias-target-list"></datalist></div>
      <div class="form-group"><label>${t("form.alias.hidden")} ${tip("Hidden aliases work for routing but are not listed to users in model discovery endpoints.")}</label><select id="m-alias-hidden"><option value="false" ${!p.hidden ? "selected" : ""}>${t("common.no")}</option><option value="true" ${p.hidden ? "selected" : ""}>${t("common.yes")}</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-alias-submit">${p.alias_name ? t("action.update") : t("action.create")}</button>
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
        loadModels();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  window._editAlias = async (name) => {
    try {
      const data = await api("/admin/aliases");
      const a = (data.aliases || []).find((x) => x.alias_name === name);
      if (!a) return;
      showNewAliasModal(a);
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  window._deleteAlias = async (name) => {
    if (!confirm(t("confirm.delete_alias", { name }))) return;
    await api(`/admin/aliases/${encodeURIComponent(name)}`, { method: "DELETE" });
    loadModels();
  };

  window._showModelAliases = async (modelName) => {
    try {
      const data = await api("/admin/aliases");
      const list = (data.aliases || []).filter((a) => a.target_model === modelName);
      showModal(`
        <h3>${t("models.aliases.modal_title", { model: esc(modelName) })}</h3>
        ${list.length === 0
          ? `<p class="muted">${t("common.no_data")}</p>`
          : `<table class="modal-table">
              <tr><th>${t("aliases.col.alias")}</th><th>${t("form.alias.hidden")}</th><th>${t("models.col.source")}</th><th>${t("aliases.col.actions")}</th></tr>
              ${list.map((a) => `<tr>
                <td><strong>${esc(a.alias_name)}</strong></td>
                <td>${a.hidden ? t("common.yes") : t("common.no")}</td>
                <td><span class="badge badge-plan">${esc(a.source || "-")}</span></td>
                <td>
                  <button class="btn-small" onclick="window._editAlias('${esc(a.alias_name)}')">${t("action.edit")}</button>
                  <button class="btn-small is-danger" onclick="window._deleteAlias('${esc(a.alias_name)}')">${t("action.delete")}</button>
                </td>
              </tr>`).join("")}
            </table>`}
        <div class="modal-actions">
          <button class="btn-small btn-inline" onclick="hideModal()">${t("action.close")}</button>
        </div>
      `);
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
    } catch {
      // Release builds compile out debug endpoints — hide the toggle button
      // and the Debug nav-link (404 → catch branch).
      btn.style.display = "none";
      document.querySelectorAll('.nav-link[data-section="admin-debug"]')
        .forEach((el) => el.style.display = "none");
    }
  }

  function updateDebugButton(btn) {
    if (!btn) return;
    btn.textContent = debugEnabled ? t("logs.debug.on") : t("logs.debug.off");
    if (debugEnabled) {
      btn.style.background = "var(--danger)";
      btn.style.color = "#fff";
      btn.style.borderColor = "var(--danger)";
    } else {
      btn.style.background = "";
      btn.style.color = "";
      btn.style.borderColor = "";
    }
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
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  }

  async function showDebugError(requestId) {
    try {
      const data = await api("/admin/debug/errors/" + requestId);
      showModal("<pre style='max-height:60vh;overflow:auto'>" + esc(JSON.stringify(data, null, 2)) + "</pre>");
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
    btn.textContent = promptLogEnabled ? t("logs.prompt_log.on") : t("logs.prompt_log.off");
    if (promptLogEnabled) {
      btn.style.background = "var(--info)";
      btn.style.color = "#fff";
      btn.style.borderColor = "var(--info)";
    } else {
      btn.style.background = "";
      btn.style.color = "";
      btn.style.borderColor = "";
    }
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
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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

  async function showDebugError(requestId) {
    try {
      const data = await api("/admin/debug/errors/" + encodeURIComponent(requestId));
      const e = data.debug_error;
      if (!e) { alert(t("debug.entry_not_found")); return; }

      let upstreamHtml = "";
      if (e.upstream_status != null) {
        upstreamHtml = `
          <div class="debug-section">
            <h4>${t("debug.upstream_response")}</h4>
            <table>
              <tr><td style="width:120px">${t("debug.status")}</td><td>${e.upstream_status}</td></tr>
              <tr><td>${t("logs.col.error")}</td><td><pre class="debug-json">${esc(formatJson(e.upstream_body || "-"))}</pre></td></tr>
            </table>
          </div>`;
      }

      let requestHtml = "";
      if (e.request_body) {
        requestHtml = `
          <div class="debug-section">
            <h4>${t("debug.original_request")}</h4>
            <pre class="debug-json">${esc(formatJson(e.request_body))}</pre>
          </div>`;
      }

      showModal(`
        <h3>${t("nav.debug")}: ${esc(e.error_type)}</h3>
        <table>
          <tr><td style="width:120px">${t("debug.request_id")}</td><td class="mono">${esc(e.request_id)}</td></tr>
          <tr><td>${t("debug.key")}</td><td>${esc(e.key_alias || e.key_hash.substring(0, 12) + "...")}</td></tr>
          <tr><td>${t("debug.model")}</td><td class="mono">${esc(e.model)}</td></tr>
          <tr><td>${t("debug.path")}</td><td class="mono">${esc(e.api_path)}</td></tr>
          <tr><td>${t("debug.stream")}</td><td>${e.is_stream ? t("common.yes") : t("common.no")}</td></tr>
          <tr><td>${t("debug.time")}</td><td>${formatTimestamp(e.created_at)}</td></tr>
          <tr><td>${t("debug.status")}</td><td>${e.status_code}</td></tr>
          <tr><td>${t("debug.error")}</td><td>${esc(e.error_message)}</td></tr>
        </table>
        ${upstreamHtml}
        ${requestHtml}
        <div class="modal-actions">
          <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.close")}</button>
        </div>
      `);
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
      if (!confirm(t("confirm.reset_all"))) return;
      const r = await api("/admin/limits/reset", { method: "POST" });
      alert(r.message || t("alert.done"));
    });
    document.getElementById("btn-new-key").addEventListener("click", showNewKeyModal);
    const btnVipFilter = document.getElementById("btn-vip-filter");
    if (btnVipFilter) btnVipFilter.addEventListener("click", () => {
      keysVipOnly = !keysVipOnly;
      if (keysVipOnly) {
        btnVipFilter.style.background = "var(--primary)";
        btnVipFilter.style.color = "#fff";
        btnVipFilter.style.borderColor = "var(--primary)";
      } else {
        btnVipFilter.style.background = "";
        btnVipFilter.style.color = "";
        btnVipFilter.style.borderColor = "";
      }
      keysPage = 1;
      loadKeys();
    });
    document.getElementById("btn-new-assignment").addEventListener("click", showNewAssignmentModal);
    const btnModel = document.getElementById("btn-new-model");
    if (btnModel) btnModel.addEventListener("click", showNewModelModal);
    const btnAlias = document.getElementById("btn-new-alias");
    if (btnAlias) btnAlias.addEventListener("click", showNewAliasModal);
    const btnReload = document.getElementById("btn-reload-config");
    if (btnReload) btnReload.addEventListener("click", async () => {
      if (!confirm(t("confirm.reload"))) return;
      btnReload.disabled = true;
      btnReload.textContent = t("action.reloading");
      try {
        const data = await api("/admin/config/reload", { method: "POST" });
        alert(data.message || t("alert.config_reloaded"));
        onRoute();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
      finally {
        btnReload.disabled = false;
        btnReload.textContent = t("action.reload_config");
      }
    });
    const btnRefreshInflight = document.getElementById("btn-refresh-inflight");
    if (btnRefreshInflight) btnRefreshInflight.addEventListener("click", async () => {
      btnRefreshInflight.disabled = true;
      btnRefreshInflight.textContent = t("action.refreshing");
      try {
        await loadDeployment24hSummary();
      } catch (err) { console.error("Refresh inflight error:", err); }
      finally {
        btnRefreshInflight.disabled = false;
        btnRefreshInflight.textContent = t("action.refresh");
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
    document.getElementById("modal-content").classList.remove("modal-wide");
  }
  window.hideModal = hideModal;

  // Prevent modal close when drag starts on content but ends on overlay.
  let _modalMouseDownTarget = null;
  document.getElementById("modal-overlay").addEventListener("mousedown", (e) => {
    _modalMouseDownTarget = e.target;
  });
  document.getElementById("modal-overlay").addEventListener("click", (e) => {
    if (e.target === e.currentTarget && _modalMouseDownTarget === e.currentTarget) hideModal();
  });

  function showNewPlanModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.name ? t("form.plan.title_edit") : t("form.plan.title_create")}</h3>
      <div class="form-group"><label>${t("form.plan.name")} ${tip("Unique plan name. Used when assigning keys to plans.")}</label><input id="m-plan-name" value="${esc(p.name || "")}" ${p.name ? "readonly" : ""} required></div>
      <div class="form-group"><label>${t("form.plan.concurrency")} ${tip("Maximum simultaneous requests per key in this plan. Leave empty for unlimited.")}</label><input id="m-plan-concurrency" type="number" value="${p.concurrency_limit || ""}"></div>
      <div class="form-group"><label>${t("form.plan.rpm")} ${tip("Maximum requests per minute per key. Leave empty for unlimited.")}</label><input id="m-plan-rpm" type="number" value="${p.rpm_limit || ""}"></div>
      <div class="form-group"><label>${t("form.plan.windows")} ${tip("Custom time windows as JSON array: [[count, seconds], ...]. E.g. [[100,18000]] = 100 requests per 5 hours. Each request's quota consumption is multiplied by the model's Quota Ratio.")}</label><textarea id="m-plan-windows" rows="2">${JSON.stringify(p.window_limits || [])}</textarea></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-plan-submit">${p.name ? t("action.update") : t("action.create")}</button>
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
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  window._editPlan = async (name) => {
    try {
      const data = await api("/admin/plans");
      const p = (data.plans || []).find((x) => x.name === name);
      if (!p) return;
      showNewPlanModal(p);
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  function showNewKeyModal() {
    showModal(`
      <h3>${t("form.key.title_create")}</h3>
      <div class="form-group"><label>${t("form.key.alias")} ${tip("Short unique identifier for this key, e.g. 'alice' or 'team-api'. Used for display in dashboard and debug logging.")}</label><input id="m-key-alias"></div>
      <div class="form-group"><label>${t("form.key.user_id")} ${tip("Optional user identifier for tracking.")}</label><input id="m-key-user"></div>
      <div class="form-group"><label>${t("form.key.team")} ${tip("Optional team assignment.")}</label><select id="m-key-team"><option value="">${t("common.none_option")}</option></select></div>
      <div class="form-group"><label>${t("form.key.models")} ${tip("Select model access. Check 'all-team-models' for full access, or pick specific models.")}</label><div class="model-check-combo" id="m-key-models-combo"></div></div>
      <div class="form-group"><label>${t("form.key.max_budget")} ${tip("Maximum budget in USD. Leave empty for unlimited.")}</label><input id="m-key-budget" type="number" step="0.01"></div>
      <div class="form-group"><label>${t("form.key.rpm")} ${tip("Per-key RPM override. Leave empty to use plan or default limits.")}</label><input id="m-key-rpm" type="number"></div>
      <div class="form-group"><label>${t("form.key.plan")} ${tip("Assign this key to a rate limit plan. Leave empty for default plan.")}</label><select id="m-key-plan"><option value="">${t("common.default_option")}</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-key-submit">${t("action.create")}</button>
      </div>
    `);
    // Populate model checkbox combo
    getModelNames().then((names) => {
      const container = document.getElementById("m-key-models-combo");
      if (container) initModelCombo(container, [], names);
    });
    // Populate team dropdown — /admin/teams is POST-only, so teams list comes
    // from the quota overview cache (set by loadQuotaOverview). Fall back to
    // fetching overview on demand if cache is cold.
    const populateTeams = async () => {
      const sel = document.getElementById("m-key-team");
      if (!sel) return;
      let teams = window._teams;
      if (!teams) {
        try {
          const data = await api("/admin/quota/overview");
          teams = data.teams || [];
          window._teams = teams;
        } catch { teams = []; }
      }
      teams.forEach((t) => {
        const o = document.createElement("option");
        o.value = t.team_id;
        o.textContent = t.team_alias || t.team_id;
        sel.appendChild(o);
      });
    };
    populateTeams();
    getPlanNames().then((names) => {
      const sel = document.getElementById("m-key-plan");
      if (sel) names.forEach((n) => { const o = document.createElement("option"); o.value = n; o.textContent = n; sel.appendChild(o); });
    });
    document.getElementById("m-key-submit").addEventListener("click", async () => {
      try {
        const modelsVal = getComboModels("m-key-models-combo");
        const data = await api("/admin/keys", {
          method: "POST",
          body: JSON.stringify({
            key_alias: document.getElementById("m-key-alias").value.trim() || null,
            user_id: document.getElementById("m-key-user").value || null,
            team_id: document.getElementById("m-key-team").value || null,
            models: modelsVal || ["all-team-models"],
            max_budget: document.getElementById("m-key-budget").value ? Number(document.getElementById("m-key-budget").value) : null,
            rpm_limit: document.getElementById("m-key-rpm").value ? Number(document.getElementById("m-key-rpm").value) : null,
            plan_name: document.getElementById("m-key-plan").value || null,
          }),
        });
        const rawKey = data.key;
        hideModal();
        showModal(`
          <h3>${t("form.key.created_title")}</h3>
          <p class="key-warning">${t("form.key.copy_warning")}</p>
          <div class="key-display">${esc(rawKey)}</div>
          <div class="modal-actions" style="justify-content:space-between">
            <button class="btn-secondary btn-inline" onclick="window._copyText(this,'${esc(rawKey)}')">${t("action.copy")}</button>
            <button class="btn-primary" onclick="hideModal(); window._loadKeysPage();">${t("action.done")}</button>
          </div>
        `);
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  function showEditKeyModal(key) {
    const existingModels = Array.isArray(key.models) ? key.models : [];
    const isVip = key.metadata && key.metadata.vip === true;
    const isPromptLogExcluded = (window._promptLogExcludedKeys || []).includes(key.token_hash);
    showModal(`
      <h3>${t("form.key.title_edit")}</h3>
      <div class="form-group"><label>${t("form.key.alias")} ${tip("Short unique identifier for this key, e.g. 'alice'. Used for dashboard display and debug logging.")}</label><input id="m-edit-alias" value="${esc(key.key_alias || "")}"></div>
      <div class="form-group"><label>${t("form.key.user_id")} ${tip("Optional user identifier.")}</label><input id="m-edit-user" value="${esc(key.user_id || "")}"></div>
      <div class="form-group"><label>${t("form.key.models")} ${tip("Select model access. Check 'all-team-models' for full access, or pick specific models.")}</label><div class="model-check-combo" id="m-edit-models-combo"></div></div>
      <div class="form-group"><label>${t("form.key.max_budget")} ${tip("Maximum budget in USD. Leave empty for unlimited.")}</label><input id="m-edit-budget" type="number" step="0.01" value="${key.max_budget != null ? key.max_budget : ""}"></div>
      <div class="form-group"><label>${t("form.key.rpm")} ${tip("Per-key RPM override. Leave empty to use plan limits.")}</label><input id="m-edit-rpm" type="number" value="${key.rpm_limit || ""}"></div>
      <div class="form-group"><label>${t("form.key.plan_locked")} ${tip("Rate limit plan assigned to this key. Change via Assignments page.")}</label><input value="${esc(key.plan_name || t("common.default"))}" readonly style="background:var(--surface3);cursor:not-allowed"></div>
      <div class="form-group"><label>${t("form.key.vip")} ${tip("VIP keys get priority in flow control queues when deployments are at capacity.")}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-vip" ${isVip ? "checked" : ""}><span style="font-weight:600;color:#b45309;white-space:nowrap">${t("form.key.vip_label")}</span></div></div>
      <div class="form-group"><label>${t("form.key.prompt_log")} ${tip("Disable prompt logging for this key. When the global prompt log switch is ON, this key will be excluded from capture.")}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-no-prompt-log" ${isPromptLogExcluded ? "checked" : ""}><span style="font-weight:600;color:#dc2626;white-space:nowrap">${t("form.key.prompt_log_label")}</span></div></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-edit-submit">${t("action.save")}</button>
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
          models: modelsVal || ["all-team-models"],
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
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  function showNewAssignmentModal() {
    showModal(`
      <h3>${t("form.asgn.title")}</h3>
      <div class="form-group"><label>${t("form.asgn.key")} ${tip("Search by key alias or token prefix, then select from the list.")}</label>
        <input id="m-asgn-search" placeholder="${t("common.search_keys_ph")}" autocomplete="off">
        <div id="m-asgn-key-list" class="key-select-list"></div>
        <input type="hidden" id="m-asgn-hash">
      </div>
      <div class="form-group"><label>${t("form.asgn.plan")} ${tip("Select an existing plan to assign this key to.")}</label><select id="m-asgn-plan" required><option value="">${t("common.select_plan")}</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-asgn-submit">${t("action.assign")}</button>
      </div>
    `);
    // Populate plan dropdown
    getPlanNames().then((names) => {
      const sel = document.getElementById("m-asgn-plan");
      if (sel) names.forEach((n) => { const o = document.createElement("option"); o.value = n; o.textContent = n; sel.appendChild(o); });
    });
    // Key search: use backend API for consistent, unlimited search
    const searchInput = document.getElementById("m-asgn-search");
    const listEl = document.getElementById("m-asgn-key-list");
    const hashInput = document.getElementById("m-asgn-hash");
    let searchTimer = null;

    function renderKeyResults(keys) {
      if (keys.length === 0) {
        listEl.innerHTML = '<div class="key-select-empty">' + t("common.no_matching_keys") + '</div>';
        return;
      }
      listEl.innerHTML = keys.map((k) => {
        const alias = k.key_alias || t("assignments.no_alias");
        const prefix = k.token_prefix || (k.token_hash || "").substring(0, 12) + "...";
        return `<div class="key-select-item" data-hash="${esc(k.token_hash)}" data-alias="${esc(k.key_alias || "")}" data-prefix="${esc(k.token_prefix || "")}">
          <span class="key-select-alias">${esc(alias)}</span>
          <span class="mono muted">${esc(prefix)}</span>
        </div>`;
      }).join("");
      listEl.querySelectorAll(".key-select-item").forEach((el) => {
        el.addEventListener("click", () => {
          hashInput.value = el.dataset.hash;
          searchInput.value = el.dataset.alias || el.dataset.prefix || el.dataset.hash.substring(0, 12) + "...";
          listEl.innerHTML = "";
        });
      });
    }

    searchInput.addEventListener("input", () => {
      const q = searchInput.value.trim();
      hashInput.value = "";
      clearTimeout(searchTimer);
      if (!q) { listEl.innerHTML = ""; return; }
      searchTimer = setTimeout(async () => {
        try {
          const data = await api(`/admin/keys?per_page=12&search=${encodeURIComponent(q)}`);
          renderKeyResults(data.keys || []);
        } catch (err) {
          listEl.innerHTML = '<div class="key-select-empty">' + t("common.search_failed") + '</div>';
        }
      }, 200);
    });

    document.getElementById("m-asgn-submit").addEventListener("click", async () => {
      if (!hashInput.value) { alert(t("confirm.select_key")); return; }
      try {
        await api("/admin/assignments", {
          method: "POST",
          body: JSON.stringify({
            key_hash: hashInput.value,
            plan_name: document.getElementById("m-asgn-plan").value,
          }),
        });
        hideModal();
        loadAssignments();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  // ── Admin: Quota Management ───────────────────────────
  // Two views share the same #admin-quota section:
  //   #/admin/quota                       → overview (team cards)
  //   #/admin/quota/team/{team_id}        → team detail (paginated keys)
  //   #/admin/quota/unassigned            → unassigned keys (paginated)

  let quotaKeysPage = 1;
  let quotaKeysSearch = "";
  let quotaKeysSort = "cost";
  let quotaKeysSearchTimer = null;

  async function loadQuota() {
    const hash = location.hash || "";
    if (hash.includes("/admin/quota/team/")) {
      const m = hash.match(/\/admin\/quota\/team\/([^/?#]+)/);
      const tid = m ? decodeURIComponent(m[1]) : null;
      if (tid) { renderQuotaTeamDetail(tid); return; }
    } else if (hash.includes("/admin/quota/unassigned")) {
      renderQuotaUnassigned();
      return;
    }
    loadQuotaOverview();
  }

  async function loadQuotaOverview() {
    const wrap = document.getElementById("quota-overview-wrap");
    if (!wrap) return;
    wrap.innerHTML = '<p class="loading">' + t("common.loading") + '</p>';
    try {
      // Load prompt-log status alongside — to know which teams are excluded.
      try {
        const plData = await api("/admin/prompt-log/status");
        window._promptLogExcludedTeams = plData.excluded_teams || [];
      } catch { window._promptLogExcludedTeams = []; }
      // Cache default_team_plan + explicit assignments for the create/edit modal.
      // We don't have list_teams anymore — derive from quota overview teams.
      const data = await api("/admin/quota/overview");
      const assignments = {};
      const defaultTeamPlan = (data.default_team_plan) || null;
      (data.teams || []).forEach((tm) => {
        if (tm.plan_explicit && tm.plan_name) assignments[tm.team_id] = tm.plan_name;
      });
      window._teamPlanState = { default_team_plan: defaultTeamPlan, assignments };
      // Cache teams list for the key-creation modal dropdown.
      window._teams = data.teams || [];
      // Cache the team-plan names list once for the plan dropdowns.
      if (!window._teamPlanNames) {
        try {
          window._teamPlanNames = await getTeamPlanNames();
        } catch { window._teamPlanNames = []; }
      }
      renderQuotaOverview(data);
    } catch (err) {
      wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("quota.title"), message: esc(err.message) })}</p>`;
    }
  }

  // Render team plan spec as a single line of compact tags.
  // Every plan-configurable dimension is shown; unset ones display "∞"
  // so the operator can distinguish "unlimited" from "not applicable".
  function renderTeamEffectiveLimits(el) {
    if (!el) return "";
    const INF = "∞";
    const fmtOrInf = (v, fmt) => (v == null ? INF : fmt(v));
    const fmtNum = (v) => formatNumber(Number(v));
    const fmtCost = (v) => "$" + (Number(v) || 0).toLocaleString(undefined, { maximumFractionDigits: 2 });

    // Aggregate window_limits by secs → {counts, tokens, costs}.
    // rpm_limit / tpm_limit are 60s-window shorthands; merge them into the 60s
    // bucket if not already set there (so we render one combined 60s tag).
    const bySecs = new Map();
    (el.window_limits || []).forEach((w) => {
      const [counts, tokens, costs, secs] = w;
      if (!bySecs.has(secs)) bySecs.set(secs, {});
      const e = bySecs.get(secs);
      if (counts != null) e.counts = counts;
      if (tokens != null) e.tokens = tokens;
      if (costs != null) e.costs = costs;
    });
    // Ensure a 60s bucket always exists — RPM/TPM live there. If plan didn't
    // configure either and no 60s window_limits entry exists, we still show
    // the 60s slot as "∞" so the operator sees that short-window dimension
    // is unrestricted (rather than silently absent).
    if (!bySecs.has(60)) bySecs.set(60, {});
    const min60 = bySecs.get(60);
    if (min60.counts == null && el.rpm_limit != null) min60.counts = el.rpm_limit;
    if (min60.tokens == null && el.tpm_limit != null) min60.tokens = el.tpm_limit;

    const tags = [];
    // Concurrency
    tags.push(`<span class="plan-tag"><span class="plan-tag-label">${esc(t("plan.dim.concurrency"))}</span><span class="plan-tag-value">${fmtOrInf(el.concurrency_limit, fmtNum)}</span></span>`);
    // Windows sorted by secs ascending (always includes 60s due to above).
    const sortedSecs = [...bySecs.keys()].sort((a, b) => a - b);
    sortedSecs.forEach((secs) => {
      const e = bySecs.get(secs);
      const label = secs === 60 ? "60s" : formatDuration(secs);
      const parts = [];
      if (e.counts != null) parts.push(`${fmtNum(e.counts)} ${t("quota.col.counts_short") || "req"}`);
      if (e.tokens != null) parts.push(`${fmtNum(e.tokens)} tok`);
      if (e.costs != null) parts.push(fmtCost(e.costs));
      tags.push(`<span class="plan-tag"><span class="plan-tag-label">${esc(label)}</span><span class="plan-tag-value">${esc(parts.join(" / ") || INF)}</span></span>`);
    });
    // Cumulative totals
    tags.push(`<span class="plan-tag"><span class="plan-tag-label">${esc(t("plan.dim.total_tokens"))}</span><span class="plan-tag-value">${fmtOrInf(el.total_token_limit, fmtNum)}</span></span>`);
    tags.push(`<span class="plan-tag"><span class="plan-tag-label">${esc(t("plan.dim.total_cost") || "Total Cost")}</span><span class="plan-tag-value">${fmtOrInf(el.total_cost_limit, fmtCost)}</span></span>`);

    return `<div class="team-limits-block">
      <div class="team-limits-title">${esc(t("quota.team_limits_title"))}</div>
      <div class="plan-tags">${tags.join("")}</div>
    </div>`;
  }

  // Render a <select> for plan choice. value="" = use default_team_plan.
  function renderTeamPlanSelect(teamId, currentPlan, isExplicit) {
    const names = window._teamPlanNames || [];
    const opts = [`<option value="">${esc(t("quota.team_plan_default"))}</option>`];
    names.forEach((n) => {
      const sel = (isExplicit && n === currentPlan) ? "selected" : "";
      opts.push(`<option value="${esc(n)}" ${sel}>${esc(n)}</option>`);
    });
    return `<select class="search-input" style="max-width:160px" onchange="window._changeTeamPlan('${esc(teamId)}', this.value)">
      ${opts.join("")}
    </select>`;
  }

  function renderQuotaOverview(data) {
    const wrap = document.getElementById("quota-overview-wrap");
    const teams = data.teams || [];
    const noTeam = data.no_team || {};

    const teamCard = (t1) => {
      const tokens = Number(t1.total_input_tokens || 0) + Number(t1.total_output_tokens || 0);
      const planBadge = t1.plan_name
        ? `<span class="badge ${t1.plan_explicit ? "badge-plan" : "badge-default"}">${esc(t1.plan_name)}</span>`
        : `<span class="muted">${esc(t("quota.team_plan_none"))}</span>`;
      const isExcluded = (window._promptLogExcludedTeams || []).includes(t1.team_id);
      const promptLogBtn = `<button class="btn-small ${isExcluded ? "btn-secondary" : "btn-primary"}" onclick="window._toggleTeamPromptLog('${esc(t1.team_id)}', ${isExcluded})">${isExcluded ? t("quota.prompt_log_off") : t("quota.prompt_log_on")}</button>`;
      const planSelect = renderTeamPlanSelect(t1.team_id, t1.plan_name, t1.plan_explicit);
      const limitsHtml = renderTeamEffectiveLimits(t1.effective_limits);
      return `<div class="quota-card">
        <div class="quota-card-header">
          <strong>${esc(t1.team_alias || t1.team_id)}</strong>
          ${planBadge}
        </div>
        <div class="quota-card-stats">
          <div><span class="muted">${esc(t("teams.col.keys_count") || "Keys")}</span> <strong>${t1.key_count || 0}</strong></div>
          <div><span class="muted">${esc(t("quota.col.tokens"))}</span> <strong>${formatNumber(tokens)}</strong></div>
          <div><span class="muted">${esc(t("quota.col.cost"))}</span> <strong>$${esc(t1.total_cost || "0")}</strong></div>
        </div>
        ${limitsHtml}
        <div class="quota-card-actions">
          <button class="btn-small" onclick="location.hash='#/admin/quota/team/${encodeURIComponent(t1.team_id)}'">${t("quota.view_detail")}</button>
          ${planSelect}
          ${promptLogBtn}
          <button class="btn-danger btn-small" onclick="window._deleteTeam('${esc(t1.team_id)}', ${t1.key_count || 0})">${t("action.delete")}</button>
          <select class="search-input" style="max-width:140px" onchange="window._quotaTeamAction('${esc(t1.team_id)}', this.value); this.value=''">
            <option value="">${esc(t("quota.actions_label"))}</option>
            <option value="cumulative">${esc(t("quota.reset_cumulative"))}</option>
            <option value="windows">${esc(t("quota.reset_windows"))}</option>
            <option value="all">${esc(t("quota.reset_all"))}</option>
          </select>
        </div>
      </div>`;
    };

    let html = "";
    if (teams.length > 0) {
      html += `<h3 class="quota-section-title">${t("quota.teams_header")}</h3>`;
      html += `<div class="quota-grid">${teams.map(teamCard).join("")}</div>`;
    }

    // No-team card.
    const ntTokens = Number(noTeam.total_input_tokens || 0) + Number(noTeam.total_output_tokens || 0);
    html += `<h3 class="quota-section-title">${t("quota.no_team_header")}</h3>`;
    html += `<div class="quota-grid"><div class="quota-card">
      <div class="quota-card-header">
        <strong>${t("quota.no_team_header")}</strong>
      </div>
      <div class="quota-card-stats">
        <div><span class="muted">${esc(t("teams.col.keys_count") || "Keys")}</span> <strong>${noTeam.key_count || 0}</strong></div>
        <div><span class="muted">${esc(t("quota.col.tokens"))}</span> <strong>${formatNumber(ntTokens)}</strong></div>
        <div><span class="muted">${esc(t("quota.col.cost"))}</span> <strong>$${esc(noTeam.total_cost || "0")}</strong></div>
      </div>
      <div class="quota-card-actions">
        <button class="btn-small" onclick="location.hash='#/admin/quota/unassigned'">${t("quota.view_detail")}</button>
      </div>
    </div></div>`;

    wrap.innerHTML = html;
  }

  async function renderQuotaTeamDetail(teamId) {
    await _renderQuotaKeysView({ teamId, titleSuffix: t("quota.teams_header") });
  }

  async function renderQuotaUnassigned() {
    await _renderQuotaKeysView({ teamId: null, titleSuffix: t("quota.no_team_header") });
  }

  async function _renderQuotaKeysView({ teamId, titleSuffix }) {
    const wrap = document.getElementById("quota-overview-wrap");
    if (!wrap) return;
    quotaKeysPage = 1;
    quotaKeysSearch = "";

    const backHref = "#/admin/quota";
    wrap.innerHTML = `
      <div class="section-header">
        <h3>${esc(titleSuffix)} ${teamId ? "· " + esc(teamId) : ""}</h3>
        <a href="${backHref}" class="btn-small" style="text-decoration:none;display:inline-block">${t("quota.back_to_overview")}</a>
      </div>
      <div class="keys-toolbar" style="margin-bottom:12px">
        <input type="text" id="quota-keys-search" placeholder="${esc(t("keys.search_placeholder"))}" class="search-input">
        <select id="quota-keys-sort" class="search-input" style="max-width:160px">
          <option value="cost">${t("quota.sort.cost")}</option>
          <option value="tokens">${t("quota.sort.tokens")}</option>
          <option value="alias">${t("quota.sort.alias")}</option>
        </select>
        ${teamId ? `<select class="search-input" style="max-width:140px" id="quota-team-action-top" onchange="window._quotaTeamAction('${esc(teamId)}', this.value); this.value=''">
          <option value="">${esc(t("quota.actions_label"))}</option>
          <option value="cumulative">${esc(t("quota.reset_cumulative"))}</option>
          <option value="windows">${esc(t("quota.reset_windows"))}</option>
          <option value="all">${esc(t("quota.reset_all"))}</option>
        </select>` : ""}
      </div>
      <div id="quota-keys-table-wrap"></div>
      <div id="quota-keys-pagination" class="pagination"></div>
    `;

    document.getElementById("quota-keys-search").addEventListener("input", (e) => {
      clearTimeout(quotaKeysSearchTimer);
      quotaKeysSearchTimer = setTimeout(() => {
        quotaKeysSearch = e.target.value.trim();
        quotaKeysPage = 1;
        _loadQuotaKeys(teamId);
      }, 300);
    });
    document.getElementById("quota-keys-sort").value = quotaKeysSort;
    document.getElementById("quota-keys-sort").addEventListener("change", (e) => {
      quotaKeysSort = e.target.value;
      quotaKeysPage = 1;
      _loadQuotaKeys(teamId);
    });

    await _loadQuotaKeys(teamId);
  }

  async function _loadQuotaKeys(teamId) {
    const wrap = document.getElementById("quota-keys-table-wrap");
    if (!wrap) return;
    wrap.innerHTML = '<p class="loading">' + t("common.loading") + '</p>';
    let url = teamId
      ? `/admin/quota/team/${encodeURIComponent(teamId)}?page=${quotaKeysPage}&per_page=50&sort=${encodeURIComponent(quotaKeysSort)}`
      : `/admin/quota/unassigned?page=${quotaKeysPage}&per_page=50&sort=${encodeURIComponent(quotaKeysSort)}`;
    if (quotaKeysSearch) url += `&search=${encodeURIComponent(quotaKeysSearch)}`;
    try {
      const data = await api(url);
      _renderQuotaKeysTable(data, teamId);
      _renderQuotaKeysPagination(data);
    } catch (err) {
      wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("quota.title"), message: esc(err.message) })}</p>`;
    }
  }

  function _renderQuotaKeysTable(data, teamId) {
    const wrap = document.getElementById("quota-keys-table-wrap");
    const keys = data.keys || [];
    if (keys.length === 0) {
      wrap.innerHTML = "<p>" + (teamId ? t("quota.empty") : t("quota.empty_no_team")) + "</p>";
      return;
    }
    wrap.innerHTML = `<table>
      <tr>
        <th>${t("quota.col.alias")}</th>
        <th>${t("quota.col.plan")}</th>
        <th>${t("quota.col.concurrency")}</th>
        <th>${t("quota.col.tokens")}</th>
        <th>${t("quota.col.cost")}</th>
        <th>${t("quota.col.actions")}</th>
      </tr>
      ${keys.map((k) => {
        const tokens = Number(k.total_input_tokens || 0) + Number(k.total_output_tokens || 0);
        return `<tr class="quota-key-row" data-token="${esc(k.token)}">
          <td>
            <strong>${esc(k.key_alias || k.key_name || k.token_prefix)}</strong>
            ${k.user_id ? `<br><span class="muted">${esc(k.user_id)}</span>` : ""}
            ${k.blocked ? `<br><span class="badge-danger">blocked</span>` : ""}
          </td>
          <td>${esc(k.plan_name || "-")}</td>
          <td>${k.concurrency || 0}</td>
          <td>${formatNumber(tokens)}</td>
          <td>$${esc(k.total_cost || "0")}</td>
          <td>
            <button class="btn-small" onclick="window._quotaToggleWindows('${esc(k.token)}', this)">${t("quota.expand_windows")}</button>
            <select class="search-input" style="max-width:120px" onchange="window._quotaKeyAction('${esc(k.token)}', this.value); this.value=''">
              <option value="">${esc(t("quota.actions_label"))}</option>
              <option value="cumulative">${esc(t("quota.reset_cumulative"))}</option>
              <option value="windows">${esc(t("quota.reset_windows"))}</option>
              <option value="all">${esc(t("quota.reset_all"))}</option>
            </select>
          </td>
        </tr>
        <tr class="quota-windows-row" id="quota-windows-${esc(k.token)}" style="display:none">
          <td colspan="6"><div class="quota-windows-content"><span class="loading">${esc(t("common.loading"))}</span></div></td>
        </tr>`;
      }).join("")}
    </table>`;
  }

  function _renderQuotaKeysPagination(data) {
    const el = document.getElementById("quota-keys-pagination");
    if (!el) return;
    const pages = Math.ceil((data.total || 0) / (data.per_page || 50));
    if (pages <= 1) { el.innerHTML = ""; return; }
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._quotaKeysPrev()">&lt;</button>
      <span>${t("common.page_of", { page: data.page, total: pages, count: data.total, unit: t("keys.title") })}</span>
      <button ${data.page >= pages ? "disabled" : ""} onclick="window._quotaKeysNext()">&gt;</button>
    `;
  }

  window._quotaKeysPrev = () => { quotaKeysPage = Math.max(1, quotaKeysPage - 1); _loadQuotaKeys(_currentQuotaTeamId()); };
  window._quotaKeysNext = () => { quotaKeysPage += 1; _loadQuotaKeys(_currentQuotaTeamId()); };
  function _currentQuotaTeamId() {
    const hash = location.hash || "";
    const m = hash.match(/\/admin\/quota\/team\/([^/?#]+)/);
    return m ? decodeURIComponent(m[1]) : null;
  }

  window._quotaToggleWindows = async (token, btn) => {
    const row = document.getElementById("quota-windows-" + token);
    if (!row) return;
    const showing = row.style.display !== "none";
    if (showing) {
      row.style.display = "none";
      btn.textContent = t("quota.expand_windows");
      return;
    }
    row.style.display = "";
    btn.textContent = t("quota.collapse_windows");
    const content = row.querySelector(".quota-windows-content");
    content.innerHTML = '<span class="loading">' + t("common.loading") + '</span>';
    try {
      const data = await api(`/admin/quota/key/${encodeURIComponent(token)}/windows`);
      content.innerHTML = _renderWindowsBlock(data);
    } catch (err) {
      content.innerHTML = `<span class="error-msg">${esc(err.message)}</span>`;
    }
  };

  // Reuse the user-side multi-dim window card layout.
  function _renderWindowsBlock(data) {
    const windows = data.windows || [];
    if (windows.length === 0) return '<p class="muted">' + t("req.no_active_windows") + '</p>';
    return windows.map((w) => {
      const isRpm = w.window_secs === 60;
      const label = isRpm ? "RPM (60s)" : t("plan.window_limit_label", { duration: formatDuration(w.window_secs) });
      const remaining = w.remaining_secs || 0;
      const dims = w.dims || {};
      const dimKeys = ["counts", "tokens", "costs"].filter((k) => dims[k]);
      const dimHtml = dimKeys.map((k) => {
        const d = dims[k];
        let cur, limit, display;
        if (k === "costs") {
          cur = Number(d.current_micros || 0);
          limit = Number(d.limit_micros || 0);
          display = "$" + (d.current || "0") + " / " + (limit > 0 ? "$" + (d.limit || "0") : t("common.unlimited"));
        } else {
          cur = Number(d.current || 0);
          limit = Number(d.limit || 0);
          display = formatNumber(cur) + " / " + (limit > 0 ? formatNumber(limit) : t("common.unlimited"));
        }
        const pct = limit > 0 ? Math.min(100, (cur / limit) * 100) : 0;
        const cls = pct >= 90 ? "danger" : pct >= 70 ? "warn" : "";
        return `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim." + k))}</span>
            <div class="progress-bar"><div class="progress-fill ${cls}" style="width:${pct}%"></div></div>
            <span class="dim-value">${esc(display)}</span>
          </div>`;
      }).join("");
      return `<div class="usage-limit-card usage-limit-wide">
        <div class="usage-limit-title">${esc(label)}</div>
        ${dimHtml}
        <div class="usage-limit-reset">${t("req.resets_in", { time: formatCountdown(remaining) })}</div>
      </div>`;
    }).join("");
  }

  window._quotaKeyAction = async (token, scope) => {
    if (!scope) return;
    const confirmKey = scope === "cumulative"
      ? "quota.confirm_reset_key_cumulative"
      : scope === "windows"
      ? "quota.confirm_reset_key_windows"
      : "quota.confirm_reset_key_all";
    if (!confirm(t(confirmKey))) return;
    const suffix = scope === "all" ? "" : `/${scope}`;
    try {
      await api(`/admin/quota/reset/key/${encodeURIComponent(token)}${suffix}`, { method: "POST" });
      const tid = _currentQuotaTeamId();
      _loadQuotaKeys(tid);
    } catch (err) {
      alert("Reset failed: " + err.message);
    }
  };

  window._quotaTeamAction = async (teamId, scope) => {
    if (!scope) return;
    const confirmKey = scope === "cumulative"
      ? "quota.confirm_reset_team_cumulative"
      : scope === "windows"
      ? "quota.confirm_reset_team_windows"
      : "quota.confirm_reset_team_all";
    if (!confirm(t(confirmKey))) return;
    const suffix = scope === "all" ? "" : `/${scope}`;
    try {
      await api(`/admin/quota/reset/team/${encodeURIComponent(teamId)}${suffix}`, { method: "POST" });
      // Refresh either detail page (if on team detail) or overview (if on overview).
      if (location.hash.includes("/admin/quota/team/")) {
        _loadQuotaKeys(teamId);
      } else {
        loadQuotaOverview();
      }
    } catch (err) {
      alert("Reset failed: " + err.message);
    }
  };

  // ── Admin: Team helpers (cards live in quota overview) ──
  function formatTeamModels(models) {
    if (!models || models.length === 0) return "all-team-models";
    if (models.includes("all-team-models")) return "all-team-models";
    return models.join(", ");
  }

  window.showCreateTeamModal = function(prefill) {
    const p = prefill || {};
    // Explicit plan state cached from last quota_overview load.
    const tps = window._teamPlanState || { default_team_plan: null, assignments: {} };
    const currentExplicit = p.team_id ? (tps.assignments[p.team_id] || "") : "";
    showModal(`
      <h3>${p.team_id ? t("form.team.title_edit") : t("form.team.title_create")}</h3>
      <div class="form-group"><label>${t("form.team.id")} ${tip("Unique identifier for this team. Cannot be changed after creation.")}</label><input id="m-team-id" value="${esc(p.team_id || "")}" ${p.team_id ? "readonly" : ""} required></div>
      <div class="form-group"><label>${t("form.team.alias")} ${tip("Display name for this team. Can be non-unique.")}</label><input id="m-team-alias" value="${esc(p.team_alias || "")}"></div>
      <div class="form-group"><label>${t("form.team.models")} ${tip("Select model access for this team. Check 'all-team-models' for full access to all current and future models, or pick specific models.")}</label><div class="model-check-combo" id="m-team-models-combo"></div></div>
      <div class="form-group"><label>${t("teams.col.plan")} ${tip("Pick a type=team plan, or leave on default to fall back to default_team_plan (YAML-configured).")}</label>
        <select id="m-team-plan">
          <option value="">${esc(t("teams.plan_use_default"))}${tps.default_team_plan ? " (" + tps.default_team_plan + ")" : ""}</option>
        </select>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-team-submit">${p.team_id ? t("action.update") : t("action.create")}</button>
      </div>
    `);
    getModelNames().then((names) => {
      const container = document.getElementById("m-team-models-combo");
      if (container) initModelCombo(container, p.models || [], names);
    });
    getTeamPlanNames().then((names) => {
      const sel = document.getElementById("m-team-plan");
      if (!sel) return;
      names.forEach((n) => {
        const o = document.createElement("option");
        o.value = n;
        o.textContent = n;
        if (n === currentExplicit) o.selected = true;
        sel.appendChild(o);
      });
    });
    document.getElementById("m-team-submit").addEventListener("click", async () => {
      try {
        const modelsVal = getComboModels("m-team-models-combo");
        const body = {
          team_id: document.getElementById("m-team-id").value.trim(),
          team_alias: document.getElementById("m-team-alias").value.trim() || null,
          models: modelsVal || ["all-team-models"],
        };
        const selectedPlan = document.getElementById("m-team-plan").value;
        if (p.team_id) {
          await api("/admin/teams/" + encodeURIComponent(p.team_id), {
            method: "PUT",
            body: JSON.stringify({
              team_alias: body.team_alias,
              models: body.models,
            }),
          });
          if (selectedPlan && selectedPlan !== currentExplicit) {
            await api("/admin/team-assignments", {
              method: "POST",
              body: JSON.stringify({ team_id: p.team_id, plan_name: selectedPlan }),
            });
          } else if (!selectedPlan && currentExplicit) {
            await api("/admin/team-assignments/" + encodeURIComponent(p.team_id), {
              method: "DELETE",
            });
          }
        } else {
          await api("/admin/teams", { method: "POST", body: JSON.stringify(body) });
          if (selectedPlan) {
            await api("/admin/team-assignments", {
              method: "POST",
              body: JSON.stringify({ team_id: body.team_id, plan_name: selectedPlan }),
            });
          }
        }
        hideModal();
        loadQuotaOverview();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  };

  // Plan dropdown onchange: POST or DELETE the team-assignment.
  // value="" means "use default_team_plan" (delete explicit assignment).
  window._changeTeamPlan = async (teamId, value) => {
    try {
      if (value) {
        await api("/admin/team-assignments", {
          method: "POST",
          body: JSON.stringify({ team_id: teamId, plan_name: value }),
        });
      } else {
        await api("/admin/team-assignments/" + encodeURIComponent(teamId), {
          method: "DELETE",
        });
      }
      loadQuotaOverview();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  window._deleteTeam = async (teamId, keyCount) => {
    if (keyCount > 0) {
      alert(t("alert.cannot_delete_team", { count: keyCount }));
      return;
    }
    if (!confirm(t("confirm.delete_team", { name: teamId }))) return;
    try {
      await api("/admin/teams/" + encodeURIComponent(teamId), { method: "DELETE" });
      loadQuotaOverview();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  window._toggleTeamPromptLog = async (teamId, isExcluded) => {
    try {
      await api('/admin/prompt-log/team', {
        method: 'POST',
        body: JSON.stringify({ team_id: teamId, excluded: !isExcluded }),
      });
      loadQuotaOverview();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
      if (tbody) tbody.innerHTML = `<tr><td colspan="14" class="no-results">${t("logs.failed", { message: esc(err.message) })}</td></tr>`;
    }
  }

  function renderLogsTable(logs) {
    const tbody = document.getElementById("logs-tbody");
    if (!tbody) return;
    if (logs.length === 0) {
      tbody.innerHTML = '<tr><td colspan="14" class="no-results">' + t("common.no_matching", { what: t("logs.title").toLowerCase() }) + '</td></tr>';
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
          ? '<button class="btn-small" onclick="window._viewPromptLog(\'' + esc(l.request_id) + '\',\'' + esc(l.key_hash) + '\',\'' + esc(l.team_alias || "") + '\')">' + t("action.view") + '</button>'
          : "-";
        // Timestamp: muted mono badge
        var tsCell = '<span class="log-ts">' + formatTimestamp(l.created_at) + '</span>';
        // IP: special mono style
        var ipCell = '<span class="log-ip">' + esc(l.client_ip || "-") + '</span>';
        // Model: split into model_name + deployment_id
        var modelVal = esc(l.model || "-");
        var modelCell;
        if (l.model && l.model.includes(":")) {
          var parts = l.model.split(":");
          modelCell = '<span class="log-model-name">' + esc(parts[0]) + '</span>' +
            '<span class="log-model-sep">:</span>' +
            '<span class="log-model-deploy">' + esc(parts.slice(1).join(":")) + '</span>';
        } else {
          modelCell = '<span class="log-model-name">' + modelVal + '</span>';
        }
        // Prefix hit rate = cached_tokens / input_tokens * 100, truncated to
        // 1 decimal and capped at 99.9 (no rounding to 100%). "-" if missing.
        var kvCell = (l.cached_tokens != null && l.input_tokens > 0)
          ? esc(fmtPrefixHit(l.cached_tokens / l.input_tokens * 100))
          : "-";
        // IN/OUT: show input and output tokens in one cell, slash-separated.
        var inOutCell = (l.input_tokens != null || l.output_tokens != null)
          ? formatNumber(l.input_tokens) + " / " + formatNumber(l.output_tokens)
          : "- / -";
        return `<tr>
        <td>${tsCell}</td>
        <td>${ipCell}</td>
        <td>${esc(l.team_alias || l.team_id || "-")}</td>
        <td>${esc(l.key_alias || l.key_name || "-")}</td>
        <td>${modelCell}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? t("common.yes") : t("common.no")}</td>
        <td class="mono">${inOutCell}</td>
        <td class="mono">${kvCell}</td>
        <td>${l.duration_ms != null ? l.duration_ms + "ms" : "-"}</td>
        <td>${l.ttft_ms != null ? l.ttft_ms + "ms" : "-"}</td>
        <td>${errorCell}</td>
        <td>${detailCell}</td>
      </tr>`;
    }).join("");
  }

  function renderLogsPagination(data) {
    const el = document.getElementById("logs-pagination");
    el.innerHTML = `
      <button ${data.page <= 1 ? "disabled" : ""} onclick="window._loadLogsPage(${data.page - 1})">&lt;</button>
      <span>${t("common.page_only", { page: data.page })}</span>
      <button ${!data.has_next ? "disabled" : ""} onclick="window._loadLogsPage(${data.page + 1})">&gt;</button>
    `;
  }

  window._loadLogsPage = (p) => loadLogs(p);

  // ── Prompt Log Entry Viewer ──────────────────────────
  window._viewPromptLog = async function(requestId, keyHash, teamAlias) {
    const overlay = document.getElementById("modal-overlay");
    const modalEl = overlay ? overlay.querySelector(".modal") : null;
    // Widen modal for JSON viewing via CSS class (cleared on hideModal).
    if (modalEl) modalEl.classList.add("modal-wide");
    showModal('<div style="text-align:center;padding:40px">' + t("common.loading") + '</div>');
    try {
      const params = new URLSearchParams({ key_hash: keyHash });
      if (teamAlias) params.set("team_alias", teamAlias);
      const data = await api("/admin/prompt-log/entry/" + encodeURIComponent(requestId) + "?" + params);
      const containerId = "plj-" + Date.now();
      showModal(
        '<div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:12px">' +
        '<h3 style="margin:0">' + t("logs.detail_title") + '</h3>' +
        '<div style="display:flex;gap:6px">' +
        '<button class="btn-small" id="' + containerId + '-collapse">' + t("action.collapse_all") + '</button>' +
        '<button class="btn-small" id="' + containerId + '-expand">' + t("action.expand_all") + '</button>' +
        '<button class="btn-small" id="' + containerId + '-raw">' + t("action.raw_json") + '</button>' +
        '</div></div>' +
        '<div id="' + containerId + '" style="max-height:72vh;overflow:auto;background:var(--surface2);color:var(--text);padding:16px;border-radius:8px;font-size:13px;line-height:1.5;font-family:var(--mono)"></div>' +
        '<pre id="' + containerId + '-rawpre" style="display:none;max-height:72vh;overflow:auto;background:var(--surface2);color:var(--text);padding:16px;border-radius:8px;font-size:13px;line-height:1.5;white-space:pre-wrap;word-break:break-word;font-family:var(--mono)">' + esc(JSON.stringify(data, null, 2)) + '</pre>'
      );
      const tree = document.getElementById(containerId);
      renderJsonTree(data, tree);
      document.getElementById(containerId + "-collapse").onclick = () => {
        tree.querySelectorAll(".jvt-toggle.open").forEach(el => el.click());
      };
      document.getElementById(containerId + "-expand").onclick = () => {
        tree.querySelectorAll(".jvt-toggle:not(.open)").forEach(el => el.click());
      };
      document.getElementById(containerId + "-raw").onclick = (e) => {
        const rawPre = document.getElementById(containerId + "-rawpre");
        const showing = rawPre.style.display !== "none";
        rawPre.style.display = showing ? "none" : "block";
        tree.style.display = showing ? "block" : "none";
        e.target.textContent = showing ? t("action.raw_json") : t("action.tree_view");
      };
    } catch (err) {
      showModal('<div style="padding:20px;color:var(--danger)">' + t("logs.failed_prompt", { message: esc(err.message) }) + '</div>');
    }
  };

  // JSON tree renderer with collapsible nodes.
  function renderJsonTree(val, container, depth) {
    depth = depth || 0;
    const maxDepth = 3; // auto-expand up to this depth
    if (val === null || val === undefined) {
      container.appendChild(document.createTextNode("null"));
      return;
    }
    if (typeof val === "boolean" || typeof val === "number") {
      container.appendChild(document.createTextNode(String(val)));
      return;
    }
    if (typeof val === "string") {
      // Long strings (likely content): truncate with expand
      if (val.length > 500) {
        const short = document.createElement("span");
        short.className = "jvt-str-preview";
        short.textContent = JSON.stringify(val.substring(0, 200)) + ' … (' + t("common.char_count", { n: val.length }) + ')';
        short.title = t("common.click_to_show");
        short.style.cursor = "pointer";
        short.style.color = "var(--info)";
        const full = document.createElement("span");
        full.className = "jvt-str-full";
        full.style.display = "none";
        full.textContent = JSON.stringify(val);
        short.onclick = () => { short.style.display = "none"; full.style.display = "inline"; };
        full.onclick = () => { full.style.display = "none"; short.style.display = "inline"; };
        full.style.cursor = "pointer";
        full.style.color = "var(--info)";
        container.appendChild(short);
        container.appendChild(full);
      } else {
        const s = document.createElement("span");
        s.style.color = "var(--info)";
        s.textContent = JSON.stringify(val);
        container.appendChild(s);
      }
      return;
    }
    const isArr = Array.isArray(val);
    const entries = isArr ? val.map((v, i) => [i, v]) : Object.entries(val);
    if (entries.length === 0) {
      container.appendChild(document.createTextNode(isArr ? "[]" : "{}"));
      return;
    }
    // Collapsible block
    const toggle = document.createElement("span");
    toggle.className = "jvt-toggle" + (depth < maxDepth ? " open" : "");
    toggle.textContent = depth < maxDepth ? "▼" : "▶";
    toggle.style.cursor = "pointer";
    toggle.style.userSelect = "none";
    toggle.style.marginRight = "4px";
    toggle.style.color = "var(--text3)";

    const summary = document.createElement("span");
    summary.className = "jvt-summary";
    summary.textContent = isArr ? "[" + t("common.items_count", { n: entries.length }) + "]" : "{" + t("common.keys_count", { n: entries.length }) + "}";
    summary.style.color = "var(--text3)";
    summary.style.marginRight = "4px";
    summary.style.display = depth < maxDepth ? "none" : "inline";

    const body = document.createElement("div");
    body.className = "jvt-body";
    body.style.display = depth < maxDepth ? "block" : "none";
    body.style.marginLeft = "16px";
    body.style.borderLeft = "1px solid var(--border)";
    body.style.paddingLeft = "8px";

    entries.forEach(function(entry) {
      var line = document.createElement("div");
      line.style.marginTop = "2px";
      if (!isArr) {
        var keySpan = document.createElement("span");
        keySpan.style.color = "var(--primary)";
        keySpan.textContent = JSON.stringify(entry[0]) + ": ";
        line.appendChild(keySpan);
      }
      renderJsonTree(entry[1], line, depth + 1);
      body.appendChild(line);
    });

    var closing = document.createElement("span");
    closing.className = "jvt-close";
    closing.textContent = isArr ? "]" : "}";
    closing.style.color = "var(--text3)";

    toggle.onclick = function() {
      var isOpen = toggle.classList.toggle("open");
      toggle.textContent = isOpen ? "▼" : "▶";
      body.style.display = isOpen ? "block" : "none";
      closing.style.display = isOpen ? "none" : "inline";
      summary.style.display = isOpen ? "none" : "inline";
    };

    container.appendChild(toggle);
    container.appendChild(summary);
    container.appendChild(document.createTextNode(isArr ? "[" : "{"));
    container.appendChild(body);
    container.appendChild(closing);
    container.appendChild(document.createTextNode("\n"));
  }

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
      <div class="mcc-display">${isFullAccess ? t("plans.full_access") : (existingModels || []).map((m) => esc(m)).join(", ") || t("plans.no_models")}</div>
      <div class="mcc-dropdown hidden">
        <label class="mcc-item mcc-item-all"><input type="checkbox" value="all-team-models" ${isFullAccess ? "checked" : ""}> ${t("plans.full_access")}</label>
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
        display.textContent = t("plans.full_access");
      } else {
        const selected = Array.from(modelCbs).filter((c) => c.checked).map((c) => c.value);
        display.textContent = selected.length > 0 ? selected.join(", ") : t("plans.no_models_selected");
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

  // Map model name → vendor slug for the logo endpoint
  // (/dashboard/assets/vendor/:name). Add patterns here when new vendors ship.
  function vendorOf(model) {
    const s = (model || "").toLowerCase();
    if (/^glm|^chatglm|^thinking|^cogvlm|^cogview/.test(s)) return "glm";
    if (/^minimax|^abab|^emotion|^speech-0/.test(s))        return "minimax";
    if (/^qwen|^qwq/.test(s))                                return "qwen";
    return "default";
  }

  // Two-line deployment cell: [logo] + bold model name + faded deployment_id.
  // Replaces the old "model:deployment_id" flat concatenation.
  function renderDeployCell(model, deploymentId) {
    const v = vendorOf(model);
    return '' +
      '<div class="deploy-cell">' +
        '<img class="vendor-logo" src="/dashboard/assets/vendor/' + v + '" alt="' + v + '">' +
        '<div class="deploy-text">' +
          '<div class="deploy-model">' + esc(model) + '</div>' +
          (deploymentId ? '<div class="deploy-id">' + esc(deploymentId) + '</div>' : '') +
        '</div>' +
      '</div>';
  }

  // ISO 8601 (e.g. "2026-07-13T17:50:00Z") → "2026-07-13 17:50:00".
  // Falls back to the raw string when Date can't parse it.
  function formatRangeISO(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    if (isNaN(d.getTime())) return String(iso);
    const p = (n, w) => String(n).padStart(w || 2, "0");
    return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate())
         + " " + p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
  }

  // Place the formatted time range next to the section title (`<h2>`).
  // target = "rate" | "agent" — matches `data-target` on `.range-controls`.
  // Removes the span when from/to are falsy so stale ranges don't linger.
  function setRangeWindowNote(target, from, to) {
    const controls = document.querySelector(`.range-controls[data-target="${target}"]`);
    const header = controls && controls.parentElement;
    if (!header) return;
    let span = header.querySelector(".range-window-note");
    if (!from || !to) {
      if (span) span.remove();
      return;
    }
    if (!span) {
      span = document.createElement("span");
      span.className = "range-window-note";
      header.appendChild(span);
    }
    span.textContent = formatRangeISO(from) + " → " + formatRangeISO(to);
  }

  function formatDuration(secs) {
    if (secs < 60) return secs + "s";
    if (secs < 3600) return (secs / 60) + "min";
    if (secs < 86400) return (secs / 3600) + "h";
    return (secs / 86400) + "d";
  }

  // Normalize a window_limits entry into {counts, tokens, costs, window_secs}.
  // Accepts the verbose object form ({counts, tokens, costs, window_secs}),
  // the 4-element compact array ([counts, tokens, costs, window_secs]), and
  // the legacy 2-element array ([count, window_secs]) for backward compat.
  // Returns null if the entry is not a usable shape.
  function normalizeWindowLimit(w) {
    if (w == null) return null;
    if (Array.isArray(w)) {
      if (w.length === 4) {
        return { counts: w[0], tokens: w[1], costs: w[2], window_secs: w[3] };
      }
      if (w.length === 2) {
        return { counts: w[0], tokens: null, costs: null, window_secs: w[1] };
      }
      return null;
    }
    if (typeof w === "object") {
      return {
        counts: w.counts ?? null,
        tokens: w.tokens ?? null,
        costs: w.costs == null ? null : w.costs,
        window_secs: w.window_secs,
      };
    }
    return null;
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

  // Format a prefix hit-rate percentage. Truncate to 1 decimal (no rounding)
  // and cap at 99.9 so we never display "100%" — cached_tokens can equal
  // input_tokens in healthy cases, but rounding 99.95 upward would mislead.
  function fmtPrefixHit(num) {
    if (num == null) return "-";
    var v = Math.floor(num * 10) / 10;
    if (v > 99.9) v = 99.9;
    return v.toFixed(1) + "%";
  }

  // Stable pastel color for a deployment_id. Same id → same color across all
  // charts. Low saturation / high lightness keep it easy on the eyes; the
  // hue is derived from a 32-bit FNV-1a hash of the id so similar ids do not
  // collapse to the same hue.
  var __deploymentColorCache = {};
  function deploymentColor(deploymentId) {
    if (__deploymentColorCache[deploymentId]) return __deploymentColorCache[deploymentId];
    var h = 0x811c9dc5;
    for (var i = 0; i < deploymentId.length; i++) {
      h ^= deploymentId.charCodeAt(i);
      h = (h + ((h << 1) + (h << 4) + (h << 7) + (h << 8) + (h << 24))) >>> 0;
    }
    var hue = h % 360;
    // Pastel: 38-45% saturation, 64-70% lightness.
    var sat = 38 + (h % 8);       // 38..45
    var light = 64 + ((h >> 4) % 7); // 64..70
    var c = "hsl(" + hue + ", " + sat + "%, " + light + "%)";
    __deploymentColorCache[deploymentId] = c;
    return c;
  }
})();
