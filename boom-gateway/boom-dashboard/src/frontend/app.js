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

  // Return plan names filtered to type=key (for key create/edit dropdown).
  // Mirrors getTeamPlanNames' filter pattern — type=key plans only.
  async function getKeyPlanNames() {
    try {
      const data = await api("/admin/plans");
      return (data.plans || [])
        .filter((p) => !p.type || p.type === "key")
        .map((p) => p.name);
    } catch { return []; }
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

  // ── Viewport-aware tooltip for .cell-tip and .field-tip ──────
  // Positions tooltip above or below the element depending on available space.
  // Both the table-cell hover hints and the form-field "?" buttons route
  // through this single JS-positioned tooltip so that:
  //   1. The tooltip can escape containers with `overflow:auto` (modals,
  //      scrollable panels) — a CSS `::after` pseudo-element cannot.
  //   2. The same viewport-clamping logic applies everywhere, so a "?"
  //      button near the right edge or bottom of the screen no longer
  //      has its hint clipped.
  function setupViewportTooltip() {
    var tip = document.getElementById("vtip");
    if (!tip) return;
    document.addEventListener("mouseover", function(e) {
      var el = e.target.closest(".cell-tip, .field-tip");
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
      var el = e.target.closest(".cell-tip, .field-tip");
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
    // Surface partial-success warnings (e.g. DB write ok but reload failed).
    // The request itself didn't fail, but the user needs to know the
    // operation didn't fully take effect.
    if (data && data.warning) {
      console.warn("Server warning:", data.warning);
      try {
        alert(t("common.warning_prefix", { message: data.warning }));
      } catch {
        alert(t("common.warning_prefix", { message: data.warning }));
      }
    }
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
    else if (section === "admin-quota") loadQuota();
    else if (section === "admin-logs") { setupLogsFilters(); loadLogs(); }
    else if (section === "admin-debug") { loadAgentStats(); loadRebalanceMoves(); loadKvcDfx(); loadAuditLogStats(); }
    else if (section === "admin-config") loadConfigPage();
  }

  function sectionFromHash(hash) {
    if (hash.includes("/admin/stats")) return "admin-stats";
    if (hash.includes("/admin/models")) return "admin-models";
    if (hash.includes("/admin/plans")) return "admin-plans";
    if (hash.includes("/admin/keys")) return "admin-keys";
    if (hash.includes("/admin/quota")) return "admin-quota";
    if (hash.includes("/admin/logs")) return "admin-logs";
    if (hash.includes("/admin/debug")) return "admin-debug";
    if (hash.includes("/admin/config")) return "admin-config";
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

  // ── Audit Log Stats (dropped counter) ───────────────────
  async function loadAuditLogStats() {
    try {
      const data = await api("/admin/stats/audit-log");
      renderAuditLogStats(data || {});
    } catch (err) {
      console.error("loadAuditLogStats error:", err);
    }
  }

  function renderAuditLogStats(data) {
    const wrap = document.getElementById("audit-log-stats-wrap");
    if (!wrap) return;

    if (!data.db_configured) {
      wrap.innerHTML = '<p class="muted">' + t("debug.audit_log.no_db") + '</p>';
      return;
    }

    const dropped = Number(data.dropped || 0);
    const tone = dropped === 0 ? "ok" : (dropped < 1000 ? "warn" : "bad");
    wrap.innerHTML =
      '<div class="audit-log-stats">' +
        '<div class="als-row als-' + tone + '">' +
          '<span class="als-label">' + t("debug.audit_log.dropped") + '</span>' +
          '<span class="als-value">' + dropped.toLocaleString() + '</span>' +
        '</div>' +
        '<p class="als-hint">' + t("debug.audit_log.hint") + '</p>' +
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
          const valTxt = "¥" + (d.current || "0") + " / " + (limit > 0 ? "¥" + (d.limit || "0") : "∞");
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
        var pct = (l.cached_tokens != null && l.input_tokens > 0)
          ? fmtPrefixHit(l.cached_tokens / l.input_tokens * 100)
          : "-";
        var kvCell = '<span class="mono">' + esc(pct) + "</span>";
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
    // Each panel is fetched independently so a single failing endpoint
    // (e.g. /user/usage throwing on a stale limiter state) doesn't strand
    // the other two panels in their initial "Loading..." state.
    try {
      const plan = await api("/user/plan");
      renderPlan(plan);
    } catch (err) {
      console.error("Failed to load user plan:", err);
      const el = document.getElementById("plan-info");
      if (el) el.innerHTML = '<p class="error">' + esc(String(err)) + '</p>';
    }
    try {
      const usage = await api("/user/usage");
      renderUsage(usage);
    } catch (err) {
      console.error("Failed to load user usage:", err);
      const el = document.getElementById("usage-info");
      if (el) el.innerHTML = '<p class="error">' + esc(String(err)) + '</p>';
    }
    try {
      const keyInfo = await api("/user/key-info");
      renderTokenInfo(keyInfo);
      renderKeyInfo(keyInfo);
    } catch (err) {
      console.error("Failed to load key info:", err);
      const el = document.getElementById("key-info");
      if (el) el.innerHTML = '<p class="error">' + esc(String(err)) + '</p>';
      const tEl = document.getElementById("token-info");
      if (tEl) tEl.innerHTML = '<p class="error">' + esc(String(err)) + '</p>';
    }
  }

  function renderPlan(plan) {
    const el = document.getElementById("plan-info");
    if (!plan.plan_name) {
      // Backend distinguishes "no assignment row" (None → falls back to
      // default_plan at runtime, surfaced above as plan_name=...default's
      // name) from "explicit no-plan" (Some(None) → opts out of plan-based
      // limits, no default fallback). Show a distinct label for the latter
      // so users know they actively chose no plan, not "default applied".
      const label = plan.is_explicit_no_plan
        ? t("keys.plan.no_plan")
        : t("plans.using_default");
      el.innerHTML = "<p>" + label + "</p>";
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
            display = "¥" + (d.current || "0") + " / " + (limit > 0 ? "¥" + (d.limit || "0") : t("common.unlimited"));
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
            <div class="dim-value">¥${esc(c.regular_input_cost || "0")}</div>
          </div>`;
      }
      if ((c.cached_input_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.cached_input_cost"))}</span>
            <div class="dim-value">¥${esc(c.cached_input_cost || "0")}</div>
          </div>`;
      }
      if ((c.output_cost_micros || 0) > 0) {
        dimHtml += `
          <div class="dim-row">
            <span class="dim-label">${esc(t("plan.dim.output_cost"))}</span>
            <div class="dim-value">¥${esc(c.output_cost || "0")}</div>
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
      [t("keyinfo.spend"), fmtCost(info.spend)],
      [t("keyinfo.max_budget"), info.max_budget != null ? fmtCost(info.max_budget) : t("common.unlimited")],
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
    const fmtOptCost = (v) => (v == null ? "-" : "¥" + String(v));
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
  let keysPlanFilter = "";  // "" = all, "unassigned"/"none" = follows default, "no_plan" = explicit no-plan, otherwise plan name
  let keysDataCache = [];

  function setupKeysSearch() {
    const el = document.getElementById("keys-search");
    if (el) {
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
    // Plan filter dropdown: populated lazily on first focus / section show,
    // since plans may be added or removed via the Plans page.
    const planSel = document.getElementById("keys-plan-filter");
    if (planSel) {
      refreshKeysPlanFilterOptions(planSel);
      planSel.addEventListener("change", () => {
        keysPlanFilter = planSel.value;
        keysPage = 1;
        loadKeys();
      });
    }
  }

  // Rebuild the dropdown's <option> list. Preserves the current selection when
  // possible — falls back to "all" if the selected plan no longer exists.
  function refreshKeysPlanFilterOptions(sel) {
    getPlanNames().then((names) => {
      const opts = [`<option value="">${t("keys.plan_filter.all")}</option>`]
        .concat([`<option value="unassigned" ${keysPlanFilter === "unassigned" ? "selected" : ""}>${t("keys.plan_filter.unassigned")}</option>`])
        .concat([`<option value="no_plan" ${keysPlanFilter === "no_plan" ? "selected" : ""}>${t("keys.plan_filter.no_plan")}</option>`])
        .concat(names.map((n) => `<option value="${esc(n)}" ${keysPlanFilter === n ? "selected" : ""}>${esc(n)}</option>`));
      sel.innerHTML = opts.join("");
      // Drop stale selection (plan deleted on Plans page). Legacy "none" maps
      // to "unassigned" now — silently migrate.
      if (keysPlanFilter === "none") keysPlanFilter = "unassigned";
      const stillExists = keysPlanFilter === "" || keysPlanFilter === "unassigned" || keysPlanFilter === "no_plan" || names.includes(keysPlanFilter);
      if (!stillExists) {
        keysPlanFilter = "";
        sel.value = "";
      } else {
        sel.value = keysPlanFilter;
      }
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
      if (keysPlanFilter) url += `&plan=${encodeURIComponent(keysPlanFilter)}`;
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

  // Render the plan cell for a key row. Three kinds:
  //   - "default" → no DB row; runtime follows default_plan. Show effective
  //                 plan name with a muted "(默认)" suffix badge.
  //   - "no_plan" → row with plan_name IS NULL; runtime does NOT fall back.
  //                 Show "无套餐" with a "兜底" badge.
  //   - "plan"    → explicit plan name. Show the name plain.
  function renderKeyPlanCell(k) {
    const kind = k.plan_assignment_kind || (k.plan_name ? "plan" : "default");
    if (kind === "no_plan") {
      return `<span class="badge badge-plan-no-plan">${t("keys.plan.no_plan")}</span> <span class="muted" style="font-size:11px">${t("keys.plan.fallback_hint")}</span>`;
    }
    if (kind === "default") {
      if (!k.plan_name) {
        // No default_plan configured either — effectively unbounded.
        return `<span class="muted">-</span>`;
      }
      return `${esc(k.plan_name)} <span class="badge badge-plan-default">${t("keys.plan.default_suffix")}</span>`;
    }
    return esc(k.plan_name || "-");
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
    wrap.innerHTML = `<table>
      <tr><th>${t("keys.col.token")}</th><th>${t("keys.col.alias")}</th><th>${t("keys.col.user")}</th><th>${t("keys.col.plan")}</th><th>${t("keys.col.usage")}</th><th>${t("keys.col.spend")}</th><th>${t("keys.col.budget")}</th><th>${t("keys.col.status")}</th><th>${t("keys.col.actions")}</th></tr>
      ${keys.map((k) => `<tr>
        <td class="mono">${esc(k.token_prefix)}</td>
        <td>${esc(k.key_alias || "-")}${k.key_prefix ? ' <span class="badge badge-prefix">' + esc(k.key_prefix) + "</span>" : ""}${k.tag ? ' <span class="badge badge-tag">' + esc(k.tag) + "</span>" : ""}</td>
        <td>${esc(k.user_id || "-")}</td>
        <td>${renderKeyPlanCell(k)}</td>
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
          <button class="btn-danger btn-small" onclick="window._deleteKey('${esc(k.token_hash)}','${esc(k.key_alias || k.token_prefix || "")}')">${t("action.delete")}</button>
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
  window._editKey = async (tokenHash) => {
    const key = keysDataCache.find((k) => k.token_hash === tokenHash);
    if (!key) return;
    try {
      await showEditKeyModal(key);
    } catch (err) {
      console.error("showEditKeyModal failed:", err);
      alert(t("common.error_prefix", { message: err?.message || String(err) }));
    }
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
  window._deleteKey = async (hash, alias) => {
    if (!confirm(t("confirm.delete_key", { name: alias || hash.slice(0, 12) }))) return;
    try {
      await api(`/admin/keys/${encodeURIComponent(hash)}`, { method: "DELETE" });
      loadKeys();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
    wrap.innerHTML = `<table class="models-table">
      <tr><th>${t("models.col.model")}</th><th>${t("models.col.aliases")}</th><th>${t("models.col.litellm_model")}</th><th>${t("models.col.cost")}</th><th>${t("models.col.base_url")}</th><th>${t("models.col.ratio")}</th><th>${t("models.col.rpm")}</th><th>${t("models.col.timeout")}</th><th>${t("models.col.enabled")}</th><th>${t("models.col.source")}</th><th>${t("models.col.actions")}</th></tr>
      ${models.map((m) => {
        const isAutoDisabled = !m.enabled && m.auto_disabled;
        const rowClass = !m.enabled ? "model-row-disabled" : "";
        const enabledBadge = m.enabled
          ? '<span class="badge badge-active">' + t("common.yes") + '</span>'
          : isAutoDisabled
            ? '<span class="badge badge-blocked">' + t("common.no") + '</span><br><span style="color:var(--danger);font-size:0.8em">' + t("status.auto_disabled") + '</span>'
            : '<span class="badge badge-blocked">' + t("common.no") + '</span>';
        const warningRow = isAutoDisabled
          ? `<tr class="model-row-warning"><td colspan="11">${t("models.fault_disabled")}</td></tr>`
          : '';
        // Cost cell: inline "label:$value" per line, three rows. Compact so
        // the column stays narrow even when EN headers squeeze the table.
        const c = m.cost_per_million || {};
        const fmtCost = (v) => (v == null || v === "0" || v === "") ? "-" : "¥" + v;
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
        return `<tr class="${rowClass}">
        <td>${renderDeployCell(m.model_name, m.deployment_id)}</td>
        <td>${aliasCell}</td>
        <td class="mono">${esc(m.litellm_model)}</td>
        <td>${costCell}</td>
        <td class="mono cell-url" title="${esc(m.api_base || "")}">${esc(m.api_base || "-")}</td>
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

  async function showNewModelModal(prefill) {
    // Cost templates drive the pricing dropdown — load them lazily so the
    // modal works even on a fresh page where Config hasn't been visited.
    if (!_configCache) {
      try { _configCache = await api("/admin/config"); }
      catch (_e) { /* dropdown just shows empty — non-fatal */ }
    }
    const p = prefill || {};
    const headers = p.headers || {};
    const modelInfo = p.model_info || {};
    const __html = `
      <h3>${p.id ? t("form.model.title_edit") : t("form.model.title_create")}</h3>
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("model_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.model.name")} * ${tip(t("tip.model.name"))}</label><input id="m-model-name" value="${esc(p.model_name || "")}" required></div>
            <div class="form-group"><label>${t("form.model.provider")} * ${tip(t("tip.model.provider"))}</label><select id="m-model-provider"><option value="">${t("common.select_placeholder")}</option><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="azure">Azure OpenAI</option><option value="gemini">Google Gemini</option><option value="bedrock">AWS Bedrock</option></select></div>
            <div class="form-group"><label>${t("form.model.id")} * ${tip(t("tip.model.id"))}</label><input id="m-model-id" value="${esc((p.litellm_model || "").includes("/") ? p.litellm_model.split("/").slice(1).join("/") : p.litellm_model || "")}" required></div>
            <div class="form-group"><label>${t("form.model.deployment_id")} ${tip(t("tip.model.deployment_id"))}</label><input id="m-model-deployment-id" value="${esc(p.deployment_id || "")}" placeholder="(auto UUID)"></div>
            <div class="form-group field-checkbox"><input id="m-model-enabled" type="checkbox" ${p.enabled !== false ? "checked" : ""}><label for="m-model-enabled">${t("form.model.enabled")} ${tip(t("tip.model.enabled"))}</label></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.auth")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.model.api_key")} ${tip(t("tip.model.api_key"))}</label><input id="m-model-key" type="password" value="${esc(p.api_key || "")}" placeholder="sk-... or os.environ/VAR"></div>
            <div class="form-group field-checkbox"><input id="m-model-key-env" type="checkbox" ${(p.api_key_env) ? "checked" : ""}><label for="m-model-key-env">${t("form.model.api_key_env")} ${tip(t("tip.model.api_key_env"))}</label></div>
            <div class="form-group field-full"><label>${t("form.model.headers")} ${tip(t("tip.model.headers"))}</label><textarea id="m-model-headers" rows="2" style="font-family:var(--mono);font-size:12px">${esc(Object.keys(headers).length ? JSON.stringify(headers, null, 2) : "")}</textarea></div>
          </div>
        </div>
        <div class="form-card" id="m-model-aws-card" style="display:none">
          <div class="form-card-title">${t("model_card.aws")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.model.aws_region")} ${tip(t("tip.model.aws_region"))}</label><input id="m-model-aws-region" value="${esc(p.aws_region_name || "")}"></div>
            <div class="form-group"><label>${t("form.model.aws_key_id")} ${tip(t("tip.model.aws_key_id"))}</label><input id="m-model-aws-key" value="${esc(p.aws_access_key_id || "")}"></div>
            <div class="form-group field-full"><label>${t("form.model.aws_secret")} ${tip(t("tip.model.aws_secret"))}</label><input id="m-model-aws-secret" type="password" value="${esc(p.aws_secret_access_key || "")}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.rate_limit")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.model.rpm")} ${tip(t("tip.model.rpm"))}</label><input id="m-model-rpm" type="number" value="${p.rpm || ""}"></div>
            <div class="form-group"><label>${t("form.model.tpm")} ${tip(t("tip.model.tpm"))}</label><input id="m-model-tpm" type="number" value="${p.tpm || ""}"></div>
            <div class="form-group"><label>${t("form.model.ratio")} ${tip(t("tip.model.ratio"))}</label><input id="m-model-ratio" type="number" min="1" step="1" value="${p.quota_count_ratio || 1}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.flow_control")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.model.maxinflight")} ${tip(t("tip.model.maxinflight"))}</label><input id="m-model-maxinflight" type="number" min="0" value="${p.max_inflight_queue_len || ""}"></div>
            <div class="form-group"><label>${t("form.model.maxctx")} ${tip(t("tip.model.maxctx"))}</label><input id="m-model-maxctx" type="number" min="0" value="${p.max_context_len || ""}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.tuning")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.model.base")} ${tip(t("tip.model.base"))}</label><input id="m-model-base" value="${esc(p.api_base || "")}" placeholder="https://api.openai.com/v1"></div>
            <div class="form-group"><label>${t("form.model.version")} ${tip(t("tip.model.version"))}</label><input id="m-model-version" value="${esc(p.api_version || "")}"></div>
            <div class="form-group"><label>${t("form.model.timeout")} ${tip(t("tip.model.timeout"))}</label><input id="m-model-timeout" type="number" value="${p.timeout || 1200}"></div>
            <div class="form-group"><label>${t("form.model.temp")} ${tip(t("tip.model.temp"))}</label><input id="m-model-temp" type="number" step="0.1" value="${p.temperature || ""}"></div>
            <div class="form-group"><label>${t("form.model.maxtok")} ${tip(t("tip.model.maxtok"))}</label><input id="m-model-maxtok" type="number" value="${p.max_tokens || ""}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.behavior")}</div>
          <div class="form-card-grid">
            <div class="form-group field-checkbox"><input id="m-model-serve-not-match" type="checkbox" ${p.serve_not_match ? "checked" : ""}><label for="m-model-serve-not-match">${t("form.model.serve_not_match")} ${tip(t("tip.model.serve_not_match"))}</label></div>
            <div class="form-group field-checkbox"><input id="m-model-client-type" type="checkbox" ${p.client_type_header ? "checked" : ""}><label for="m-model-client-type">${t("form.model.client_type_header")} ${tip(t("tip.model.client_type_header"))}</label></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("model_card.cost")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.model.cost_template")} ${tip(t("tip.model.cost_template"))}</label>
              <select id="m-model-cost-template">
                <option value="">${t("common.none_option")}</option>
                ${((_configCache && _configCache.cost_templates) || []).map((tpl) => `<option value="${esc(tpl.name)}" ${modelInfo.cost_template === tpl.name ? "selected" : ""}>${esc(tpl.name)}</option>`).join("")}
              </select>
            </div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-model-submit">${p.id ? t("action.update") : t("action.create")}</button>
      </div>
    `;
    showModal(__html, { xwide: true });
    // Pre-select provider dropdown from litellm_model
    const providerSel = document.getElementById("m-model-provider");
    if (p.litellm_model && p.litellm_model.includes("/")) {
      const prov = p.litellm_model.split("/")[0];
      if (providerSel.querySelector(`option[value="${prov}"]`)) providerSel.value = prov;
    }
    // Show/hide the AWS Bedrock card based on provider. Only bedrock uses IAM
    // credentials; for everything else the card is hidden so users don't get
    // the misleading impression that they need to fill it.
    const awsCard = document.getElementById("m-model-aws-card");
    const syncAwsCard = () => {
      awsCard.style.display = providerSel.value === "bedrock" ? "" : "none";
    };
    syncAwsCard();
    providerSel.addEventListener("change", syncAwsCard);
    document.getElementById("m-model-submit").addEventListener("click", async () => {
      try {
        const providerVal = document.getElementById("m-model-provider").value;
        const modelIdVal = document.getElementById("m-model-id").value.trim();
        const litellmModel = providerVal ? providerVal + "/" + modelIdVal : modelIdVal;
        const headersText = document.getElementById("m-model-headers").value.trim();
        let headers = {};
        if (headersText) {
          try { headers = JSON.parse(headersText); }
          catch (e) { throw new Error(t("config.error.invalid_json", { msg: e.message })); }
        }
        // Build model_info only when cost_template is selected.
        const costTemplate = document.getElementById("m-model-cost-template").value;
        const modelInfo = {};
        if (costTemplate) modelInfo.cost_template = costTemplate;
        const body = {
          model_name: document.getElementById("m-model-name").value,
          litellm_model: litellmModel,
          deployment_id: document.getElementById("m-model-deployment-id").value.trim() || null,
          api_key: document.getElementById("m-model-key").value || null,
          api_key_env: document.getElementById("m-model-key-env").checked,
          api_base: document.getElementById("m-model-base").value || null,
          api_version: document.getElementById("m-model-version").value || null,
          aws_region_name: document.getElementById("m-model-aws-region").value || null,
          aws_access_key_id: document.getElementById("m-model-aws-key").value || null,
          aws_secret_access_key: document.getElementById("m-model-aws-secret").value || null,
          rpm: document.getElementById("m-model-rpm").value ? Number(document.getElementById("m-model-rpm").value) : null,
          tpm: document.getElementById("m-model-tpm").value ? Number(document.getElementById("m-model-tpm").value) : null,
          quota_count_ratio: Number(document.getElementById("m-model-ratio").value) || 1,
          timeout: Number(document.getElementById("m-model-timeout").value) || 1200,
          temperature: document.getElementById("m-model-temp").value ? Number(document.getElementById("m-model-temp").value) : null,
          max_tokens: document.getElementById("m-model-maxtok").value ? Number(document.getElementById("m-model-maxtok").value) : null,
          max_inflight_queue_len: document.getElementById("m-model-maxinflight").value ? Number(document.getElementById("m-model-maxinflight").value) : null,
          max_context_len: document.getElementById("m-model-maxctx").value ? Number(document.getElementById("m-model-maxctx").value) : null,
          enabled: document.getElementById("m-model-enabled").checked,
          serve_not_match: document.getElementById("m-model-serve-not-match").checked,
          client_type_header: document.getElementById("m-model-client-type").checked,
          headers,
        };
        if (Object.keys(modelInfo).length > 0) body.model_info = modelInfo;
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
      // Backend masks api_key / aws_* as "****". Clear them so the edit
      // form's inputs start empty — submitting empty converts to null,
      // and update_db's COALESCE keeps the stored value untouched.
      ["api_key", "aws_access_key_id", "aws_secret_access_key"].forEach((k) => {
        if (m[k] === "****") m[k] = "";
      });
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
    const __html = `
      <h3>${p.alias_name ? t("form.alias.title_edit") : t("form.alias.title_create")}</h3>
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("alias_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.alias.name")} * ${tip(t("tip.alias.name"))}</label><input id="m-alias-name" value="${esc(p.alias_name || "")}" ${p.alias_name ? "readonly" : ""}></div>
            <div class="form-group field-full"><label>${t("form.alias.target")} * ${tip(t("tip.alias.target"))}</label><input id="m-alias-target" value="${esc(p.target_model || "")}" required list="alias-target-list"><datalist id="alias-target-list"></datalist></div>
            <div class="form-group field-full"><label>${t("form.alias.hidden")} ${tip(t("tip.alias.hidden"))}</label><select id="m-alias-hidden"><option value="false" ${!p.hidden ? "selected" : ""}>${t("common.no")}</option><option value="true" ${p.hidden ? "selected" : ""}>${t("common.yes")}</option></select></div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-alias-submit">${p.alias_name ? t("action.update") : t("action.create")}</button>
      </div>
    `;
    showModal(__html, { xwide: true });
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

  // ── Admin: Config Page ────────────────────────────────

  let _configCache = null;

  async function loadConfigPage() {
    const wrap = document.getElementById("config-page-wrap");
    if (!wrap) return;
    wrap.innerHTML = '<p class="loading">' + t("common.loading") + "</p>";
    try {
      const cfg = await api("/admin/config");
      _configCache = cfg;
      wrap.innerHTML = renderConfigPage(cfg);
      wireConfigPage(cfg);
    } catch (err) {
      wrap.innerHTML = '<p style="color:var(--danger)">' + esc(err.message) + "</p>";
    }
  }

  function renderConfigPage(cfg) {
    return `
      <div class="config-page">
        <div class="config-group">
          <div class="config-group-title">${t("config.group.runtime")}</div>
          <div class="config-grid">
            ${renderCardServer(cfg.server || {})}
            ${renderCardGeneral(cfg.general_settings || {})}
            ${renderCardHealthCheck(cfg.deployment_health_check || {})}
            ${renderCardPromptLog(cfg.prompt_log || {})}
          </div>
        </div>
        <div class="config-group">
          <div class="config-group-title">${t("config.group.traffic")}</div>
          <div class="config-grid">
            ${renderCardRateLimit(cfg.rate_limit || {})}
            ${renderCardPlanSettings(cfg.plan_settings || {}, cfg)}
          </div>
        </div>
        <div class="config-group">
          <div class="config-group-title">${t("config.group.routing")}</div>
          <div class="config-grid">
            ${renderCardRouter(cfg.router_settings || {})}
            ${renderCardCostTemplates(cfg.cost_templates || [])}
            ${renderCardModelList(cfg.model_list || [])}
          </div>
        </div>
      </div>
    `;
  }

  function fieldText(id, label, value, opts = {}) {
    const v = value === null || value === undefined ? "" : String(value);
    const placeholder = opts.placeholder ? ` placeholder="${esc(opts.placeholder)}"` : "";
    const type = opts.type || "text";
    return `<div class="form-group${opts.full ? " field-full" : ""}"><label for="${id}">${esc(label)}</label><input id="${id}" type="${type}" value="${esc(v)}"${placeholder}></div>`;
  }
  function fieldNum(id, label, value, opts = {}) {
    const v = value === null || value === undefined ? "" : String(value);
    return `<div class="form-group${opts.full ? " field-full" : ""}"><label for="${id}">${esc(label)}</label><input id="${id}" type="number" value="${esc(v)}" ${opts.min !== undefined ? `min="${opts.min}"` : ""} ${opts.step ? `step="${opts.step}"` : ""}></div>`;
  }
  function fieldCheckbox(id, label, checked) {
    return `<div class="form-group field-checkbox"><input id="${id}" type="checkbox" ${checked ? "checked" : ""}><label for="${id}">${esc(label)}</label></div>`;
  }
  function fieldSelect(id, label, options, selected) {
    const opts = options.map((o) => {
      const val = typeof o === "string" ? o : o.value;
      const txt = typeof o === "string" ? o : o.label;
      return `<option value="${esc(val)}" ${val === selected ? "selected" : ""}>${esc(txt)}</option>`;
    }).join("");
    return `<div class="form-group"><label for="${id}">${esc(label)}</label><select id="${id}">${opts}</select></div>`;
  }
  function fieldTextarea(id, label, value, opts = {}) {
    const v = value === null || value === undefined ? "" : typeof value === "string" ? value : JSON.stringify(value, null, 2);
    return `<div class="form-group field-full"><label for="${id}">${esc(label)}</label><textarea id="${id}" rows="${opts.rows || 3}" style="font-family:var(--mono);font-size:12px">${esc(v)}</textarea></div>`;
  }

  function renderCardServer(s) {
    return `<div class="form-card" data-section="server">
      <div class="form-card-title">${t("config.section.server")}</div>
      <div class="form-card-grid">
        ${fieldText("cfg-server-host", t("config.field.host"), s.host)}
        ${fieldNum("cfg-server-port", t("config.field.port"), s.port, { min: 1 })}
        ${fieldNum("cfg-server-workers", t("config.field.workers"), s.workers, { min: 1 })}
      </div>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="server">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function renderCardGeneral(g) {
    const masked = (v) => v ? "****" : "(" + t("common.none_option") + ")";
    return `<div class="form-card" data-section="general">
      <div class="form-card-title">${t("config.section.general_settings")}</div>
      <div class="form-card-grid">
        <div class="form-group"><label>${t("config.field.master_key")}</label><input value="${esc(masked(g.master_key))}" readonly style="opacity:.6"></div>
        <div class="form-group"><label>${t("config.field.database_url")}</label><input value="${esc(masked(g.database_url))}" readonly style="opacity:.6"></div>
        ${fieldFullList("cfg-general-public-models", t("config.field.public_models"), g.public_models || [])}
      </div>
      <p class="modal-hint">${t("config.tip.master_key_readonly")}</p>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="general">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function fieldFullList(id, label, list) {
    const v = Array.isArray(list) ? list.join(", ") : "";
    return `<div class="form-group field-full"><label for="${id}">${esc(label)}</label><input id="${id}" value="${esc(v)}" placeholder="comma-separated"></div>`;
  }

  function renderCardRateLimit(r) {
    const enabled = r.enabled !== false;
    return `<div class="form-card" data-section="rate_limit">
      <div class="form-card-title">${t("config.section.rate_limit")}</div>
      <div class="form-card-grid">
        <div class="form-group field-checkbox"><input id="cfg-rl-enabled" type="checkbox" ${enabled ? "checked" : ""}><label for="cfg-rl-enabled">${t("config.field.rate_limit_enabled")}</label></div>
        ${fieldNum("cfg-rl-default-rpm", t("config.field.default_rpm"), r.default_rpm, { min: 0 })}
        ${fieldNum("cfg-rl-default-tpm", t("config.field.default_tpm"), r.default_tpm, { min: 0 })}
        ${fieldTextarea("cfg-rl-windows", t("config.field.window_limits"), r.window_limits || [], { rows: 2 })}
      </div>
      <p class="modal-hint">${t("config.tip.rate_limit_scope")}</p>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="rate_limit">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function renderCardPlanSettings(p, cfg) {
    const planRows = (cfg && cfg.plan_settings && cfg.plan_settings.plans) ? Object.keys(cfg.plan_settings.plans) : [];
    const planOptions = [{ value: "", label: "(" + t("common.none_option") + ")" }].concat(planRows.map((n) => ({ value: n, label: n })));
    return `<div class="form-card" data-section="plan_settings">
      <div class="form-card-title">${t("config.section.plan_settings")}</div>
      <div class="form-card-grid">
        ${fieldSelect("cfg-ps-default-plan", t("config.field.default_plan"), planOptions, p.default_plan || "")}
        ${fieldSelect("cfg-ps-default-team-plan", t("config.field.default_team_plan"), planOptions, p.default_team_plan || "")}
      </div>
      <p class="modal-hint">${t("config.tip.manage_plans_on_plans_page")}</p>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="plan_defaults">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function renderCardHealthCheck(h) {
    return `<div class="form-card" data-section="deployment_health_check">
      <div class="form-card-title">${t("config.section.deployment_health_check")}</div>
      <div class="form-card-grid">
        ${fieldCheckbox("cfg-hc-auto-off", t("config.field.auto_offline_enabled"), h.auto_offline_enabled)}
        ${fieldCheckbox("cfg-hc-auto-rec", t("config.field.auto_recovery_enabled"), h.auto_recovery_enabled)}
        ${fieldText("cfg-hc-path", t("config.field.path"), h.path)}
        ${fieldNum("cfg-hc-failure-thr", t("config.field.failure_threshold"), h.failure_threshold, { min: 1 })}
        ${fieldNum("cfg-hc-recovery-thr", t("config.field.recovery_threshold"), h.recovery_threshold, { min: 1 })}
        ${fieldNum("cfg-hc-offline-int", t("config.field.offline_check_interval_secs"), h.offline_check_interval_secs, { min: 1 })}
        ${fieldNum("cfg-hc-recovery-int", t("config.field.recovery_check_interval_secs"), h.recovery_check_interval_secs, { min: 1 })}
        ${fieldCheckbox("cfg-hc-req-fail-auto", t("config.field.request_failure_auto_offline_enabled"), h.request_failure_auto_offline_enabled)}
        ${fieldNum("cfg-hc-req-fail-thr", t("config.field.request_failure_threshold"), h.request_failure_threshold, { min: 1 })}
      </div>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="health_check">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function renderCardPromptLog(p) {
    return `<div class="form-card" data-section="prompt_log">
      <div class="form-card-title">${t("config.section.prompt_log")}</div>
      <div class="form-card-grid">
        ${fieldCheckbox("cfg-pl-enabled", t("config.field.enabled"), p.enabled)}
        ${fieldText("cfg-pl-dir", t("config.field.dir"), p.dir, { full: true })}
        ${fieldNum("cfg-pl-max-mb", t("config.field.max_file_size_mb"), p.max_file_size_mb, { min: 1 })}
        ${fieldCheckbox("cfg-pl-capture", t("config.field.capture_raw_upstream"), p.capture_raw_upstream)}
        ${fieldFullList("cfg-pl-excluded-keys", t("config.field.excluded_keys"), p.excluded_keys || [])}
        ${fieldFullList("cfg-pl-excluded-teams", t("config.field.excluded_teams"), p.excluded_teams || [])}
        ${fieldFullList("cfg-pl-record-headers", t("config.field.record_headers"), p.record_headers || [])}
      </div>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="prompt_log">${t("action.save")}</button>
      </div>
    </div>`;
  }

  function renderCardRouter(r) {
    return `<div class="form-card" data-section="router_settings">
      <div class="form-card-title">${t("config.section.router_settings")}</div>
      <div class="form-card-grid">
        ${fieldSelect("cfg-rs-policy", t("config.field.schedule_policy"),
          [{ value: "round_robin", label: "L0 round_robin" }, { value: "key_affinity", label: "L1 key_affinity" }],
          r.schedule_policy || "round_robin")}
        ${fieldNum("cfg-rs-affinity-ctx", t("config.field.key_affinity_context_threshold"), r.key_affinity_context_threshold || 0, { min: 0 })}
        ${fieldNum("cfg-rs-affinity-rebal", t("config.field.rebalance_threshold"), r.rebalance_threshold || 20, { min: 1, step: 1 })}
        ${fieldNum("cfg-rs-flow-timeout", t("config.field.flow_control_queue_timeout_secs"), r.flow_control_queue_timeout_secs || 1200, { min: 1, step: 1 })}
        ${fieldCheckbox("cfg-rs-priority-hdr", t("config.field.enable_priority_header"), r.enable_priority_header)}
        ${fieldCheckbox("cfg-rs-strip-cc", t("config.field.strip_claude_code_attribution"), r.strip_claude_code_attribution)}
        ${fieldTextarea("cfg-rs-aliases", t("config.field.model_group_alias"), r.model_group_alias || {}, { rows: 3 })}
      </div>
      <details class="form-card-collapsible is-disabled">
        <summary>
          <span>${t("config.section.kvc_aware")}</span>
          <span class="badge-wip">${t("common.wip")}</span>
        </summary>
        <p class="modal-hint">${t("config.tip.kvc_aware_wip")}</p>
        <div class="form-card-grid" aria-disabled="true">
          ${fieldNum("cfg-kvc-block", t("config.field.block_size"), (r.kvc_aware || {}).block_size, { min: 1 })}
          ${fieldNum("cfg-kvc-cache-w", t("config.field.cache_weight"), (r.kvc_aware || {}).cache_weight, { step: "0.05" })}
          ${fieldNum("cfg-kvc-load-w", t("config.field.load_weight"), (r.kvc_aware || {}).load_weight, { step: "0.05" })}
          ${fieldNum("cfg-kvc-max-blocks", t("config.field.max_blocks"), (r.kvc_aware || {}).max_blocks, { min: 1 })}
          ${fieldNum("cfg-kvc-overload", t("config.field.overload_threshold_pct"), (r.kvc_aware || {}).overload_threshold_pct, { min: 1, max: 100, step: 1 })}
          ${fieldNum("cfg-kvc-ttl", t("config.field.router_ttl_secs"), (r.kvc_aware || {}).router_ttl_secs, { min: 0, step: 1 })}
        </div>
      </details>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" data-save="router">${t("action.save")}</button>
        <button class="btn-secondary btn-small" data-save="kvc_aware" disabled title="${esc(t("config.tip.kvc_aware_wip"))}">${t("config.action.save_kvc")}</button>
      </div>
    </div>`;
  }

  function renderCardCostTemplates(templates) {
    const rows = (templates || []).map((tpl, i) => `
      <tr>
        <td><strong>${esc(tpl.name)}</strong></td>
        <td>${esc(tpl.input_cost_per_million_tokens ?? "-")}</td>
        <td>${esc(tpl.cached_input_cost_per_million_tokens ?? "-")}</td>
        <td>${esc(tpl.output_cost_per_million_tokens ?? "-")}</td>
        <td>
          <button class="btn-small" onclick="window._editCostTemplate('${esc(tpl.name)}')">${t("action.edit")}</button>
          <button class="btn-small is-danger" onclick="window._deleteCostTemplate('${esc(tpl.name)}')">${t("action.delete")}</button>
        </td>
      </tr>
    `).join("");
    return `<div class="form-card" data-section="cost_templates">
      <div class="form-card-title">${t("config.section.cost_templates")}</div>
      <div class="modal-table-wrap">
        <table class="modal-table">
          <thead><tr><th>${t("config.field.name")}</th><th>${t("config.field.input_cost_per_million_tokens")}</th><th>${t("config.field.cached_input_cost_per_million_tokens")}</th><th>${t("config.field.output_cost_per_million_tokens")}</th><th>${t("common.actions")}</th></tr></thead>
          <tbody>${rows || `<tr><td colspan="5" class="muted">${t("common.no_data")}</td></tr>`}</tbody>
        </table>
      </div>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" onclick="window._addCostTemplate()">${t("config.action.add_template")}</button>
      </div>
    </div>`;
  }

  function renderCardModelList(modelList) {
    const count = (modelList || []).length;
    return `<div class="form-card" data-section="model_list">
      <div class="form-card-title">${t("config.section.model_list")}</div>
      <p>${t("config.tip.model_list_count", { count })}</p>
      <div class="form-card-actions">
        <button class="btn-primary btn-small" onclick="window.location.hash='#/admin/models'">${t("config.action.manage_models")}</button>
      </div>
    </div>`;
  }

  function wireConfigPage(cfg) {
    document.querySelectorAll("[data-save]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const kind = btn.getAttribute("data-save");
        saveConfigSectionKind(kind, cfg);
      });
    });
    const reloadBtn = document.getElementById("btn-reload-config-page");
    if (reloadBtn) reloadBtn.addEventListener("click", reloadConfigHandler);
  }

  async function reloadConfigHandler() {
    try {
      await api("/admin/config/reload", { method: "POST" });
      showToast(t("config.reloaded"));
      loadConfigPage();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  }

  function parseListInput(el) {
    const v = (el.value || "").trim();
    if (!v) return [];
    return v.split(",").map((s) => s.trim()).filter(Boolean);
  }

  function parseJsonInput(el, fallback) {
    const v = (el.value || "").trim();
    if (!v) return fallback;
    try { return JSON.parse(v); } catch (e) { throw new Error(t("config.error.invalid_json", { msg: e.message })); }
  }

  function numOr(el, fallback) {
    const v = el.value;
    if (v === "" || v === null || v === undefined) return fallback;
    const n = Number(v);
    return Number.isFinite(n) ? n : fallback;
  }

  async function saveConfigSectionKind(kind, cfg) {
    try {
      const $ = (id) => document.getElementById(id);
      if (kind === "server") {
        await saveConfigSection("server", {
          host: $("cfg-server-host").value,
          port: numOr($("cfg-server-port"), 4000),
          workers: numOr($("cfg-server-workers"), 4),
        });
      } else if (kind === "general") {
        await saveConfigSection("general_settings.public_models", parseListInput($("cfg-general-public-models")));
      } else if (kind === "rate_limit") {
        const tpmRaw = $("cfg-rl-default-tpm").value;
        await saveConfigSection("rate_limit", {
          enabled: $("cfg-rl-enabled").checked,
          default_rpm: numOr($("cfg-rl-default-rpm"), 60),
          default_tpm: tpmRaw === "" ? null : Number(tpmRaw),
          window_limits: parseJsonInput($("cfg-rl-windows"), []),
        });
      } else if (kind === "plan_defaults") {
        const dp = $("cfg-ps-default-plan").value;
        const dtp = $("cfg-ps-default-team-plan").value;
        await saveConfigSection("plan_settings.default_plan", dp || null);
        await saveConfigSection("plan_settings.default_team_plan", dtp || null);
      } else if (kind === "health_check") {
        await saveConfigSection("deployment_health_check", {
          auto_offline_enabled: $("cfg-hc-auto-off").checked,
          auto_recovery_enabled: $("cfg-hc-auto-rec").checked,
          path: $("cfg-hc-path").value || "/metric",
          failure_threshold: numOr($("cfg-hc-failure-thr"), 3),
          recovery_threshold: numOr($("cfg-hc-recovery-thr"), 2),
          offline_check_interval_secs: numOr($("cfg-hc-offline-int"), 30),
          recovery_check_interval_secs: numOr($("cfg-hc-recovery-int"), 60),
          request_failure_auto_offline_enabled: $("cfg-hc-req-fail-auto").checked,
          request_failure_threshold: numOr($("cfg-hc-req-fail-thr"), 3),
        });
      } else if (kind === "prompt_log") {
        await saveConfigSection("prompt_log", {
          enabled: $("cfg-pl-enabled").checked,
          dir: $("cfg-pl-dir").value || "/data/prompt_logs",
          max_file_size_mb: numOr($("cfg-pl-max-mb"), 50),
          capture_raw_upstream: $("cfg-pl-capture").checked,
          excluded_keys: parseListInput($("cfg-pl-excluded-keys")),
          excluded_teams: parseListInput($("cfg-pl-excluded-teams")),
          record_headers: parseListInput($("cfg-pl-record-headers")),
        });
      } else if (kind === "router") {
        const aliases = parseJsonInput($("cfg-rs-aliases"), {});
        const routerValue = {
          schedule_policy: $("cfg-rs-policy").value,
          key_affinity_context_threshold: numOr($("cfg-rs-affinity-ctx"), 0),
          rebalance_threshold: numOr($("cfg-rs-affinity-rebal"), 20),
          flow_control_queue_timeout_secs: numOr($("cfg-rs-flow-timeout"), 1200),
          enable_priority_header: $("cfg-rs-priority-hdr").checked,
          strip_claude_code_attribution: $("cfg-rs-strip-cc").checked,
          model_group_alias: aliases,
        };
        await saveConfigSection("router_settings", routerValue);
      } else if (kind === "kvc_aware") {
        const kvcValue = {
          block_size: numOr($("cfg-kvc-block"), 16),
          cache_weight: numOr($("cfg-kvc-cache-w"), 0.5),
          load_weight: numOr($("cfg-kvc-load-w"), 0.2),
          max_blocks: numOr($("cfg-kvc-max-blocks"), 500000),
          overload_threshold_pct: numOr($("cfg-kvc-overload"), 90),
          router_ttl_secs: numOr($("cfg-kvc-ttl"), 1200),
        };
        await saveConfigSection("router_settings.kvc_aware", kvcValue);
      }
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  }

  async function saveConfigSection(path, value) {
    await api("/admin/config", { method: "PUT", body: JSON.stringify({ path, value }) });
    showToast(t("config.saved"));
    await loadConfigPage();
  }

  function showCostTemplateModal(existing) {
    const isEdit = !!existing;
    const tpl = existing || { name: "", input_cost_per_million_tokens: null, cached_input_cost_per_million_tokens: null, output_cost_per_million_tokens: null };
    showModal(`
      <h3>${isEdit ? t("config.action.edit_template") : t("config.action.add_template")}</h3>
      <div class="form-group"><label>${t("config.field.name")}</label><input id="ct-name" value="${esc(tpl.name)}" ${isEdit ? "readonly" : ""}></div>
      <div class="form-group"><label>${t("config.field.input_cost_per_million_tokens")}</label><input id="ct-input" type="number" step="0.01" value="${tpl.input_cost_per_million_tokens ?? ""}"></div>
      <div class="form-group"><label>${t("config.field.cached_input_cost_per_million_tokens")}</label><input id="ct-cached" type="number" step="0.01" value="${tpl.cached_input_cost_per_million_tokens ?? ""}"></div>
      <div class="form-group"><label>${t("config.field.output_cost_per_million_tokens")}</label><input id="ct-output" type="number" step="0.01" value="${tpl.output_cost_per_million_tokens ?? ""}"></div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="ct-save">${isEdit ? t("action.save") : t("action.create")}</button>
      </div>
    `);
    document.getElementById("ct-save").addEventListener("click", async () => {
      const nameVal = document.getElementById("ct-name").value.trim();
      if (!nameVal) { alert(t("common.error_prefix", { message: t("config.field.name") })); return; }
      const updated = {
        name: nameVal,
        input_cost_per_million_tokens: Number(document.getElementById("ct-input").value) || null,
        cached_input_cost_per_million_tokens: Number(document.getElementById("ct-cached").value) || null,
        output_cost_per_million_tokens: Number(document.getElementById("ct-output").value) || null,
      };
      const current = ((_configCache && _configCache.cost_templates) || []);
      // Replace by name on edit; append on create. Name is the identity, so
      // editing keeps the template at its original slot.
      const next = isEdit
        ? current.map((t) => (t.name === existing.name ? updated : t))
        : current.concat([updated]);
      try {
        await saveConfigSection("cost_templates", next);
        hideModal();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  window._addCostTemplate = () => {
    showCostTemplateModal(null);
  };

  window._editCostTemplate = (name) => {
    const existing = ((_configCache && _configCache.cost_templates) || []).find((t) => t.name === name);
    if (!existing) return;
    showCostTemplateModal(existing);
  };

  window._deleteCostTemplate = async (name) => {
    if (!confirm(t("confirm.delete", { name }))) return;
    const current = ((_configCache && _configCache.cost_templates) || []).filter((t) => t.name !== name);
    try { await saveConfigSection("cost_templates", current); } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };

  let debugEnabled = false;

  async function loadDebugStatus() {
    const btn = document.getElementById("btn-debug-toggle");
    if (!btn) return;
    try {
      const data = await api("/admin/debug/status");
      debugEnabled = data.enabled;
      updateDebugButton(btn);
    } catch {
      // Debug status endpoint unreachable (network/probe failure) — hide the
      // toggle button only. The standalone Debug nav-link is gated solely by
      // window.__KVC_DEBUG (set at HTML injection time by handlers_static),
      // independent of this endpoint's availability.
      btn.style.display = "none";
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
      `, { xwide: true });
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
    document.getElementById("btn-import-keys").addEventListener("click", showImportKeysModal);
    document.getElementById("btn-import-help").addEventListener("click", showImportHelpModal);
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
      } catch (err) { alert(t("alert.reload_failed", { message: err.message })); }
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

  function showModal(html, opts = {}) {
    const content = document.getElementById("modal-content");
    content.innerHTML = html;
    content.classList.toggle("modal-wide", opts.wide || false);
    content.classList.toggle("modal-xwide", opts.xwide || false);
    document.getElementById("modal-overlay").classList.remove("hidden");
  }

  function hideModal() {
    document.getElementById("modal-overlay").classList.add("hidden");
    const content = document.getElementById("modal-content");
    content.classList.remove("modal-wide");
    content.classList.remove("modal-xwide");
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
    const __html = `
      <h3>${p.name ? t("form.plan.title_edit") : t("form.plan.title_create")}</h3>
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("plan_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.plan.name")} ${tip(t("tip.plan.name"))}</label><input id="m-plan-name" value="${esc(p.name || "")}" ${p.name ? "readonly" : ""} required></div>
            <div class="form-group"><label>${t("form.plan.type")} ${tip(t("tip.plan.type"))}</label><select id="m-plan-type">
              <option value="key" ${(p.type || "key") === "key" ? "selected" : ""}>Key</option>
              <option value="team" ${p.type === "team" ? "selected" : ""}>Team</option>
            </select></div>
            <div class="form-group"><label>${t("form.plan.member_plan")} ${tip(t("tip.plan.member_plan"))}</label><input id="m-plan-member-plan" value="${esc(p.member_plan || "")}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("plan_card.simple_limits")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.plan.concurrency")} ${tip(t("tip.plan.concurrency"))}</label><input id="m-plan-concurrency" type="number" value="${p.concurrency_limit || ""}"></div>
            <div class="form-group"><label>${t("form.plan.rpm")} ${tip(t("tip.plan.rpm"))}</label><input id="m-plan-rpm" type="number" value="${p.rpm_limit || ""}"></div>
            <div class="form-group"><label>${t("form.plan.tpm")} ${tip(t("tip.plan.tpm"))}</label><input id="m-plan-tpm" type="number" value="${p.tpm_limit || ""}"></div>
            <div class="form-group"><label>${t("form.plan.total_token")} ${tip(t("tip.plan.total_token"))}</label><input id="m-plan-total-token" type="number" value="${p.total_token_limit || ""}"></div>
            <div class="form-group"><label>${t("form.plan.total_cost")} ${tip(t("tip.plan.total_cost"))}</label><input id="m-plan-total-cost" type="number" step="0.01" value="${p.total_cost_limit || ""}"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("plan_card.window_limits")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.plan.windows")} ${tip(t("tip.plan.windows"))}</label><div id="m-plan-windows-container" class="wl-editor"></div></div>
          </div>
        </div>
        <div class="form-card form-card-full">
          <div class="form-card-title">${t("plan_card.schedule")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.plan.schedule")} ${tip(t("tip.plan.schedule"))}</label><div id="m-plan-schedule-container" class="schedule-editor"></div><div id="m-plan-schedule-warning" class="schedule-warning"></div></div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-plan-submit">${p.name ? t("action.update") : t("action.create")}</button>
      </div>
    `;
    showModal(__html, { xwide: true });

    // ── Window-limits sub-editor ──────────────────────────
    // A plan has exactly ONE window_limits entry. Pass the first item only.
    // normalizeWindowLimit accepts both array and object forms so legacy DB
    // rows written as `[[100, null, null, 60]]` still display correctly.
    const windowsContainer = document.getElementById("m-plan-windows-container");
    const wlRows = Array.isArray(p.window_limits) && p.window_limits.length
      ? [normalizeWindowLimit(p.window_limits[0])].filter(Boolean)
      : [];
    renderWindowLimitsEditor(windowsContainer, wlRows, "wl");

    // ── Schedule sub-editor ───────────────────────────────
    // Each slot card = one ScheduleSlot {hours, concurrency, rpm, tpm, window_limits}.
    // hours is rendered as two <input type="time">; on submit we join them
    // back into the "HH:MM-HH:MM" string the backend expects.
    const scheduleContainer = document.getElementById("m-plan-schedule-container");
    const scheduleWarning = document.getElementById("m-plan-schedule-warning");
    const slotRows = Array.isArray(p.schedule) ? p.schedule.slice() : [];
    renderScheduleEditor(scheduleContainer, scheduleWarning, slotRows);

    // Re-run schedule overlap validation on every input change so the
    // warning surfaces live (not only at submit time).
    const revalidate = () => {
      const slots = collectSchedule(scheduleContainer);
      renderScheduleWarning(scheduleWarning, scheduleContainer, slots);
    };
    scheduleContainer.addEventListener("input", revalidate);
    revalidate();

    document.getElementById("m-plan-submit").addEventListener("click", async () => {
      try {
        const windows = collectWindowLimits(windowsContainer);
        const schedule = collectSchedule(scheduleContainer);
        const conflicts = validateScheduleOverlap(schedule);
        if (conflicts.length) {
          renderScheduleWarning(scheduleWarning, scheduleContainer, schedule);
          // Surface the first conflict to the user as an alert so they don't
          // have to scroll to spot the red border.
          const c = conflicts[0];
          alert(t("form.plan.schedule_conflict", { i: c.i + 1, j: c.j + 1 }));
          return;
        }
        await api("/admin/plans", {
          method: "PUT",
          body: JSON.stringify({
            name: document.getElementById("m-plan-name").value,
            type: document.getElementById("m-plan-type").value,
            member_plan: document.getElementById("m-plan-member-plan").value || null,
            concurrency_limit: document.getElementById("m-plan-concurrency").value ? Number(document.getElementById("m-plan-concurrency").value) : null,
            rpm_limit: document.getElementById("m-plan-rpm").value ? Number(document.getElementById("m-plan-rpm").value) : null,
            tpm_limit: document.getElementById("m-plan-tpm").value ? Number(document.getElementById("m-plan-tpm").value) : null,
            total_token_limit: document.getElementById("m-plan-total-token").value ? Number(document.getElementById("m-plan-total-token").value) : null,
            total_cost_limit: document.getElementById("m-plan-total-cost").value ? Number(document.getElementById("m-plan-total-cost").value) : null,
            window_limits: windows,
            schedule,
          }),
        });
        hideModal();
        invalidateCaches();
        loadPlans();
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  // ── Window-limits editor helpers ─────────────────────────
  // Renders a single 4-field row: counts / tokens / costs / window_secs.
  // Each plan has exactly ONE window_limits entry; each schedule slot has
  // exactly ONE too. No add/remove buttons — the row count is fixed at 1
  // so the UI matches the backend's effective_limits() logic (which picks the
  // slot's window_limits[0] or falls back to the plan's window_limits[0]).
  // If `rows` is empty, a blank row is rendered so the user can fill it in.
  function renderWindowLimitsEditor(container, rows, prefix) {
    if (!container) return;
    const w = rows && rows.length ? rows[0] : { counts: null, tokens: null, costs: null, window_secs: 60 };
    container.innerHTML = `
      <div class="wl-row" data-wl-row="0">
        <label class="wl-cell">
          <span class="wl-cell-label">${esc(t("form.plan.window_counts"))}</span>
          <input type="number" min="0" step="1" class="wl-input" data-wl-field="counts"
            value="${w.counts == null ? "" : esc(String(w.counts))}" placeholder="—">
        </label>
        <label class="wl-cell">
          <span class="wl-cell-label">${esc(t("form.plan.window_tokens"))}</span>
          <input type="number" min="0" step="1" class="wl-input" data-wl-field="tokens"
            value="${w.tokens == null ? "" : esc(String(w.tokens))}" placeholder="—">
        </label>
        <label class="wl-cell">
          <span class="wl-cell-label">${esc(t("form.plan.window_costs"))}</span>
          <input type="number" min="0" step="0.01" class="wl-input" data-wl-field="costs"
            value="${w.costs == null ? "" : esc(String(w.costs))}" placeholder="—">
        </label>
        <label class="wl-cell wl-cell-secs">
          <span class="wl-cell-label">${esc(t("form.plan.window_secs"))}</span>
          <input type="number" min="1" step="1" class="wl-input" data-wl-field="window_secs"
            value="${w.window_secs == null ? "" : esc(String(w.window_secs))}" placeholder="60">
        </label>
      </div>`;
  }

  // Collect window_limits from a sub-editor container. Returns [] when the
  // single row is entirely empty (no limits set), otherwise [{...}].
  function collectWindowLimits(container) {
    if (!container) return [];
    const row = container.querySelector("[data-wl-row]");
    if (!row) return [];
    const get = (f) => {
      const el = row.querySelector(`[data-wl-field="${f}"]`);
      const v = el && el.value.trim();
      if (!v) return null;
      return f === "costs" ? Number(v) : Math.max(0, Math.floor(Number(v)));
    };
    const counts = get("counts");
    const tokens = get("tokens");
    const costs = get("costs");
    const secsRaw = row.querySelector('[data-wl-field="window_secs"]');
    const secsVal = secsRaw && secsRaw.value.trim();
    const window_secs = secsVal ? Math.max(1, Math.floor(Number(secsVal))) : 60;
    // window_secs alone is meaningless — only emit an entry if at least one
    // limit dimension is set. Otherwise the backend would treat the plan /
    // slot as having a window cap when the user actually left it blank.
    if (counts == null && tokens == null && costs == null) return [];
    return [{ counts, tokens, costs, window_secs }];
  }

  // ── Schedule sub-editor helpers ─────────────────────────
  function renderScheduleEditor(container, warningEl, slots) {
    if (!container) return;
    // Note: signature is (slot, idx) so it can be passed directly to
    // Array.prototype.map (which calls back with (element, index)).
    // Swapping these two would render "Slot [object Object]1" and read all
    // field values off the number index — silent data-loss bug.
    const renderSlot = (s, idx) => {
      s = s || {};
      // Parse hours "H:MM-HH:MM" (backend tolerates single-digit hours like
      // "9:00-21:00") into start/end for <input type="time">, which REQUIRES
      // the strict "HH:MM" 2-digit form. Without zero-pad the input renders
      // empty and the user thinks their config was lost.
      let start = "", end = "";
      if (s.hours && s.hours.includes("-")) {
        const [a, b] = s.hours.split("-");
        start = padTime((a || "").trim());
        end = padTime((b || "").trim());
      }
      const crossBadge = start && end && toMinutes(end) <= toMinutes(start)
        ? `<span class="slot-cross-badge">${esc(t("form.plan.slot_cross_midnight"))}</span>` : "";
      // Per-slot window-limits: a slot has at most ONE entry — render it as
      // a single row, no add/remove buttons.
      const wlRows = Array.isArray(s.window_limits) ? s.window_limits.map(normalizeWindowLimit).filter(Boolean) : [];
      return `
        <div class="schedule-slot-card" data-slot-idx="${idx}">
          <div class="slot-card-header">
            <span class="slot-card-title">${esc("Slot " + (idx + 1))}</span>
            <span class="slot-time">
              <input type="time" step="600" class="slot-time-input" data-slot-field="start" value="${esc(start)}" aria-label="${esc(t("form.plan.slot_start"))}">
              <span class="slot-dash">—</span>
              <input type="time" step="600" class="slot-time-input" data-slot-field="end" value="${esc(end)}" aria-label="${esc(t("form.plan.slot_end"))}">
              ${crossBadge}
            </span>
            <button type="button" class="slot-remove" aria-label="${esc(t("form.plan.remove_row"))}">×</button>
          </div>
          <div class="slot-card-body">
            <div class="slot-card-grid">
              <label class="slot-cell">${esc(t("form.plan.concurrency"))}<input type="number" min="0" step="1" class="slot-input" data-slot-field="concurrency_limit"
                value="${s.concurrency_limit == null ? "" : esc(String(s.concurrency_limit))}"></label>
              <label class="slot-cell">${esc(t("form.plan.rpm"))}<input type="number" min="0" step="1" class="slot-input" data-slot-field="rpm_limit"
                value="${s.rpm_limit == null ? "" : esc(String(s.rpm_limit))}"></label>
              <label class="slot-cell">${esc(t("form.plan.tpm"))}<input type="number" min="0" step="1" class="slot-input" data-slot-field="tpm_limit"
                value="${s.tpm_limit == null ? "" : esc(String(s.tpm_limit))}"></label>
            </div>
            <div class="slot-card-section">
              <div class="slot-section-label">${esc(t("form.plan.windows"))} ${tip(t("tip.plan.windows"))}</div>
              <div class="slot-wl-editor" data-slot-wl="${idx}"></div>
            </div>
          </div>
        </div>`;
    };
    const slotHtml = slots.map(renderSlot).join("");
    container.innerHTML = slotHtml + `<button type="button" class="slot-add btn-secondary btn-small">${esc(t("form.plan.add_slot"))}</button>`;
    // Wire up per-slot window-limits sub-editors.
    container.querySelectorAll("[data-slot-wl]").forEach((el) => {
      const slotIdx = Number(el.getAttribute("data-slot-wl"));
      const wlRows = Array.isArray(slots[slotIdx] && slots[slotIdx].window_limits)
        ? slots[slotIdx].window_limits.map(normalizeWindowLimit).filter(Boolean)
        : [];
      renderWindowLimitsEditor(el, wlRows, "slot-" + slotIdx + "-wl");
    });
    // Wire up remove buttons (after sub-editors render so children retain their
    // own listeners; removing the parent card tears them down automatically).
    container.querySelectorAll(".slot-remove").forEach((btn) => {
      btn.addEventListener("click", () => {
        const card = btn.closest("[data-slot-idx]");
        if (card) card.remove();
        renumberScheduleSlots(container);
        const slots2 = collectSchedule(container);
        renderScheduleWarning(warningEl, container, slots2);
      });
    });
    // Wire up add button.
    container.querySelector(".slot-add").addEventListener("click", () => {
      const nextIdx = container.querySelectorAll("[data-slot-idx]").length;
      container.insertAdjacentHTML("beforeend", renderSlot({}, nextIdx));
      const newCard = container.querySelector(`[data-slot-idx="${nextIdx}"]`);
      const wlHost = newCard.querySelector("[data-slot-wl]");
      renderWindowLimitsEditor(wlHost, [], "slot-" + nextIdx + "-wl");
      const rm = newCard.querySelector(".slot-remove");
      rm.addEventListener("click", () => {
        newCard.remove();
        renumberScheduleSlots(container);
        const slots2 = collectSchedule(container);
        renderScheduleWarning(warningEl, container, slots2);
      });
    });
  }

  // Pad "9:00" → "09:00" for <input type="time"> which requires strict HH:MM.
  // Tolerates already-padded values and odd whitespace.
  function padTime(hm) {
    if (!hm || !hm.includes(":")) return hm || "";
    const [h, ...rest] = hm.split(":");
    const m = rest.join(":");
    const hNum = parseInt(h, 10);
    if (isNaN(hNum)) return hm;
    return String(hNum).padStart(2, "0") + ":" + m;
  }

  // Renumber slot cards after add/remove so the "Slot N" labels stay in order.
  // Doesn't touch the underlying data — collectSchedule reads from current DOM.
  function renumberScheduleSlots(container) {
    if (!container) return;
    container.querySelectorAll("[data-slot-idx]").forEach((card, i) => {
      card.setAttribute("data-slot-idx", String(i));
      const title = card.querySelector(".slot-card-title");
      if (title) title.textContent = "Slot " + (i + 1);
      const wlHost = card.querySelector("[data-slot-wl]");
      if (wlHost) wlHost.setAttribute("data-slot-wl", String(i));
    });
  }

  // Read the schedule editor into the backend payload shape:
  // [{hours:"HH:MM-HH:MM", concurrency_limit, rpm_limit, tpm_limit, window_limits}, ...].
  function collectSchedule(container) {
    if (!container) return [];
    const out = [];
    container.querySelectorAll("[data-slot-idx]").forEach((card) => {
      const start = (card.querySelector('[data-slot-field="start"]') || {}).value || "";
      const end = (card.querySelector('[data-slot-field="end"]') || {}).value || "";
      const num = (f) => {
        const el = card.querySelector(`[data-slot-field="${f}"]`);
        const v = el && el.value.trim();
        if (!v) return null;
        return Math.max(0, Math.floor(Number(v)));
      };
      const wlHost = card.querySelector("[data-slot-wl]");
      const window_limits = collectWindowLimits(wlHost);
      out.push({
        hours: start && end ? start + "-" + end : "",
        concurrency_limit: num("concurrency_limit"),
        rpm_limit: num("rpm_limit"),
        tpm_limit: num("tpm_limit"),
        window_limits,
      });
    });
    return out;
  }

  // Convert "HH:MM" to minutes since midnight. Returns null on bad input.
  function toMinutes(hm) {
    if (!hm || !hm.includes(":")) return null;
    const [h, m] = hm.split(":").map((x) => parseInt(x, 10));
    if (isNaN(h) || isNaN(m)) return null;
    return h * 60 + m;
  }

  // Check overlap between schedule slots. Returns array of {i, j} conflict
  // pairs (0-indexed). Cross-midnight slots (end<=start) are split into
  // [start, 1440) ∪ [0, end); the standard interval-overlap test then applies.
  function validateScheduleOverlap(slots) {
    const ranges = slots
      .map((s) => {
        if (!s || !s.hours || !s.hours.includes("-")) return null;
        const [a, b] = s.hours.split("-");
        const start = toMinutes((a || "").trim());
        const end = toMinutes((b || "").trim());
        if (start == null || end == null) return null;
        if (start < end) return [[start, end]];
        if (start === end) return [[0, 1440]]; // empty range treats as full day
        return [[start, 1440], [0, end]];
      });
    const conflicts = [];
    for (let i = 0; i < ranges.length; i++) {
      if (!ranges[i]) continue;
      for (let j = i + 1; j < ranges.length; j++) {
        if (!ranges[j]) continue;
        const overlap = ranges[i].some((ra) =>
          ranges[j].some((rb) => Math.max(ra[0], rb[0]) < Math.min(ra[1], rb[1]))
        );
        if (overlap) conflicts.push({ i, j });
      }
    }
    return conflicts;
  }

  // Render the live overlap warning + apply/remove red border on conflicting
  // slot cards. Called on every input/change in the schedule editor.
  function renderScheduleWarning(warningEl, container, slots) {
    if (!warningEl || !container) return;
    const conflicts = validateScheduleOverlap(slots);
    container.querySelectorAll(".schedule-slot-card").forEach((card, idx) => {
      const inConflict = conflicts.some((c) => c.i === idx || c.j === idx);
      card.classList.toggle("slot-conflict", inConflict);
    });
    if (!conflicts.length) {
      warningEl.innerHTML = "";
      return;
    }
    const lines = conflicts
      .map((c) => "<div>" + esc(t("form.plan.schedule_conflict", { i: c.i + 1, j: c.j + 1 })) + "</div>")
      .join("");
    warningEl.innerHTML = lines;
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
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("key_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.key.alias")} ${tip(t("tip.key.alias"))}</label><input id="m-key-alias"></div>
            <div class="form-group"><label>${t("form.key.prefix")} ${tip(t("tip.key.prefix"))}</label><input id="m-key-prefix" placeholder="e.g. prod, TeamA, v2" pattern="[a-zA-Z0-9]{1,50}" maxlength="50"></div>
            <div class="form-group"><label>${t("form.key.tag")} ${tip(t("tip.key.tag"))}</label><input id="m-key-tag" placeholder="e.g. production, customer-acme, exp-2026Q1" maxlength="64"></div>
            <div class="form-group"><label>${t("form.key.user_id")} ${tip(t("tip.key.user_id"))}</label><input id="m-key-user"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("key_card.assignment")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.key.team")} ${tip(t("tip.key.team"))}</label><select id="m-key-team"><option value="">${t("common.none_option")}</option></select></div>
            <div class="form-group"><label>${t("form.key.plan")} ${tip(t("tip.key.plan"))}</label><select id="m-key-plan"><option value="">${t("form.key.plan_default")}</option><option value="__no_plan__">${t("form.key.plan_no_plan")}</option></select></div>
            <div class="form-group field-full"><label>${t("form.key.models")} ${tip(t("tip.key.models"))}</label><div class="model-check-combo" id="m-key-models-combo"></div></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("key_card.limits")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.key.max_budget")} ${tip(t("tip.key.max_budget"))}</label><input id="m-key-budget" type="number" step="0.01"></div>
            <div class="form-group"><label>${t("form.key.rpm")} ${tip(t("tip.key.rpm"))}</label><input id="m-key-rpm" type="number"></div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-key-submit">${t("action.create")}</button>
      </div>
    `, { xwide: true });
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
    getKeyPlanNames().then((names) => {
      const sel = document.getElementById("m-key-plan");
      if (sel) names.forEach((n) => { const o = document.createElement("option"); o.value = n; o.textContent = n; sel.appendChild(o); });
    });
    // Helper: convert select value to API payload. Three states:
    //   ""            → field omitted (use default_plan at runtime)
    //   "__no_plan__" → null (explicit opt-out, no default fallback)
    //   "{name}"      → name string
    const planPayload = () => {
      const v = document.getElementById("m-key-plan").value;
      if (v === "__no_plan__") return null;
      if (v === "") return undefined; // omitted from JSON body entirely
      return v;
    };
    document.getElementById("m-key-submit").addEventListener("click", async () => {
      try {
        const modelsVal = getComboModels("m-key-models-combo");
        const data = await api("/admin/keys", {
          method: "POST",
          body: JSON.stringify({
            key_alias: document.getElementById("m-key-alias").value.trim() || null,
            key_prefix: document.getElementById("m-key-prefix").value.trim() || null,
            tag: document.getElementById("m-key-tag").value.trim() || null,
            user_id: document.getElementById("m-key-user").value || null,
            team_id: document.getElementById("m-key-team").value || null,
            models: modelsVal || ["all-team-models"],
            max_budget: document.getElementById("m-key-budget").value ? Number(document.getElementById("m-key-budget").value) : null,
            rpm_limit: document.getElementById("m-key-rpm").value ? Number(document.getElementById("m-key-rpm").value) : null,
            ...(planPayload() === undefined ? {} : { plan_name: planPayload() }),
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

  function showImportHelpModal() {
    const jsonlTemplate =
`{"key_alias":"alice","key_name":"Alice Wang","key_prefix":"prod","tag":"production","user_id":"alice","team_id":"team-eng","models":["gpt-4","claude-3"],"rpm_limit":60,"tpm_limit":100000,"max_budget":100.0,"budget_duration":"30d","expires":"2026-12-31 23:59:59","metadata":{"env":"prod","tier":"paid"},"plan_name":"default"}
{"key_alias":"bob","key_prefix":"stg","tag":"staging","models":["all-team-models"],"team_id":"team-eng"}
{"key_alias":"ci-runner","key_prefix":"ci","tag":"automation","models":["gpt-4"],"rpm_limit":30}`;
    const csvTemplate =
`key_alias,key_name,key_prefix,tag,user_id,team_id,models,rpm_limit,tpm_limit,max_budget,budget_duration,expires,metadata,plan_name
alice,Alice Wang,prod,production,alice,team-eng,"gpt-4|claude-3",60,100000,100.0,30d,"2026-12-31 23:59:59","{""env"":""prod"",""tier"":""paid""}",default
bob,,stg,staging,,team-eng,all-team-models,,,,,,,
ci-runner,,ci,automation,,,gpt-4,30,,,,,,`;
    const fieldsTable = t("keys.import_help.fields_table");
    showModal(`
      <h3>${t("keys.import_help.title")}</h3>
      <p class="muted">${t("keys.import_help.intro")}</p>

      <h4>${t("keys.import_help.fields_title")}</h4>
      ${fieldsTable}

      <h4>${t("keys.import_help.jsonl_title")} <span class="muted" style="font-size:11px">${t("keys.import_help.copy_hint")}</span></h4>
      <pre id="help-jsonl" style="background:var(--surface3);padding:8px;border-radius:4px;font-size:11px;overflow-x:auto;max-height:200px">${esc(jsonlTemplate)}</pre>
      <button class="btn-small" onclick="window._copyText(this, document.getElementById('help-jsonl').textContent)">${t("keys.import_help.copy_jsonl")}</button>

      <h4>${t("keys.import_help.csv_title")}</h4>
      <pre id="help-csv" style="background:var(--surface3);padding:8px;border-radius:4px;font-size:11px;overflow-x:auto;max-height:200px">${esc(csvTemplate)}</pre>
      <button class="btn-small" onclick="window._copyText(this, document.getElementById('help-csv').textContent)">${t("keys.import_help.copy_csv")}</button>

      <div class="modal-actions">
        <button class="btn-primary" onclick="hideModal()">${t("action.close")}</button>
      </div>
    `, { xwide: true });
  }

  function showImportKeysModal() {
    showModal(`
      <h3>${t("keys.import.title")}</h3>
      <p class="muted">${t("keys.import.intro")}</p>
      <div class="form-group">
        <label>${t("keys.import.file_label")}</label>
        <input type="file" id="m-import-file" accept=".jsonl,.csv">
      </div>
      <details style="margin:8px 0">
        <summary class="muted" style="cursor:pointer">${t("keys.import_help.format_ref")}</summary>
        <pre style="background:var(--surface3);padding:8px;border-radius:4px;font-size:11px;overflow-x:auto">${t("keys.import.sample_block")}</pre>
      </details>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-import-submit">${t("keys.import.submit")}</button>
      </div>
    `, { xwide: true });
    document.getElementById("m-import-submit").addEventListener("click", async () => {
      const fileInput = document.getElementById("m-import-file");
      const file = fileInput && fileInput.files && fileInput.files[0];
      if (!file) { alert(t("keys.import.no_file")); return; }
      const fd = new FormData();
      fd.append("file", file);
      try {
        const resp = await fetch("/dashboard/api/admin/keys/import", {
          method: "POST",
          body: fd,
          credentials: "include",
        });
        const data = await resp.json();
        if (!resp.ok) {
          alert(t("keys.import.failed", { reason: data.error || resp.status }));
          return;
        }
        renderImportResult(data);
      } catch (err) { alert(t("keys.import.error", { reason: err.message })); }
    });
  }

  function renderImportResult(data) {
    const createdRows = (data.created || []).map((c) => `<tr>
      <td class="mono">${esc(c.key_alias || "-")}</td>
      <td class="mono" style="font-size:11px">${esc(c.key)}</td>
      <td><button class="btn-small" onclick="window._copyText(this,'${esc(c.key)}')">${t("action.copy")}</button></td>
    </tr>`).join("");
    const skippedRows = (data.skipped || []).map((s) => `<tr>
      <td>${esc(s.key_alias || "-")}</td>
      <td class="muted">${esc(s.reason)}</td>
    </tr>`).join("");
    const parseErrorRows = (data.parse_errors || []).map((e) => `<tr>
      <td class="mono">${t("keys.import.line_n", { n: e.line })}</td>
      <td class="muted">${esc(e.reason)}</td>
    </tr>`).join("");
    // Download button: server returns a same-format attachment with the
    // generated api_key column/field appended. We trigger it client-side
    // via Blob so we don't need a second round-trip.
    const dl = data.download;
    const downloadBlock = (dl && dl.content) ? `
      <div class="form-card" style="margin:12px 0;background:var(--surface3)">
        <div style="display:flex;align-items:center;justify-content:space-between;gap:12px">
          <div>
            <div style="font-weight:600">${t("keys.import.download_ready")}</div>
            <div class="muted" style="font-size:12px;margin-top:4px">
              ${t("keys.import.download_hint", { rows: dl.rows, file: `<code>${esc(dl.filename)}</code>` })}
            </div>
          </div>
          <button class="btn-primary" id="m-import-download">${t("keys.import.download_btn")}</button>
        </div>
      </div>` : "";
    showModal(`
      <h3>${t("keys.import.result_title")}</h3>
      <p>${t("keys.import.result_summary", {
        file: `<code>${esc(data.file_name)}</code>`,
        parsed: `<b>${data.parsed}</b>`,
        created: `<b style="color:var(--success)">${data.created_count}</b>`,
        skipped: `<b style="color:var(--danger)">${data.skipped_count}</b>`,
      })}</p>
      ${downloadBlock}
      ${data.created && data.created.length ? `
        <h4>${t("keys.import.created_title")} <span class="muted" style="font-size:11px">${t("keys.import.copy_now_hint")}</span></h4>
        <table style="width:100%">
          <tr><th>${t("keys.import.col_alias")}</th><th>${t("keys.import.col_raw_key")}</th><th></th></tr>
          ${createdRows}
        </table>` : ""}
      ${data.skipped && data.skipped.length ? `
        <h4>${t("keys.import.skipped_title")}</h4>
        <table style="width:100%">
          <tr><th>${t("keys.import.col_alias")}</th><th>${t("keys.import.col_reason")}</th></tr>
          ${skippedRows}
        </table>` : ""}
      ${data.parse_errors && data.parse_errors.length ? `
        <h4>${t("keys.import.parse_errors_title")}</h4>
        <table style="width:100%">
          <tr><th>${t("keys.import.col_location")}</th><th>${t("keys.import.col_reason")}</th></tr>
          ${parseErrorRows}
        </table>` : ""}
      <div class="modal-actions">
        <button class="btn-primary" onclick="hideModal(); window._loadKeysPage();">${t("action.done")}</button>
      </div>
    `, { xwide: true });
    if (dl && dl.content) {
      const btn = document.getElementById("m-import-download");
      if (btn) btn.addEventListener("click", () => {
        const blob = new Blob([dl.content], { type: dl.mime || "text/plain" });
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = dl.filename;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        URL.revokeObjectURL(url);
      });
    }
  }

  async function showEditKeyModal(key) {
    const existingModels = Array.isArray(key.models) ? key.models : [];
    const isVip = key.metadata && key.metadata.vip === true;
    const isPromptLogExcluded = (window._promptLogExcludedKeys || []).includes(key.token_hash);
    // Plans drive the assignment dropdown — load before rendering so the
    // <select> can populate synchronously.
    // Build team options for the editable team_id select. Uses window._teams
    // cache (populated by loadQuotaOverview); falls back to fetching overview
    // if cold. The first option is "no team" (value=""), which sets team_id
    // to NULL via the backend's empty-string→NULL translation.
    const teamOptionsHtml = await (async () => {
      let teams = window._teams;
      if (!teams) {
        try {
          const data = await api("/admin/quota/overview");
          teams = data.teams || [];
          window._teams = teams;
        } catch { teams = []; }
      }
      const opts = [`<option value="">${t("common.none_option")}</option>`];
      teams.forEach((tm) => {
        const sel = (tm.team_id === key.team_id) ? "selected" : "";
        opts.push(`<option value="${esc(tm.team_id)}" ${sel}>${esc(tm.team_alias || tm.team_id)}</option>`);
      });
      return opts.join("");
    })();
    const planNames = await getKeyPlanNames();
    // Three plan-assignment states map to <select> values:
    //   ""              → no DB row (follows default_plan at runtime)
    //   "__no_plan__"   → row with plan_name IS NULL (explicit opt-out)
    //   "{plan_name}"   → row with plan_name = name
    let currentPlanSel = "";
    if (key.plan_assignment_kind === "no_plan") {
      currentPlanSel = "__no_plan__";
    } else if (key.plan_assignment_kind === "plan") {
      currentPlanSel = key.plan_name || "";
    }
    const planOptions = [
        { value: "", label: t("form.key.plan_default") },
        { value: "__no_plan__", label: t("form.key.plan_no_plan") },
      ]
      .concat(planNames.map((n) => ({ value: n, label: n })))
      .map((o) => `<option value="${esc(o.value)}" ${o.value === currentPlanSel ? "selected" : ""}>${esc(o.label)}</option>`)
      .join("");
    const __html = `
      <h3>${t("form.key.title_edit")}</h3>
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("key_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.key.key_name")} ${tip(t("tip.key.key_name"))}</label><input id="m-edit-key-name" value="${esc(key.key_name || "")}"></div>
            <div class="form-group"><label>${t("form.key.alias")} ${tip(t("tip.key.alias_short"))}</label><input id="m-edit-alias" value="${esc(key.key_alias || "")}"></div>
            <div class="form-group"><label>${t("form.key.user_id")} ${tip(t("tip.key.user_id_short"))}</label><input id="m-edit-user" value="${esc(key.user_id || "")}"></div>
            <div class="form-group"><label>${t("form.key.team_id")} ${tip(t("tip.key.team_id"))}</label><select id="m-edit-team" style="background:var(--surface3)">${teamOptionsHtml}</select></div>
            <div class="form-group"><label>${t("form.key.prefix")} ${tip(t("tip.key.prefix_readonly"))}</label><input value="${esc(key.key_prefix ? "sk-" + key.key_prefix + "-***" : "sk-***")}" readonly style="background:var(--surface3);cursor:not-allowed;font-family:var(--mono);font-size:12px"></div>
            <div class="form-group field-full"><label>${t("form.key.tag")} ${tip(t("tip.key.tag_clearable"))}</label><input id="m-edit-tag" value="${esc(key.tag || "")}" maxlength="64"></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("key_card.assignment")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.key.models")} ${tip(t("tip.key.models"))}</label><div class="model-check-combo" id="m-edit-models-combo"></div></div>
            <div class="form-group field-full"><label>${t("form.key.plan")} ${tip(t("tip.key.plan_edit"))}</label><select id="m-edit-plan">${planOptions}</select></div>
          </div>
        </div>
        <div class="form-card">
          <div class="form-card-title">${t("key_card.limits")}</div>
          <div class="form-card-grid">
            <div class="form-group"><label>${t("form.key.rpm")} ${tip(t("tip.key.rpm_edit"))}</label><input id="m-edit-rpm" type="number" min="0" value="${key.rpm_limit != null ? key.rpm_limit : ""}"></div>
            <div class="form-group"><label>${t("form.key.tpm")} ${tip(t("tip.key.tpm_edit"))}</label><input id="m-edit-tpm" type="number" min="0" value="${key.tpm_limit != null ? key.tpm_limit : ""}"></div>
            <div class="form-group"><label>${t("form.key.max_budget")} ${tip(t("tip.key.max_budget_edit"))}</label><input id="m-edit-budget" type="number" step="0.01" min="0" value="${key.max_budget != null ? key.max_budget : ""}"></div>
            <div class="form-group"><label>${t("form.key.budget_duration")} ${tip(t("tip.key.budget_duration"))}</label><input id="m-edit-budget-duration" value="${esc(key.budget_duration || "")}" placeholder="1d / 7d / 30d"></div>
            <div class="form-group field-full"><label>${t("form.key.expires")} ${tip(t("tip.key.expires"))}</label><input id="m-edit-expires" value="${esc(key.expires || "")}" placeholder="2026-12-31 23:59:59" style="font-family:var(--mono);font-size:12px"></div>
            <div class="form-group field-full"><label>${t("form.key.vip")} ${tip(t("tip.key.vip"))}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-vip" ${isVip ? "checked" : ""}><span style="font-weight:600;color:#b45309;white-space:nowrap">${t("form.key.vip_label")}</span></div></div>
            <div class="form-group field-full"><label>${t("form.key.prompt_log")} ${tip(t("tip.key.prompt_log"))}</label><div style="display:flex;align-items:center;gap:8px;padding-top:4px"><input type="checkbox" id="m-edit-no-prompt-log" ${isPromptLogExcluded ? "checked" : ""}><span style="font-weight:600;color:#dc2626;white-space:nowrap">${t("form.key.prompt_log_label")}</span></div></div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-edit-submit">${t("action.save")}</button>
      </div>
    `;
    showModal(__html, { xwide: true });
    // Populate model checkbox combo with existing models pre-checked
    getModelNames().then((names) => {
      const container = document.getElementById("m-edit-models-combo");
      if (container) initModelCombo(container, existingModels, names);
    });
    document.getElementById("m-edit-submit").addEventListener("click", async () => {
      try {
        const keyNameVal = document.getElementById("m-edit-key-name").value.trim();
        const aliasVal = document.getElementById("m-edit-alias").value.trim();
        const userVal = document.getElementById("m-edit-user").value.trim();
        const tagVal = document.getElementById("m-edit-tag").value.trim();
        const modelsVal = getComboModels("m-edit-models-combo");
        const vipChecked = document.getElementById("m-edit-vip").checked;
        const budgetDurVal = document.getElementById("m-edit-budget-duration").value.trim();
        const expiresVal = document.getElementById("m-edit-expires").value.trim();
        // Preserve existing metadata fields, only update vip flag.
        const existingMeta = key.metadata && typeof key.metadata === "object" ? key.metadata : {};
        const body = {
          key_name: keyNameVal || null,
          key_alias: aliasVal || null,
          user_id: userVal || null,
          // team_id: empty string = remove from team (NULL), value = move to
          // that team. Always send the select's value so COALESCE+CASE on the
          // backend writes NULL or the new id — null would skip the update.
          team_id: document.getElementById("m-edit-team").value,
          // tag: empty string clears, null leaves untouched. Always send the
          // trimmed value so COALESCE on the backend either writes "" or the
          // new label — never silently preserves stale tag.
          tag: tagVal,
          models: modelsVal || ["all-team-models"],
          max_budget: document.getElementById("m-edit-budget").value ? Number(document.getElementById("m-edit-budget").value) : null,
          rpm_limit: document.getElementById("m-edit-rpm").value ? Number(document.getElementById("m-edit-rpm").value) : null,
          tpm_limit: document.getElementById("m-edit-tpm").value ? Number(document.getElementById("m-edit-tpm").value) : null,
          // budget_duration/expires: empty string means "clear" (not "skip"),
          // so send "" rather than null when the user wiped the field.
          budget_duration: budgetDurVal,
          expires: expiresVal || null,
          metadata: Object.assign({}, existingMeta, { vip: vipChecked }),
        };
        await api(`/admin/keys/${encodeURIComponent(key.token_hash)}`, {
          method: "PUT",
          body: JSON.stringify(body),
        });
        // Plan assignment lives in a separate table (boom_key_plan_assignment)
        // so it has its own endpoints. Sync only when the dropdown changed —
        // avoids a spurious POST/DELETE churn on every save.
        //   ""            → DELETE (row goes away, follows default_plan)
        //   "__no_plan__" → POST with plan_name=null (explicit opt-out)
        //   "{name}"      → POST with plan_name=name
        const newPlanSel = document.getElementById("m-edit-plan").value;
        if (newPlanSel !== currentPlanSel) {
          if (newPlanSel === "") {
            await api(`/admin/assignments/${encodeURIComponent(key.token_hash)}`, { method: "DELETE" });
          } else if (newPlanSel === "__no_plan__") {
            await api("/admin/assignments", {
              method: "POST",
              body: JSON.stringify({ key_hash: key.token_hash, plan_name: null }),
            });
          } else {
            await api("/admin/assignments", {
              method: "POST",
              body: JSON.stringify({ key_hash: key.token_hash, plan_name: newPlanSel }),
            });
          }
        }
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
  // `planName` is merged into the block title so the operator sees the plan
  // name and its limits as one unit instead of a detached badge in the header.
  function renderTeamEffectiveLimits(el, planName, planExplicit) {
    const INF = "∞";
    const fmtOrInf = (v, fmt) => (v == null ? INF : fmt(v));
    const fmtNum = (v) => formatNumber(Number(v));
    const fmtCost = (v) => "¥" + (Number(v) || 0).toLocaleString(undefined, { maximumFractionDigits: 2 });

    // Title line: "套餐规格 · <planName>" or "套餐规格 · 默认/无套餐".
    // planExplicit=false means the team is falling back to the default team plan
    // (or has no plan at all) — we render that state in the title so the operator
    // doesn't have to infer it from the absence of a badge.
    let title = t("quota.team_limits_title");
    if (planName) {
      const tag = planExplicit ? "" : ` <span class="team-limits-suffix">${esc(t("quota.team_plan_default"))}</span>`;
      title += ` · <strong class="${planExplicit ? "team-limits-name-explicit" : "team-limits-name-implicit"}">${esc(planName)}</strong>${tag}`;
    } else {
      title += ` · <span class="team-limits-suffix">${esc(t("quota.team_plan_none"))}</span>`;
    }

    if (!el) {
      return `<div class="team-limits-block">
        <div class="team-limits-title">${title}</div>
        <div class="plan-tags"><span class="muted">${esc(t("quota.team_limits_empty") || "—")}</span></div>
      </div>`;
    }

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
      <div class="team-limits-title">${title}</div>
      <div class="plan-tags">${tags.join("")}</div>
    </div>`;
  }

  // Render the team's allowed-models list as chips. `models` comes straight
  // from boom_team_table.models. Per project convention (CLAUDE.md): an empty
  // array OR ["all-team-models"] means full access to all models — both must
  // render as the "all models" chip. Only an explicit list of model_names
  // limits the team to those entries.
  function renderTeamModels(models) {
    const list = Array.isArray(models) ? models : [];
    let body;
    if (list.length === 0 || list.includes("all-team-models")) {
      body = `<span class="model-chip model-chip-all">${esc(t("quota.team_models_all"))}</span>`;
    } else {
      body = list.map((m) => `<span class="model-chip">${esc(m)}</span>`).join("");
    }
    return `<div class="team-models-block">
      <div class="team-models-title">${esc(t("quota.team_models_title"))}</div>
      <div class="team-models-list">${body}</div>
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
      const isExcluded = (window._promptLogExcludedTeams || []).includes(t1.team_id);
      const promptLogBtn = `<button class="btn-small ${isExcluded ? "btn-secondary" : "btn-primary"}" onclick="window._toggleTeamPromptLog('${esc(t1.team_id)}', ${isExcluded})">${isExcluded ? t("quota.prompt_log_off") : t("quota.prompt_log_on")}</button>`;
      const planSelect = renderTeamPlanSelect(t1.team_id, t1.plan_name, t1.plan_explicit);
      const limitsHtml = renderTeamEffectiveLimits(t1.effective_limits, t1.plan_name, t1.plan_explicit);
      const modelsHtml = renderTeamModels(t1.models);
      return `<div class="quota-card">
        <div class="quota-card-header">
          <strong>${esc(t1.team_alias || t1.team_id)}</strong>
        </div>
        <div class="quota-card-stats">
          <div><span class="muted">${esc(t("teams.col.keys_count") || "Keys")}</span> <strong>${t1.key_count || 0}</strong></div>
          <div><span class="muted">${esc(t("quota.col.tokens"))}</span> <strong>${formatNumber(tokens)}</strong></div>
          <div><span class="muted">${esc(t("quota.col.cost"))}</span> <strong>$${esc(t1.total_cost || "0")}</strong></div>
        </div>
        ${modelsHtml}
        ${limitsHtml}
        <div class="quota-card-actions">
          <button class="btn-small" onclick="window._editTeam('${esc(t1.team_id)}')">${t("action.edit")}</button>
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
        const removeBtn = teamId
          ? `<button class="btn-small btn-secondary" onclick="window._removeKeyFromTeam('${esc(k.token)}', this)">${t("quota.remove_from_team")}</button>`
          : "";
        return `<tr class="quota-key-row" data-token="${esc(k.token)}">
          <td>
            <strong>${esc(k.key_alias || k.key_name || k.token_prefix)}</strong>
            ${k.user_id ? `<br><span class="muted">${esc(k.user_id)}</span>` : ""}
            ${k.blocked ? `<br><span class="badge-danger">blocked</span>` : ""}
          </td>
          <td>${renderKeyPlanCell(k)}</td>
          <td>${k.concurrency || 0}</td>
          <td>${formatNumber(tokens)}</td>
          <td>$${esc(k.total_cost || "0")}</td>
          <td>
            <button class="btn-small" onclick="window._quotaToggleWindows('${esc(k.token)}', this)">${t("quota.expand_windows")}</button>
            ${removeBtn}
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
          display = "¥" + (d.current || "0") + " / " + (limit > 0 ? "¥" + (d.limit || "0") : t("common.unlimited"));
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
      alert(t("common.error_prefix", { message: err.message }));
    }
  };

  // Remove a key from its team by PUTting team_id="" (backend translates
  // empty string to NULL). Only shown on team detail pages (teamId set).
  window._removeKeyFromTeam = async (token, btn) => {
    if (!confirm(t("quota.confirm_remove_from_team"))) return;
    btn.disabled = true;
    try {
      await api(`/admin/keys/${encodeURIComponent(token)}`, {
        method: "PUT",
        body: JSON.stringify({ team_id: "" }),
      });
      const tid = _currentQuotaTeamId();
      _loadQuotaKeys(tid);
    } catch (err) {
      alert(t("common.error_prefix", { message: err.message }));
    } finally {
      btn.disabled = false;
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
      alert(t("common.error_prefix", { message: err.message }));
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
      <div class="form-grid">
        <div class="form-card">
          <div class="form-card-title">${t("team_card.basic")}</div>
          <div class="form-card-grid">
            <div class="form-group field-full"><label>${t("form.team.id")} ${tip(t("tip.team.id"))}</label><input id="m-team-id" value="${esc(p.team_id || "")}" ${p.team_id ? "readonly" : ""} required></div>
            <div class="form-group"><label>${t("form.team.alias")} ${tip(t("tip.team.alias"))}</label><input id="m-team-alias" value="${esc(p.team_alias || "")}"></div>
            <div class="form-group"><label>${t("teams.col.plan")} ${tip(t("tip.team.plan"))}</label>
              <select id="m-team-plan">
                <option value="">${esc(t("teams.plan_use_default"))}${tps.default_team_plan ? " (" + tps.default_team_plan + ")" : ""}</option>
              </select>
            </div>
            <div class="form-group field-full"><label>${t("form.team.models")} ${tip(t("tip.team.models"))}</label><div class="model-check-combo" id="m-team-models-combo"></div></div>
          </div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="btn-secondary btn-inline" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-team-submit">${p.team_id ? t("action.update") : t("action.create")}</button>
      </div>
    `, { xwide: true });
    getModelNames().then((names) => {
      const container = document.getElementById("m-team-models-combo");
      if (container) initModelCombo(container, p.models || [], names, true);
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
          // Team full-access submits `[]` (empty array) per litellm semantic.
          // Legacy rows with ["all-team-models"] still render as full-access
          // via renderTeamModels/formatTeamModels — no migration needed.
          models: modelsVal || [],
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

  // Edit entry for the team card: prefills showCreateTeamModal with the
  // team's current fields. Modal handles id (readonly), alias, models, and
  // explicit plan assignment (the only four editable fields on a team).
  window._editTeam = (teamId) => {
    const team = (window._teams || []).find((x) => x.team_id === teamId);
    if (!team) {
      alert(t("common.error_prefix", { message: "team not found in cache" }));
      return;
    }
    showCreateTeamModal({
      team_id: team.team_id,
      team_alias: team.team_alias,
      models: Array.isArray(team.models) ? team.models : [],
    });
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
  // Monotonic token used by loadLogs to drop stale responses. Each call
  // bumps the token; when the awaited fetch returns, if its captured token
  // no longer equals the live one, the response is discarded — so a slow
  // earlier request can't overwrite a newer one's render. Without this,
  // typing fast in a column filter (e.g. "张" then "张三") could see the
  // broader "张" result land *after* the narrower "张三" one and clobber
  // the table back to a wider set — or vice versa, with the narrower result
  // landing first then the wider one arriving late. Same for hashchange
  // re-entering admin-logs mid-typing.
  let logsLoadToken = 0;

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
    // Capture this request's token. When the fetch returns, if it doesn't
    // match the live token, a newer request has superseded this one — drop
    // the result on the floor so it can't clobber a fresher render.
    const myToken = ++logsLoadToken;
    try {
      let url = `/admin/logs?page=${logsPage}&per_page=50`;
      for (const [k, v] of Object.entries(logsFilters)) {
        url += `&${encodeURIComponent(k)}=${encodeURIComponent(v)}`;
      }
      const data = await api(url);
      if (myToken !== logsLoadToken) return;
      renderLogsTable(data.logs || []);
      renderLogsPagination(data);
    } catch (err) {
      if (myToken !== logsLoadToken) return;
      const tbody = document.getElementById("logs-tbody");
      if (tbody) tbody.innerHTML = `<tr><td colspan="14" class="no-results">${t("logs.failed", { message: esc(err.message) })}</td></tr>`;
      const pg = document.getElementById("logs-pagination");
      if (pg) pg.innerHTML = "";
    }
  }

  function renderLogsTable(logs) {
    const tbody = document.getElementById("logs-tbody");
    if (!tbody) return;
    if (logs.length === 0) {
      tbody.innerHTML = '<tr><td colspan="15" class="no-results">' + t("common.no_matching", { what: t("logs.title").toLowerCase() }) + '</td></tr>';
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
        var pct = (l.cached_tokens != null && l.input_tokens > 0)
          ? fmtPrefixHit(l.cached_tokens / l.input_tokens * 100)
          : "-";
        // IN/OUT: show input and output tokens in one cell, slash-separated.
        var inOutCell = (l.input_tokens != null || l.output_tokens != null)
          ? formatNumber(l.input_tokens) + " / " + formatNumber(l.output_tokens)
          : "- / -";
        var kvCell = '<span class="mono">' + esc(pct) + "</span>";
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

  // ── Debug page: KVC DFX table ─────────────────────
  // Show the Debug nav link if compiled with debug-tools.
  if (window.__KVC_DEBUG) {
    var navDebug = document.getElementById("nav-admin-debug");
    if (navDebug) navDebug.style.display = "";
  }

  // Filter state for the KVC DFX table (mirrors the Logs page filter pattern).
  var kvcFilters = {};
  var kvcFiltersTimer = null;
  var kvcPage = 1;

  // Table shell (header + filter row). Built once; only <tbody> is re-rendered
  // on each load so an active filter input keeps focus.
  function kvcDfxShellHtml() {
    return '<table class="data-table" id="kvc-dfx-table"><thead>' +
      '<tr>' +
        '<th>Time</th><th>IP</th><th>Team</th><th>Key</th><th>Model</th><th>Path</th>' +
        '<th>Status</th><th>Stream</th><th>IN/OUT</th><th>Prefix Hit</th>' +
        '<th>Policy</th><th>Req Bytes</th><th>Duration</th><th>TTFT</th><th>Error</th>' +
      '</tr>' +
      '<tr class="filter-row">' +
        '<td></td>' +
        '<td><input class="col-filter" data-col="client_ip" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="team_alias" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="key_alias" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="model" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="api_path" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="status_code" placeholder="filter"></td>' +
        '<td><input class="col-filter" data-col="stream" placeholder="filter"></td>' +
        '<td></td><td></td><td></td><td></td><td></td><td></td>' +
        '<td><input class="col-filter" data-col="error" placeholder="filter"></td>' +
      '</tr>' +
      '</thead><tbody id="kvc-dfx-tbody"></tbody></table>' +
      '<div id="kvc-dfx-pagination" class="pagination"></div>';
  }

  function renderKvcDfxRow(l) {
    var inOut = (l.input_tokens != null || l.output_tokens != null)
      ? formatNumber(l.input_tokens) + " / " + formatNumber(l.output_tokens) : "- / -";
    var pct = (l.cached_tokens != null && l.input_tokens > 0)
      ? fmtPrefixHit(l.cached_tokens / l.input_tokens * 100) : "-";
    var trieHit = (l.kv_hit_blocks != null && l.kv_input_blocks != null && l.kv_input_blocks > 0)
      ? (l.kv_hit_blocks / l.kv_input_blocks * 100).toFixed(1) + "%" : "-";
    var trieFill = (l.trie_blocks != null && l.trie_max_blocks != null && l.trie_max_blocks > 0)
      ? (l.trie_blocks / l.trie_max_blocks * 100).toFixed(2) + "%" : "-";
    var modelVal = esc(l.model || "-");
    if (l.model && l.model.includes(":")) {
      var mp = l.model.split(":");
      modelVal = '<span class="log-model-name">' + esc(mp[0]) + '</span><span class="log-model-sep">:</span><span class="log-model-deploy">' + esc(mp.slice(1).join(":")) + '</span>';
    }
    // Trie-Hit and Trie Fill are folded into the Prefix Hit cell.
    var prefixCell =
      '<div class="kvc-pct">' + esc(pct) + '</div>' +
      '<div class="kvc-sub">Trie-Hit ' + esc(trieHit) + '</div>' +
      '<div class="kvc-sub">Trie Fill ' + esc(trieFill) + '</div>';
    return '<tr>' +
      '<td class="mono">' + esc(formatTimestamp(l.created_at)) + '</td>' +
      '<td class="mono">' + esc(l.client_ip || "-") + '</td>' +
      '<td>' + esc(l.team_alias || l.team_id || "-") + '</td>' +
      '<td>' + esc(l.key_alias || l.key_name || "-") + '</td>' +
      '<td>' + modelVal + '</td>' +
      '<td class="mono">' + esc(l.api_path) + '</td>' +
      '<td>' + (l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code) + '</td>' +
      '<td>' + (l.is_stream ? t("common.yes") : t("common.no")) + '</td>' +
      '<td class="mono">' + inOut + '</td>' +
      '<td class="mono kvc-prefix">' + prefixCell + '</td>' +
      '<td class="mono">' + esc(l.policy || "-") + '</td>' +
      '<td class="mono">' + (l.request_tokens != null ? formatNumber(l.request_tokens) : "-") + '</td>' +
      '<td>' + (l.duration_ms != null ? l.duration_ms + "ms" : "-") + '</td>' +
      '<td>' + (l.ttft_ms != null ? l.ttft_ms + "ms" : "-") + '</td>' +
      '<td>' + (l.error_message ? '<span style="color:var(--danger)" title="' + esc(l.error_message) + '">' + esc((l.error_type || "").substring(0, 20)) + '</span>' : "-") + '</td>' +
      '</tr>';
  }

  function setupKvcFilters() {
    var table = document.getElementById("kvc-dfx-table");
    if (!table) return;
    // Debounced column filters forwarded to /admin/logs as query params.
    table.addEventListener("input", function(e) {
      if (!e.target.classList.contains("col-filter")) return;
      var inp = e.target;
      clearTimeout(kvcFiltersTimer);
      kvcFiltersTimer = setTimeout(function() {
        var col = inp.dataset.col;
        var val = inp.value.trim();
        if (val) { kvcFilters[col] = val; } else { delete kvcFilters[col]; }
        loadKvcDfx(1);
      }, 400);
    });
    var resetBtn = document.getElementById("btn-reset-kvc-filters");
    if (resetBtn) {
      resetBtn.addEventListener("click", function() {
        kvcFilters = {};
        table.querySelectorAll(".col-filter").forEach(function(inp) { inp.value = ""; });
        loadKvcDfx(1);
      });
    }
  }

  async function loadKvcDfx(page) {
    if (page !== undefined) kvcPage = page;
    var wrap = document.getElementById("kvc-dfx-wrap");
    if (!wrap) return;
    if (!window.__KVC_DEBUG) {
      wrap.innerHTML = '<p class="muted">Compile with --features boom-dashboard/debug-tools to enable.</p>';
      return;
    }
    if (!document.getElementById("kvc-dfx-table")) {
      wrap.innerHTML = kvcDfxShellHtml();
      setupKvcFilters();
    }
    var tbody = document.getElementById("kvc-dfx-tbody");
    if (!tbody) return;
    try {
      var url = "/admin/logs?page=" + kvcPage + "&per_page=50";
      for (var k in kvcFilters) {
        url += "&" + encodeURIComponent(k) + "=" + encodeURIComponent(kvcFilters[k]);
      }
      var data = await api(url);
      var rows = data.logs || [];
      tbody.innerHTML = rows.length
        ? rows.map(renderKvcDfxRow).join("")
        : '<tr><td colspan="15" class="muted">No logs.</td></tr>';
      renderKvcPagination(data);
    } catch (e) {
      tbody.innerHTML = '<tr><td colspan="15" class="muted">Failed to load: ' + esc(String(e)) + '</td></tr>';
    }
  }

  function renderKvcPagination(data) {
    var el = document.getElementById("kvc-dfx-pagination");
    if (!el) return;
    if (!data || data.page == null) { el.innerHTML = ""; return; }
    el.innerHTML =
      '<button ' + (data.page <= 1 ? "disabled" : "") +
        ' onclick="window._loadKvcDfxPage(' + (data.page - 1) + ')">&lt;</button>' +
      '<span>' + t("common.page_only", { page: data.page }) + '</span>' +
      '<button ' + (!data.has_next ? "disabled" : "") +
        ' onclick="window._loadKvcDfxPage(' + (data.page + 1) + ')">&gt;</button>';
  }

  window._loadKvcDfxPage = function(p) { loadKvcDfx(p); };

  // ── Prompt Log Entry Viewer ──────────────────────────
  window._viewPromptLog = async function(requestId, keyHash, teamAlias) {
    showModal('<div style="text-align:center;padding:40px">' + t("common.loading") + '</div>', { wide: true });
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
        '<pre id="' + containerId + '-rawpre" style="display:none;max-height:72vh;overflow:auto;background:var(--surface2);color:var(--text);padding:16px;border-radius:8px;font-size:13px;line-height:1.5;white-space:pre-wrap;word-break:break-word;font-family:var(--mono)">' + esc(JSON.stringify(data, null, 2)) + '</pre>',
        { wide: true }
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
      showModal('<div style="padding:20px;color:var(--danger)">' + t("logs.failed_prompt", { message: esc(err.message) }) + '</div>', { wide: true });
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

  function initModelCombo(container, existingModels, allNames, isTeam = false) {
    const checked = new Set(existingModels || []);
    const isFullAccess = checked.size === 0 || checked.has("all-team-models");
    // Key form: "all-team-models" is litellm semantic (inherit team's models).
    // Team form: full-access is stored as empty array — show generic label.
    const fullAccessLabel = isTeam ? t("plans.team_full_access") : t("plans.full_access");

    // Build HTML
    container.innerHTML = `
      <div class="mcc-display">${isFullAccess ? fullAccessLabel : (existingModels || []).map((m) => esc(m)).join(", ") || t("plans.no_models")}</div>
      <div class="mcc-dropdown hidden">
        <label class="mcc-item mcc-item-all"><input type="checkbox" value="all-team-models" ${isFullAccess ? "checked" : ""}> ${fullAccessLabel}</label>
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
        display.textContent = fullAccessLabel;
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
  // (/dashboard/assets/vendor/:name). Match is substring + case-insensitive,
  // so "GLM-5.1" / "glm-xx" / "xxGlmxx" all hit GLM. Vendors with model
  // families that don't contain the brand name as substring keep explicit
  // aliases (e.g. cogvlm/cogview/thinking → GLM, abab/emotion → MiniMax,
  // qwq → Qwen, moonshot → Kimi).
  const VENDOR_PATTERNS = [
    { slug: "glm",      needles: ["glm", "chatglm", "cogvlm", "cogview", "thinking"] },
    { slug: "minimax",  needles: ["minimax", "abab", "emotion", "speech-0"] },
    { slug: "qwen",     needles: ["qwen", "qwq"] },
    { slug: "deepseek", needles: ["deepseek"] },
    { slug: "kimi",     needles: ["kimi", "moonshot"] },
    { slug: "mimo",     needles: ["mimo"] },
  ];
  function vendorOf(model) {
    const s = (model || "").toLowerCase();
    for (const v of VENDOR_PATTERNS) {
      for (const n of v.needles) {
        if (s.indexOf(n) !== -1) return v.slug;
      }
    }
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

  // Cost formatter — accepts Number or String (backend sends spend/total_cost
  // as Decimal.to_string() to preserve precision; we parse back to Number here
  // for display. Values >= ¥1 use 2dp; smaller positive values use 4dp so
  // sub-cent usage still shows meaningful digits.)
  function fmtCost(s) {
    const v = Number(s) || 0;
    if (v >= 1) return "¥" + v.toFixed(2);
    if (v > 0) return "¥" + v.toFixed(4);
    return "¥0";
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
