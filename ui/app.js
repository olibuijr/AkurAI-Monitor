const $ = (s) => document.querySelector(s);
const app = $('#app');

// View + live state
let rangeHours = 24;
let currentView = 'dashboard';
let currentSpecs = [];
let renderedBadgeKey = '';
let chartTimer = null;
let liveLogs = [];
const LOG_CAP = 500;
let connState = 'connecting'; // 'live' | 'polling' | 'connecting'

async function api(path) {
  const res = await fetch(path);
  return res.json();
}

function ts(epoch) {
  return new Date(epoch * 1000).toLocaleString();
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#60a5fa';
}

function severity(name, value) {
  if (name.includes('used_pct') || name === 'cpu.usage' || name === 'mem.used_pct') {
    if (value >= 90) return 'crit';
    if (value >= 75) return 'warn';
    return 'ok';
  }
  return 'ok';
}

function formatValue(name, value) {
  if (name.includes('_kb')) return (value / 1024 / 1024).toFixed(1) + ' GB';
  if (name.includes('_gb')) return value.toFixed(1) + ' GB';
  if (name.includes('_pct') || name === 'cpu.usage') return value.toFixed(1) + '%';
  if (name.includes('bytes')) return formatBytes(value) + '/s';
  if (name === 'uptime.seconds') return formatDuration(value);
  if (name.startsWith('load.')) return value.toFixed(2);
  return value.toFixed(1);
}

function formatBytes(b) {
  if (b < 1024) return b.toFixed(0) + ' B';
  if (b < 1048576) return (b / 1024).toFixed(1) + ' KB';
  if (b < 1073741824) return (b / 1048576).toFixed(1) + ' MB';
  return (b / 1073741824).toFixed(1) + ' GB';
}

function formatDuration(secs) {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  return d > 0 ? `${d}d ${h}h` : `${h}h`;
}

function metricLabel(name) {
  const labels = {
    'cpu.usage': 'CPU Usage',
    'mem.used_pct': 'Memory',
    'mem.total_kb': 'Total RAM',
    'mem.used_kb': 'Used RAM',
    'mem.available_kb': 'Available RAM',
    'load.1m': 'Load 1m',
    'load.5m': 'Load 5m',
    'load.15m': 'Load 15m',
    'uptime.seconds': 'Uptime',
  };
  if (labels[name]) return labels[name];
  if (name.startsWith('disk.')) return name.replace('disk.', 'Disk ').replace('.used_pct', ' Usage').replace('.total_gb', ' Total').replace('.used_gb', ' Used');
  if (name.startsWith('net.')) return name.replace('net.', 'Net ').replace('.rx_bytes', ' RX').replace('.tx_bytes', ' TX');
  return name;
}

function connLabel() {
  if (connState === 'live') return '<span class="live-dot live"></span>Live';
  if (connState === 'polling') return '<span class="live-dot"></span>Polling (10s)';
  return '<span class="live-dot"></span>Connecting…';
}

function updateConnUI() {
  document.querySelectorAll('.conn').forEach((el) => { el.innerHTML = connLabel(); });
}

// ── Canvas line chart ──────────────────────────────────────────────
function lineChart(canvas, series, opts = {}) {
  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth || 300;
  const H = canvas.clientHeight || 140;
  canvas.width = Math.round(W * dpr);
  canvas.height = Math.round(H * dpr);
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, W, H);

  const padL = 46, padR = 12, padT = 10, padB = 22;
  const plotW = W - padL - padR;
  const plotH = H - padT - padB;

  const allTs = [], allV = [];
  for (const s of series) for (const p of s.points) { allTs.push(p.ts); allV.push(p.value); }

  ctx.font = '10px -apple-system, sans-serif';
  if (allV.length === 0) {
    ctx.fillStyle = cssVar('--text-dim');
    ctx.fillText('no data in range', padL, padT + plotH / 2);
    return;
  }

  const minT = Math.min(...allTs), maxT = Math.max(...allTs);
  let minV = opts.min != null ? opts.min : Math.min(...allV);
  let maxV = opts.max != null ? opts.max : Math.max(...allV);
  if (opts.max == null) maxV = maxV * 1.1 + 1e-9;
  if (minV === maxV) maxV = minV + 1;

  const x = (t) => padL + (maxT === minT ? plotW : ((t - minT) / (maxT - minT)) * plotW);
  const y = (v) => padT + plotH - ((v - minV) / (maxV - minV)) * plotH;

  ctx.strokeStyle = cssVar('--border');
  ctx.fillStyle = cssVar('--text-dim');
  ctx.lineWidth = 1;
  const ticks = 4;
  for (let i = 0; i <= ticks; i++) {
    const v = minV + (maxV - minV) * (i / ticks);
    const yy = Math.round(y(v)) + 0.5;
    ctx.beginPath();
    ctx.moveTo(padL, yy);
    ctx.lineTo(W - padR, yy);
    ctx.stroke();
    ctx.fillText(opts.fmt ? opts.fmt(v) : v.toFixed(0), 4, yy + 3);
  }

  const fmtTime = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  ctx.fillText(fmtTime(minT), padL, H - 6);
  const endLabel = fmtTime(maxT);
  ctx.fillText(endLabel, W - padR - ctx.measureText(endLabel).width, H - 6);

  for (const s of series) {
    if (s.points.length === 0) continue;
    const color = s.color.startsWith('--') ? cssVar(s.color) : s.color;

    ctx.beginPath();
    ctx.moveTo(x(s.points[0].ts), padT + plotH);
    for (const p of s.points) ctx.lineTo(x(p.ts), y(p.value));
    ctx.lineTo(x(s.points[s.points.length - 1].ts), padT + plotH);
    ctx.closePath();
    ctx.fillStyle = color + '22';
    ctx.fill();

    ctx.beginPath();
    s.points.forEach((p, i) => {
      const xx = x(p.ts), yy = y(p.value);
      if (i === 0) ctx.moveTo(xx, yy); else ctx.lineTo(xx, yy);
    });
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.6;
    ctx.lineJoin = 'round';
    ctx.stroke();
  }
}

// ── Dashboard ──────────────────────────────────────────────────────
const cardOrder = ['cpu.usage', 'mem.used_pct', 'load.1m', 'uptime.seconds'];
const RANGES = [[1, '1h'], [6, '6h'], [24, '24h'], [168, '7d']];

function sortMetrics(metrics) {
  return [...metrics].sort((a, b) => {
    const ai = cardOrder.indexOf(a.name);
    const bi = cardOrder.indexOf(b.name);
    if (ai >= 0 && bi >= 0) return ai - bi;
    if (ai >= 0) return -1;
    if (bi >= 0) return 1;
    return a.name.localeCompare(b.name);
  });
}

function chartSpecs(names) {
  const specs = [
    { title: 'CPU Usage', series: [{ name: 'cpu.usage', color: '--blue' }], min: 0, max: 100, fmt: (v) => v.toFixed(0) + '%' },
    { title: 'Memory', series: [{ name: 'mem.used_pct', color: '--green' }], min: 0, max: 100, fmt: (v) => v.toFixed(0) + '%' },
    {
      title: 'Load Average',
      series: [
        { name: 'load.1m', color: '--blue' },
        { name: 'load.5m', color: '--yellow' },
        { name: 'load.15m', color: '--text-dim' },
      ],
      min: 0,
      fmt: (v) => v.toFixed(1),
    },
  ];

  for (const n of names.filter((n) => n.startsWith('disk.') && n.endsWith('.used_pct')).sort()) {
    specs.push({ title: metricLabel(n), series: [{ name: n, color: '--yellow' }], min: 0, max: 100, fmt: (v) => v.toFixed(0) + '%' });
  }

  const ifaces = [...new Set(names.filter((n) => n.startsWith('net.')).map((n) => n.split('.')[1]))].sort();
  for (const iface of ifaces) {
    specs.push({
      title: `Network ${iface}`,
      series: [
        { name: `net.${iface}.rx_bytes`, color: '--green', label: 'RX' },
        { name: `net.${iface}.tx_bytes`, color: '--blue', label: 'TX' },
      ],
      min: 0,
      fmt: (v) => formatBytes(v),
    });
  }

  return specs;
}

function badgeKey(metrics) {
  return metrics.map((m) => m.name).sort().join('|');
}

function updateBadges(metrics) {
  if (currentView !== 'dashboard') return;
  if (badgeKey(metrics) !== renderedBadgeKey) {
    renderDashboard(metrics); // metric set changed — rebuild
    return;
  }
  for (const m of metrics) {
    const el = app.querySelector(`.value[data-badge="${m.name}"]`);
    if (el) {
      el.textContent = formatValue(m.name, m.value);
      el.className = `value ${severity(m.name, m.value)}`;
      el.dataset.badge = m.name;
    }
  }
}

async function refreshAlerts() {
  const el = $('#alerts-section');
  if (!el) return;
  const data = await api('/api/alerts?hours=24');
  const active = (data.alerts || []).filter((a) => !a.resolved_at);
  if (active.length === 0) { el.innerHTML = ''; return; }
  let html = '<h2>Active Alerts</h2><table><tr><th>Rule</th><th>Metric</th><th>Value</th><th>Since</th></tr>';
  for (const a of active) {
    html += `<tr class="alert-active"><td>${a.rule_name}</td><td>${a.metric_name}</td><td>${a.value.toFixed(1)}</td><td>${ts(a.triggered_at)}</td></tr>`;
  }
  html += '</table>';
  el.innerHTML = html;
}

async function drawCharts() {
  const wanted = [...new Set(currentSpecs.flatMap((s) => s.series.map((x) => x.name)))];
  const seriesData = {};
  await Promise.all(
    wanted.map(async (name) => {
      const d = await api(`/api/metrics?name=${encodeURIComponent(name)}&hours=${rangeHours}`);
      seriesData[name] = (d.metrics || []).map((m) => ({ ts: m.ts, value: m.value }));
    })
  );
  currentSpecs.forEach((spec, i) => {
    const canvas = document.getElementById(`chart-${i}`);
    if (!canvas) return;
    const series = spec.series.map((s) => ({ color: s.color, points: seriesData[s.name] || [] }));
    lineChart(canvas, series, { min: spec.min, max: spec.max, fmt: spec.fmt });
  });
}

async function renderDashboard(seedMetrics) {
  let metrics = seedMetrics;
  if (!metrics) {
    const status = await api('/api/status');
    metrics = status.metrics || [];
  }

  if (metrics.length === 0) {
    app.innerHTML = '<div class="empty">No metrics yet. Waiting for first collection cycle...</div>';
    renderedBadgeKey = '';
    return;
  }

  const sorted = sortMetrics(metrics);
  renderedBadgeKey = badgeKey(metrics);
  currentSpecs = chartSpecs(metrics.map((m) => m.name));

  let html = `<div class="refresh conn">${connLabel()}</div>`;
  html += '<h2>Current Status</h2><div class="grid">';
  for (const m of sorted) {
    const sev = severity(m.name, m.value);
    html += `<div class="card"><div class="label">${metricLabel(m.name)}</div><div class="value ${sev}" data-badge="${m.name}">${formatValue(m.name, m.value)}</div></div>`;
  }
  html += '</div>';

  html += '<div class="trend-head"><h2>Trends</h2><div class="ranges">';
  for (const [h, label] of RANGES) {
    html += `<button class="range${h === rangeHours ? ' active' : ''}" data-hours="${h}">${label}</button>`;
  }
  html += '</div></div><div class="charts">';
  currentSpecs.forEach((spec, i) => {
    const legend = spec.series.filter((s) => s.label).map((s) => `<span class="lg"><i style="background:${cssVar(s.color)}"></i>${s.label}</span>`).join('');
    html += `<div class="chart-card"><div class="chart-title">${spec.title}${legend ? `<span class="legend">${legend}</span>` : ''}</div><canvas id="chart-${i}"></canvas></div>`;
  });
  html += '</div><div id="alerts-section"></div>';

  app.innerHTML = html;

  app.querySelectorAll('button.range').forEach((btn) => {
    btn.addEventListener('click', () => {
      rangeHours = Number(btn.dataset.hours);
      renderDashboard();
    });
  });

  drawCharts();
  refreshAlerts();

  // periodic refresh of history-backed charts + alerts (badges update live via SSE)
  clearInterval(chartTimer);
  chartTimer = setInterval(() => { drawCharts(); refreshAlerts(); }, 60000);
}

// ── Logs ───────────────────────────────────────────────────────────
function renderLogsTable() {
  let html = `<div class="refresh conn">${connLabel()}</div><h2>Live Logs</h2>`;
  if (liveLogs.length === 0) {
    html += '<div class="empty">No log entries</div>';
  } else {
    html += '<table><tr><th>Time</th><th>Source</th><th>Line</th></tr>';
    for (const l of liveLogs) {
      const src = l.source.split('/').pop();
      html += `<tr><td style="white-space:nowrap">${ts(l.ts)}</td><td>${src}</td><td>${escapeHtml(l.line)}</td></tr>`;
    }
    html += '</table>';
  }
  app.innerHTML = html;
}

async function renderLogs() {
  const data = await api('/api/logs?hours=1');
  liveLogs = data.logs || []; // newest first
  renderLogsTable();
}

function handleLogBatch(data) {
  if (currentView !== 'logs') return;
  const incoming = data.logs || [];
  if (incoming.length === 0) return;
  // incoming is chronological (oldest first); prepend newest-first
  liveLogs = incoming.slice().reverse().concat(liveLogs).slice(0, LOG_CAP);
  renderLogsTable();
}

// ── Alerts (full history view) ─────────────────────────────────────
async function renderAlerts() {
  const data = await api('/api/alerts?hours=72');
  const alerts = data.alerts || [];

  let html = `<h2>Alerts (72 hours) &mdash; ${data.active || 0} active</h2>`;
  if (alerts.length === 0) {
    html += '<div class="empty">No alerts</div>';
  } else {
    html += '<table><tr><th>Status</th><th>Rule</th><th>Metric</th><th>Value</th><th>Triggered</th><th>Resolved</th></tr>';
    for (const a of alerts) {
      const cls = a.resolved_at ? 'alert-resolved' : 'alert-active';
      const status = a.resolved_at ? 'Resolved' : 'Active';
      html += `<tr class="${cls}"><td>${status}</td><td>${a.rule_name}</td><td>${a.metric_name}</td><td>${a.value.toFixed(1)}</td><td>${ts(a.triggered_at)}</td><td>${a.resolved_at ? ts(a.resolved_at) : '-'}</td></tr>`;
    }
    html += '</table>';
  }

  app.innerHTML = html;
}

function escapeHtml(s) {
  const div = document.createElement('div');
  div.textContent = s;
  return div.innerHTML;
}

// ── Live connection (SSE with polling fallback) ────────────────────
function handleStatus(data) {
  updateBadges(data.metrics || []);
}

let pollTimer = null;
function startPolling() {
  if (pollTimer) return;
  connState = 'polling';
  updateConnUI();
  pollTimer = setInterval(async () => {
    try {
      const s = await api('/api/status');
      handleStatus(s);
      if (currentView === 'logs') await renderLogs();
    } catch { /* ignore */ }
  }, 10000);
}

function startLive() {
  if (typeof EventSource === 'undefined') { startPolling(); return; }
  let opened = false;
  const es = new EventSource('/api/stream');
  es.onopen = () => { opened = true; connState = 'live'; updateConnUI(); };
  es.addEventListener('status', (e) => { try { handleStatus(JSON.parse(e.data)); } catch {} });
  es.addEventListener('log', (e) => { try { handleLogBatch(JSON.parse(e.data)); } catch {} });
  es.onerror = () => {
    if (!opened) { es.close(); startPolling(); } // stream blocked → fall back
    else { connState = 'connecting'; updateConnUI(); } // transient: let it auto-reconnect
  };
}

// ── Routing ────────────────────────────────────────────────────────
function route() {
  const hash = location.hash || '#/';
  document.querySelectorAll('nav a').forEach((a) => {
    a.classList.toggle('active', a.getAttribute('href') === hash);
  });

  clearInterval(chartTimer);

  if (hash.startsWith('#/logs')) {
    currentView = 'logs';
    renderLogs();
  } else if (hash.startsWith('#/alerts')) {
    currentView = 'alerts';
    renderAlerts();
  } else {
    currentView = 'dashboard';
    renderDashboard();
  }
}

window.addEventListener('hashchange', route);
let resizeTimer;
window.addEventListener('resize', () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => { if (currentView === 'dashboard') drawCharts(); }, 200);
});

route();
startLive();
