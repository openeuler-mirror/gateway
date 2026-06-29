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
    else if (section === "admin-aliases") loadAliases();
    else if (section === "admin-plans") loadPlans();
    else if (section === "admin-keys") { setupKeysSearch(); loadKeys(); }
    else if (section === "admin-assignments") loadAssignments();
    else if (section === "admin-teams") loadTeams();
    else if (section === "admin-logs") { setupLogsFilters(); loadLogs(); }
    else if (section === "admin-config") loadConfig();
    else if (section === "admin-debug") { loadAgentStats(); loadRebalanceMoves(); }
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
      "<th>" + t("stats.inflight.col.24h_reqs") + "</th><th>" + t("stats.inflight.col.avg_in") + "</th><th>" + t("stats.inflight.col.avg_out") + "</th><th>" + t("stats.inflight.col.avg_ttft") + "</th>" +
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

          var deployCell;
          if (d.deployment_id) {
            deployCell = '<span class="deploy-model">' + esc(d.model) + '</span>' +
              '<span class="deploy-sep">:</span>' +
              '<span class="deploy-id">' + esc(d.deployment_id) + '</span>';
          } else {
            deployCell = '<span class="deploy-model">' + esc(d.model) + '</span>';
          }

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
    if (!charts.length) { wrap.innerHTML = "<p>" + t("common.no_records") + "</p>"; return; }

    const windowNote = window
      ? `<div class="range-window-note">${esc(window.from)} → ${esc(window.to)} · bucket ${(window.bucket_secs / 60).toFixed(0)}min</div>`
      : "";
    var html = windowNote;
    charts.forEach(function (chart) {
      var events = chart.events || [];
      if (!events.length) return;
      var maxCount = Math.max(1, ...events.map(function (e) { return e.count; }));
      var label = chart.deployment_id === "_total"
        ? t("stats.rate.all_models")
        : esc(chart.model) + ":" + esc(chart.deployment_id);

      var bars = events.map(function (e, idx) {
        var pct = (e.count / maxCount) * 100;
        var showLabel = shouldShowLabel(events, idx);
        var lbl = formatBucketLabel(e.ts, window ? window.bucket_secs : 0);
        var title = lbl + ": " + t("stats.rebalance.req_count", { n: e.count });
        return '<div class="rb-bar-col" title="' + esc(title) + '">' +
          '<div class="rb-bar-value' + (e.count === 0 ? " rb-bar-value-zero" : "") + '">' + e.count + '</div>' +
          '<div class="rb-bar" style="height:' + Math.max(pct, 1) + '%;background:' + throughputBarColor(pct) + '"></div>' +
          '<div class="rb-bar-label' + (showLabel ? "" : " rb-label-hidden") + '">' + esc(lbl) + '</div>' +
          '</div>';
      }).join("");

      html += '<div style="margin-bottom:0.8rem">' +
        '<div style="font-size:0.85em;font-weight:600;margin-bottom:2px;color:var(--text2)">' + label + '</div>' +
        '<div class="rebalance-chart">' +
        '<div class="rb-y-axis"><span>' + maxCount + '</span><span>0</span></div>' +
        '<div class="rb-bars">' + bars + '</div>' +
        '</div></div>';
    });

    wrap.innerHTML = html || (windowNote + "<p>" + t("common.no_records") + "</p>");
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
    const rangeLabel = rangeState.agent.range === "custom" ? "custom" : rangeState.agent.range;

    const ratioPct = (summary.ratio * 100).toFixed(1);
    const inputRatioPct = (summary.input_token_ratio * 100).toFixed(1);
    const outputRatioPct = (summary.output_token_ratio * 100).toFixed(1);
    const windowNote = window
      ? `<div class="range-window-note">${esc(window.from)} → ${esc(window.to)} · bucket ${(window.bucket_secs / 60).toFixed(0)}min</div>`
      : "";

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
      wrap.innerHTML = summaryHtml + windowNote + '<p class="loading" style="margin-top:1rem">' + t("common.no_records") + '</p>';
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

    wrap.innerHTML = summaryHtml + windowNote +
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
  let chatModelsLoaded = false;
  let chatHistory = []; // [{role, content}]
  let chatQueueTimer = null;
  let chatAbort = null;
  let chatFirstTokenArrived = false;

  function getChatKey() { return sessionStorage.getItem(CHAT_KEY_STORAGE); }
  function getChatPrompt() { return localStorage.getItem(CHAT_PROMPT_STORAGE) || ""; }

  function initChatPage() {
    const sendBtn = document.getElementById("chat-send-btn");
    const input = document.getElementById("chat-input");
    const promptBtn = document.getElementById("chat-prompt-btn");
    // Wire up controls once.
    if (!sendBtn.dataset.wired) {
      sendBtn.dataset.wired = "1";
      sendBtn.addEventListener("click", sendChatMessage);
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && !e.shiftKey) {
          e.preventDefault();
          sendChatMessage();
        }
      });
      promptBtn.addEventListener("click", showSystemPromptModal);
    }
    if (!chatModelsLoaded) loadChatModels();
    loadChatQuota();
    renderChatHistory();
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
      chatModelsLoaded = true;
    } catch (err) {
      sel.innerHTML = `<option value="">${t("chat.models_failed", { error: esc(String(err.message || err)) })}</option>`;
    }
  }

  async function loadChatQuota() {
    const el = document.getElementById("chat-quota");
    if (!el) return;
    try {
      const [plan, usage] = await Promise.all([api("/user/plan"), api("/user/usage")]);
      const rpmWindow = (usage.windows || []).find((w) => w.window_secs === 60);
      const rpmUsed = rpmWindow ? rpmWindow.count : 0;
      const rpmLimit = plan.rpm_limit;
      const concUsed = usage.concurrency || 0;
      const concLimit = plan.concurrency_limit;
      const fmt = (u, l) => (l == null ? `${u}/∞` : `${u}/${l}`);
      el.textContent = `RPM ${fmt(rpmUsed, rpmLimit)} · ${t("chat.concurrency")} ${fmt(concUsed, concLimit)}`;
    } catch {
      el.textContent = "";
    }
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
    return `<div class="${aligned}"><div class="chat-bubble ${cls}">${esc(content)}</div></div>`;
  }

  async function sendChatMessage() {
    const input = document.getElementById("chat-input");
    const sendBtn = document.getElementById("chat-send-btn");
    const text = (input.value || "").trim();
    if (!text) return;
    const key = getChatKey();
    if (!key) { showLogin(); return; }
    const model = document.getElementById("chat-model").value;
    if (!model) { showToast(t("chat.no_model_selected")); return; }

    input.value = "";
    input.style.height = "";
    sendBtn.disabled = true;
    sendBtn.textContent = t("chat.sending");

    // Build messages: optional system prompt + history + new user message.
    const sysPrompt = getChatPrompt();
    const messages = [];
    if (sysPrompt) messages.push({ role: "system", content: sysPrompt });
    for (const m of chatHistory) messages.push(m);
    messages.push({ role: "user", content: text });
    chatHistory.push({ role: "user", content: text });
    chatHistory.push({ role: "assistant", content: "" });

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
        body: JSON.stringify({ model, messages, stream: true }),
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
                aBubble.innerHTML = "";
              }
              buf += delta;
              aBubble.textContent = buf;
              // Re-append cursor for live feel.
              const cur = document.createElement("span");
              cur.className = "chat-cursor";
              cur.textContent = "▌";
              aBubble.appendChild(cur);
              box.scrollTop = box.scrollHeight;
            }
          } catch {}
        }
      }
      // Stream ended — strip cursor, finalize bubble.
      aBubble.textContent = buf;
      chatHistory[chatHistory.length - 1].content = buf;
    } catch (err) {
      if (err.name === "AbortError") {
        // User navigated away or aborted — keep partial.
      } else {
        aBubble.classList.add("chat-bubble-error");
        aBubble.textContent = String(err.message || err);
        showToast(String(err.message || err));
        // Roll back the empty assistant placeholder we pushed.
        if (chatHistory.length && chatHistory[chatHistory.length - 1].content === "") {
          chatHistory.pop();
        }
      }
    } finally {
      stopQueuePolling();
      sendBtn.disabled = false;
      sendBtn.textContent = t("chat.send");
      loadChatQuota();
      chatAbort = null;
    }
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
          el.textContent = t("chat.queue_waiting", { ahead: r.ahead || 0, secs: (r.wait_time_secs || 0).toFixed(1) });
        } else {
          el.textContent = t("chat.queue_processing", { parallel: r.parallel_count || 0, secs: (r.processing_secs || 0).toFixed(1) });
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

  function showSystemPromptModal() {
    const current = getChatPrompt();
    showModal(`
      <h3>${t("chat.system_prompt_title")}</h3>
      <p class="modal-hint">${t("chat.system_prompt_hint")}</p>
      <textarea id="chat-prompt-input" rows="8" placeholder="${esc(t("chat.system_prompt_placeholder"))}">${esc(current)}</textarea>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()">${t("action.cancel")}</button>
        <button class="btn-danger" id="chat-prompt-clear">${t("chat.system_prompt_clear")}</button>
        <button class="btn-primary" id="chat-prompt-save">${t("chat.system_prompt_save")}</button>
      </div>
    `);
    document.getElementById("chat-prompt-save").addEventListener("click", () => {
      const v = document.getElementById("chat-prompt-input").value;
      if (v && v.trim()) localStorage.setItem(CHAT_PROMPT_STORAGE, v);
      else localStorage.removeItem(CHAT_PROMPT_STORAGE);
      hideModal();
      showToast(t("chat.system_prompt_saved"));
    });
    document.getElementById("chat-prompt-clear").addEventListener("click", () => {
      localStorage.removeItem(CHAT_PROMPT_STORAGE);
      document.getElementById("chat-prompt-input").value = "";
      showToast(t("chat.system_prompt_cleared"));
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
      <tr><th>${t("logs.col.time")}</th><th>${t("logs.col.ip")}</th><th>${t("logs.col.model")}</th><th>${t("logs.col.path")}</th><th>${t("logs.col.status")}</th><th>${t("logs.col.stream")}</th><th>${t("logs.col.input")}</th><th>${t("logs.col.output")}</th><th>${t("logs.col.duration")}</th><th>${t("logs.col.error")}</th></tr>
      ${logs.map((l) => `<tr>
        <td class="mono">${formatTimestamp(l.created_at)}</td>
        <td class="mono">${esc(l.client_ip || "-")}</td>
        <td class="mono">${esc(l.model)}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? t("common.yes") : t("common.no")}</td>
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
    if (plan.rpm_limit) limits.push(t("plan.limits.rpm", { n: plan.rpm_limit }));
    if (plan.window_limits && plan.window_limits.length > 0) {
      plan.window_limits.forEach(([l, w]) => limits.push(t("plan.limits.window", { n: l, duration: formatDuration(w) })));
    }
    el.innerHTML = `
      <p><strong>${esc(plan.plan_name)}</strong></p>
      <ul>${limits.map((l) => `<li>${esc(l)}</li>`).join("")}</ul>
    `;
  }

  function renderUsage(usage) {
    const el = document.getElementById("usage-info");
    let html = '<div class="usage-grid">';

    // Concurrency card
    const concLimit = usage.concurrency_limit;
    const concCount = usage.concurrency;
    if (concLimit != null) {
      html += `<div class="usage-limit-card">
        <div class="usage-limit-title">${t("req.concurrency")}</div>
        <div class="usage-limit-count">${concCount} / ${concLimit}</div>
        <div class="usage-limit-reset">${t("req.simultaneous")}</div>
      </div>`;
    } else {
      html += `<div class="usage-limit-card">
        <div class="usage-limit-title">${t("req.concurrency")}</div>
        <div class="usage-limit-count">${concCount}</div>
        <div class="usage-limit-reset">${t("common.unlimited")}</div>
      </div>`;
    }

    // Rate limit window cards
    if (usage.windows.length === 0) {
      html += '<div class="usage-limit-card"><div class="usage-limit-title">' + t("req.rate_limits") + '</div><div class="usage-limit-reset">' + t("req.no_active_windows") + '</div></div>';
    } else {
      usage.windows.forEach((w) => {
        const limit = w.limit;
        const isRpm = w.window_secs === 60;
        const label = isRpm ? "RPM" : t("plan.window_limit_label", { duration: formatDuration(w.window_secs) });
        const remaining = Math.max(0, w.window_secs - w.elapsed_secs);

        if (limit != null) {
          html += `<div class="usage-limit-card">
            <div class="usage-limit-title">${esc(label)}</div>
            <div class="usage-limit-count">${w.count} / ${limit}</div>
            <div class="usage-limit-reset">${t("req.resets_in", { time: formatCountdown(remaining) })}</div>
          </div>`;
        } else {
          html += `<div class="usage-limit-card">
            <div class="usage-limit-title">${esc(label)}</div>
            <div class="usage-limit-count">${w.count}</div>
            <div class="usage-limit-reset">${t("req.unlimited_resets", { time: formatCountdown(remaining) })}</div>
          </div>`;
        }
      });
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
      [t("keyinfo.rpm_limit"), info.rpm_limit || t("common.default")],
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
    wrap.innerHTML = `<table>
      <tr><th>${t("plans.col.name")}</th><th>${t("plans.col.concurrent")}</th><th>${t("plans.col.rpm")}</th><th>${t("plans.col.windows")}</th><th>${t("plans.col.actions")}</th></tr>
      ${plans.map((p) => `<tr>
        <td><strong>${esc(p.name)}</strong></td>
        <td>${p.concurrency_limit || "-"}</td>
        <td>${p.rpm_limit || "-"}</td>
        <td>${(p.window_limits || []).map(([l, w]) => `${l}/${formatDuration(w)}`).join(", ") || "-"}</td>
        <td>
          <button class="btn-small" onclick="window._editPlan('${esc(p.name)}')">${t("action.edit")}</button>
          <button class="btn-danger" onclick="window._deletePlan('${esc(p.name)}')">${t("action.delete")}</button>
        </td>
      </tr>`).join("")}
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
    wrap.innerHTML = `<table>
      <tr><th>${t("keys.col.token")}</th><th>${t("keys.col.alias")}</th><th>${t("keys.col.user")}</th><th>${t("keys.col.plan")}</th><th>${t("keys.col.usage")}</th><th>${t("keys.col.reset")}</th><th>${t("keys.col.spend")}</th><th>${t("keys.col.budget")}</th><th>${t("keys.col.status")}</th><th>${t("keys.col.actions")}</th></tr>
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
      const data = await api("/admin/models");
      renderModelsTable(data.models || []);
    } catch (err) {
      const wrap = document.getElementById("models-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("models.title"), message: esc(err.message) })}</p>`;
    }
  }

  function renderModelsTable(models) {
    const wrap = document.getElementById("models-table-wrap");
    if (models.length === 0) { wrap.innerHTML = "<p>" + t("models.empty") + "</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>${t("form.model.name")}</th><th>${t("models.col.litellm_model")}</th><th>${t("models.col.base_url")}</th><th>${t("models.col.ratio")}</th><th>${t("models.col.rpm")}</th><th>${t("models.col.timeout")}</th><th>${t("models.col.enabled")}</th><th>${t("models.col.source")}</th><th>${t("models.col.actions")}</th></tr>
      ${models.map((m) => {
        const isAutoDisabled = !m.enabled && m.auto_disabled;
        const enabledBadge = m.enabled
          ? '<span class="badge badge-active">' + t("common.yes") + '</span>'
          : isAutoDisabled
            ? '<span class="badge badge-blocked">' + t("common.no") + '</span><br><span style="color:var(--danger);font-size:0.8em">' + t("status.auto_disabled") + '</span>'
            : '<span class="badge badge-blocked">' + t("common.no") + '</span>';
        const warningRow = isAutoDisabled
          ? `<tr style="background:rgba(255,80,80,0.08)"><td colspan="9" style="padding:4px 8px;font-size:0.85em;color:var(--danger)">${t("models.fault_disabled")}</td></tr>`
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
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
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
  async function loadAliases() {
    try {
      const data = await api("/admin/aliases");
      renderAliasesTable(data.aliases || []);
    } catch (err) {
      const wrap = document.getElementById("aliases-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("aliases.title"), message: esc(err.message) })}</p>`;
    }
  }

  function renderAliasesTable(aliases) {
    const wrap = document.getElementById("aliases-table-wrap");
    if (aliases.length === 0) { wrap.innerHTML = "<p>" + t("aliases.empty") + "</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>${t("aliases.col.alias")}</th><th>${t("aliases.col.target")}</th><th>${t("form.alias.hidden")}</th><th>${t("models.col.source")}</th><th>${t("aliases.col.actions")}</th></tr>
      ${aliases.map((a) => `<tr>
        <td><strong>${esc(a.alias_name)}</strong></td>
        <td class="mono">${esc(a.target_model)}</td>
        <td>${a.hidden ? t("common.yes") : t("common.no")}</td>
        <td><span class="badge badge-plan">${esc(a.source || "-")}</span></td>
        <td>
          <button class="btn-small" onclick="window._editAlias('${esc(a.alias_name)}')">${t("action.edit")}</button>
          <button class="btn-danger" onclick="window._deleteAlias('${esc(a.alias_name)}')">${t("action.delete")}</button>
        </td>
      </tr>`).join("")}
    </table>`;
  }

  function showNewAliasModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.alias_name ? t("form.alias.title_edit") : t("form.alias.title_create")}</h3>
      <div class="form-group"><label>${t("form.alias.name")} * ${tip("The name clients will use in their request. E.g. 'gpt-4' → routes to 'gpt-4o'.")}</label><input id="m-alias-name" value="${esc(p.alias_name || "")}" ${p.alias_name ? "readonly" : ""}></div>
      <div class="form-group"><label>${t("form.alias.target")} * ${tip("The actual model name to route to. Must match an existing model deployment name.")}</label><input id="m-alias-target" value="${esc(p.target_model || "")}" required list="alias-target-list"><datalist id="alias-target-list"></datalist></div>
      <div class="form-group"><label>${t("form.alias.hidden")} ${tip("Hidden aliases work for routing but are not listed to users in model discovery endpoints.")}</label><select id="m-alias-hidden"><option value="false" ${!p.hidden ? "selected" : ""}>${t("common.no")}</option><option value="true" ${p.hidden ? "selected" : ""}>${t("common.yes")}</option></select></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
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
        loadAliases();
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
    loadAliases();
  };

  // ── Admin: Config ─────────────────────────────────────
  async function loadConfig() {
    try {
      const data = await api("/admin/config");
      renderConfigTable(data.config || {});
    } catch (err) {
      const wrap = document.getElementById("config-table-wrap");
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("config.title"), message: esc(err.message) })}</p>`;
    }
  }

  function renderConfigTable(config) {
    const wrap = document.getElementById("config-table-wrap");
    const keys = Object.keys(config);
    if (keys.length === 0) { wrap.innerHTML = "<p>" + t("config.empty") + "</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>${t("config.col.key")}</th><th>${t("config.col.value")}</th><th>${t("config.col.actions")}</th></tr>
      ${keys.map((k) => `<tr>
        <td><strong>${esc(k)}</strong></td>
        <td class="mono" style="max-width:400px;word-break:break-all;white-space:pre-wrap">${esc(JSON.stringify(config[k], null, 2))}</td>
        <td><button class="btn-small" onclick="window._editConfig('${esc(k)}')">${t("action.edit")}</button></td>
      </tr>`).join("")}
    </table>`;
  }

  function showNewConfigModal(prefill) {
    const p = prefill || {};
    showModal(`
      <h3>${p.key ? t("form.config.title_edit") : t("form.config.title_create")}</h3>
      <div class="form-group"><label>${t("form.config.key")} * ${tip("Configuration key name, e.g. 'general_settings' or a custom key.")}</label><input id="m-config-key" value="${esc(p.key || "")}" ${p.key ? "readonly" : ""}></div>
      <div class="form-group"><label>${t("form.config.value")} * ${tip("Configuration value as valid JSON. E.g. {\"store_model_in_db\": true}")}</label><textarea id="m-config-value" rows="6">${esc(p.value ? JSON.stringify(p.value, null, 2) : "")}</textarea></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-config-submit">${t("action.save")}</button>
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
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  }

  window._editConfig = async (key) => {
    try {
      const data = await api("/admin/config");
      const config = data.config || {};
      if (config[key] !== undefined) {
        showNewConfigModal({ key, value: config[key] });
      }
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
    } catch {}
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

  async function toggleTeamPromptLog(teamId, excluded) {
    try {
      await api("/admin/prompt-log/team", {
        method: "POST",
        body: JSON.stringify({ team_id: teamId, excluded: !excluded }),
      });
      loadTeams();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
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
          <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.close")}</button>
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
    const btnConfig = document.getElementById("btn-new-config");
    if (btnConfig) btnConfig.addEventListener("click", showNewConfigModal);
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
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
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
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-key-submit">${t("action.create")}</button>
      </div>
    `);
    // Populate model checkbox combo
    getModelNames().then((names) => {
      const container = document.getElementById("m-key-models-combo");
      if (container) initModelCombo(container, [], names);
    });
    // Populate team dropdown
    api("/admin/teams").then((data) => {
      const sel = document.getElementById("m-key-team");
      if (sel && data.teams) {
        data.teams.forEach((t) => {
          const o = document.createElement("option");
          o.value = t.team_id;
          o.textContent = t.team_alias || t.team_id;
          sel.appendChild(o);
        });
      }
    }).catch(() => {});
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
            <button class="btn-secondary" style="width:auto" onclick="window._copyText(this,'${esc(rawKey)}')">${t("action.copy")}</button>
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
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
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
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
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
      if (wrap) wrap.innerHTML = `<p class="error-msg">${t("common.failed_to_load", { what: t("teams.title"), message: esc(err.message) })}</p>`;
    }
  }

  function renderTeamsTable(teams) {
    const wrap = document.getElementById("teams-table-wrap");
    if (teams.length === 0) { wrap.innerHTML = "<p>" + t("teams.empty") + "</p>"; return; }
    wrap.innerHTML = `<table>
      <tr><th>${t("teams.col.alias")}</th><th>${t("teams.col.team_id")}</th><th>${t("teams.col.models")}</th><th>${t("teams.col.keys_count")}</th><th>${t("teams.col.requests")}</th><th>${t("teams.col.input_tokens")}</th><th>${t("teams.col.output_tokens")}</th><th>${t("teams.col.total_tokens")}</th><th>${t("teams.col.prompt_log")}</th><th>${t("teams.col.actions")}</th></tr>
      ${teams.map((tm) => {
        const isExcluded = (window._promptLogExcludedTeams || []).includes(tm.team_id);
        const logBtnClass = isExcluded ? "btn-secondary" : "btn-primary";
        const logBtnText = isExcluded ? "OFF" : "ON";
        return `<tr>
        <td>${esc(tm.team_alias || "-")}</td>
        <td class="mono" title="${esc(tm.team_id)}">${esc((tm.team_id || "").substring(0, 12))}</td>
        <td class="mono">${esc(formatTeamModels(tm.models))}</td>
        <td>${tm.key_count}</td>
        <td>${formatNumber(tm.request_count)}</td>
        <td>${formatNumber(tm.total_input_tokens || 0)}</td>
        <td>${formatNumber(tm.total_output_tokens || 0)}</td>
        <td>${formatNumber((tm.total_input_tokens || 0) + (tm.total_output_tokens || 0))}</td>
        <td><button class="${logBtnClass} btn-sm" onclick='window._toggleTeamPromptLog(${JSON.stringify(tm.team_id)}, ${isExcluded})'>${logBtnText}</button></td>
        <td>
          <button class="btn-secondary btn-sm" onclick='window._editTeam(${JSON.stringify(tm.team_id)})'>${t("action.edit")}</button>
          <button class="btn-danger btn-sm" onclick='window._deleteTeam(${JSON.stringify(tm.team_id)}, ${tm.key_count})'>${t("action.delete")}</button>
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
      <h3>${p.team_id ? t("form.team.title_edit") : t("form.team.title_create")}</h3>
      <div class="form-group"><label>${t("form.team.id")} ${tip("Unique identifier for this team. Cannot be changed after creation.")}</label><input id="m-team-id" value="${esc(p.team_id || "")}" ${p.team_id ? "readonly" : ""} required></div>
      <div class="form-group"><label>${t("form.team.alias")} ${tip("Display name for this team. Can be non-unique.")}</label><input id="m-team-alias" value="${esc(p.team_alias || "")}"></div>
      <div class="form-group"><label>${t("form.team.models")} ${tip("Select model access for this team. Check 'all-team-models' for full access to all current and future models, or pick specific models.")}</label><div class="model-check-combo" id="m-team-models-combo"></div></div>
      <div class="modal-actions">
        <button class="btn-secondary" onclick="hideModal()" style="width:auto">${t("action.cancel")}</button>
        <button class="btn-primary" id="m-team-submit">${p.team_id ? t("action.update") : t("action.create")}</button>
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
      } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
    });
  };

  window._editTeam = async (teamId) => {
    try {
      const data = await api("/admin/teams");
      const tm = (data.teams || []).find((x) => x.team_id === teamId);
      if (!tm) return;
      // Fetch full team record with models from DB.
      // list_teams doesn't return models, so we pass what we have.
      showCreateTeamModal(tm);
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
      loadTeams();
    } catch (err) { alert(t("common.error_prefix", { message: err.message })); }
  };


  window._toggleTeamPromptLog = async (teamId, isExcluded) => {
    try {
      await api('/admin/prompt-log/team', {
        method: 'POST',
        body: JSON.stringify({ team_id: teamId, excluded: !isExcluded }),
      });
      loadTeams();
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
        return `<tr>
        <td>${tsCell}</td>
        <td>${ipCell}</td>
        <td>${esc(l.team_alias || l.team_id || "-")}</td>
        <td>${esc(l.key_alias || l.key_name || "-")}</td>
        <td>${modelCell}</td>
        <td class="mono">${esc(l.api_path)}</td>
        <td>${l.status_code >= 400 ? '<span style="color:var(--danger)">' + l.status_code + '</span>' : l.status_code}</td>
        <td>${l.is_stream ? t("common.yes") : t("common.no")}</td>
        <td>${l.input_tokens != null ? formatNumber(l.input_tokens) : "-"}</td>
        <td>${l.output_tokens != null ? formatNumber(l.output_tokens) : "-"}</td>
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

  function formatDuration(secs) {
    if (secs < 60) return secs + "s";
    if (secs < 3600) return (secs / 60) + "min";
    if (secs < 86400) return (secs / 3600) + "h";
    return (secs / 86400) + "d";
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
