'use strict';

// ---------------------------------------------------------------------------
// API helper
// ---------------------------------------------------------------------------

const TOKEN_KEY = 'cais_token';
let token = localStorage.getItem(TOKEN_KEY) || '';

const REQUEST_TIMEOUT_MS = 30000;

async function api(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (options.body !== undefined && !headers['Content-Type']) {
    headers['Content-Type'] = 'application/json';
  }
  if (token) {
    headers['Authorization'] = 'Bearer ' + token;
  }

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  const external = options.signal || null;
  const forwardAbort = () => controller.abort();
  if (external) {
    if (external.aborted) {
      controller.abort();
    } else {
      external.addEventListener('abort', forwardAbort);
    }
  }
  let res;
  try {
    res = await fetch(path, { ...options, headers, signal: controller.signal });
  } catch (e) {
    if (e && e.name === 'AbortError') {
      if (external && external.aborted) {
        throw new Error('__cancelled__');
      }
      throw new Error('Tempo esgotado aguardando o servidor.');
    }
    throw e;
  } finally {
    clearTimeout(timeout);
    if (external) {
      external.removeEventListener('abort', forwardAbort);
    }
  }
  if (res.status === 401) {
    lockLocal();
    throw new Error('Sessão expirada. Desbloqueie novamente.');
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(data.error || ('Erro ' + res.status));
  }
  return data;
}

const apiGet = (p) => api(p);
const apiPost = (p, body) => api(p, { method: 'POST', body: JSON.stringify(body) });
const apiPut = (p, body) => api(p, { method: 'PUT', body: JSON.stringify(body) });
const apiDelete = (p) => api(p, { method: 'DELETE' });

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let dashboard = { instances: [], totals: { databases: 0, extra_users: 0 } };

// Instance list pagination/filter/expand state.
const INSTANCES_PER_PAGE = 10;
let instancePage = 1;
let expandedInstance = null;

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

const $ = (sel) => document.querySelector(sel);
const el = (tag, attrs = {}, children = []) => {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'text') {
      node.textContent = v;
    } else if (k === 'html') {
      node.innerHTML = v;
    } else if (k.startsWith('on') && typeof v === 'function') {
      node.addEventListener(k.slice(2), v);
    } else if (v !== null && v !== undefined) {
      node.setAttribute(k, v);
    }
  }
  for (const child of [].concat(children)) {
    if (child) {
      node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
    }
  }
  return node;
};

function escapeHtml(value) {
  const div = document.createElement('div');
  div.textContent = value == null ? '' : String(value);
  return div.innerHTML;
}

function showView(id) {
  document.querySelectorAll('.view').forEach((v) => v.classList.add('hidden'));
  const view = document.getElementById(id);
  if (view) {
    view.classList.remove('hidden');
  }
}

function setStatus(msg) {
  const bars = document.querySelectorAll('#status-bar');
  bars.forEach((b) => { b.textContent = msg || ''; });
}

function showLoading(msg = 'Carregando...') {
  $('#loading-text').textContent = msg;
  $('#loading-overlay').classList.remove('hidden');
}

function hideLoading() {
  $('#loading-overlay').classList.add('hidden');
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    setStatus('Copiado para a área de transferência.');
  } catch {
    window.prompt('Copie manualmente:', text);
  }
}

// ---------------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------------

// Poll timers of active operations. Closing the modal must stop them, or they
// keep hitting the server every 800ms forever.
const opPollTimers = new Set();

// AbortController of the health check currently shown in the modal. Closing
// the modal (X, overlay, Esc or the Cancel button) aborts it.
let activeHealthCheck = null;

function openModal(html) {
  $('#modal-body').innerHTML = html;
  $('#modal-overlay').classList.remove('hidden');
  const firstInput = $('#modal-body input, #modal-body select');
  if (firstInput) {
    firstInput.focus();
  }
}

function closeModal() {
  opPollTimers.forEach((t) => clearInterval(t));
  opPollTimers.clear();
  if (activeHealthCheck) {
    activeHealthCheck.abort();
    activeHealthCheck = null;
  }
  $('#modal-overlay').classList.add('hidden');
  $('#modal-body').innerHTML = '';
}

function confirmModal(title, message, confirmLabel = 'Confirmar') {
  return new Promise((resolve) => {
    openModal(`
      <h2>${escapeHtml(title)}</h2>
      <p class="hint">${escapeHtml(message)}</p>
      <div class="form-actions">
        <button id="cm-cancel" class="ghost">Cancelar</button>
        <button id="cm-ok" class="danger">${escapeHtml(confirmLabel)}</button>
      </div>
    `);
    $('#cm-ok').addEventListener('click', () => { closeModal(); resolve(true); });
    $('#cm-cancel').addEventListener('click', () => { closeModal(); resolve(false); });
  });
}

// Destructive-action confirmation with a safety countdown: the confirm button
// stays disabled until `seconds` have passed, so accidental clicks cannot
// delete or rotate credentials immediately.
function confirmModalCountdown(title, message, confirmLabel = 'Confirmar', seconds = 5) {
  return new Promise((resolve) => {
    openModal(`
      <h2>${escapeHtml(title)}</h2>
      <p class="hint">${escapeHtml(message)}</p>
      <p id="cm-countdown" class="countdown"></p>
      <div class="form-actions">
        <button id="cm-cancel" class="ghost">Cancelar</button>
        <button id="cm-ok" class="danger" disabled>${escapeHtml(confirmLabel)}</button>
      </div>
    `);
    const okButton = $('#cm-ok');
    const countdown = $('#cm-countdown');
    let remaining = seconds;
    countdown.textContent = `Confirmação em ${remaining}s`;

    const timer = setInterval(() => {
      if (!okButton.isConnected) {
        clearInterval(timer);
        resolve(false);
        return;
      }
      remaining -= 1;
      if (remaining <= 0) {
        clearInterval(timer);
        countdown.textContent = '';
        okButton.disabled = false;
        okButton.textContent = confirmLabel;
        return;
      }
      countdown.textContent = `Confirmação em ${remaining}s`;
    }, 1000);

    okButton.addEventListener('click', () => {
      clearInterval(timer);
      closeModal();
      resolve(true);
    });
    $('#cm-cancel').addEventListener('click', () => {
      clearInterval(timer);
      closeModal();
      resolve(false);
    });
  });
}

// ---------------------------------------------------------------------------
// Unlock / first run
// ---------------------------------------------------------------------------

const WORKSPACE_KEY = 'cais_workspace';
let workspacesCache = [];
let selectedWorkspace = null;
let newWorkspaceMode = false;

async function boot() {
  try {
    await apiGet('/api/status');
  } catch (e) {
    setStatus('Falha ao consultar o servidor: ' + e.message);
    return;
  }

  if (token) {
    try {
      await loadDashboard();
      return;
    } catch {
      // Token inválido ou servidor reiniciado — cai no unlock abaixo.
    }
  }
  await showUnlock();
}

async function showUnlock() {
  showView('view-unlock');
  $('#unlock-submit').textContent = 'Desbloquear';
  $('#field-password').value = '';
  $('#field-confirm').value = '';
  $('#field-new-workspace').value = '';
  $('#unlock-error').classList.add('hidden');
  exitNewWorkspaceMode();
  try {
    await refreshWorkspaces();
  } catch {
    workspacesCache = [];
    syncUnlockMode();
  }
}

async function refreshWorkspaces() {
  workspacesCache = await apiGet('/api/workspaces');
  const stored = localStorage.getItem(WORKSPACE_KEY);
  const names = workspacesCache.map((ws) => ws.name);
  if (!names.includes(selectedWorkspace)) {
    selectedWorkspace = names.includes(stored) ? stored : names[0] || null;
  }
  renderWorkspaceList();
  if (!names.length) {
    enterNewWorkspaceMode();
  }
  syncUnlockMode();
}

function renderWorkspaceList() {
  const list = $('#workspace-list');
  list.innerHTML = '';
  const query = ($('#field-workspace-filter').value || '').trim().toLowerCase();
  let shown = 0;
  for (const ws of workspacesCache) {
    if (query && !ws.name.toLowerCase().includes(query)) {
      continue;
    }
    shown += 1;
    list.appendChild(el('button', {
      type: 'button',
      class: 'workspace-item' + (ws.name === selectedWorkspace ? ' selected' : ''),
      onclick: () => selectWorkspace(ws.name),
    }, [
      el('span', { class: 'workspace-name', text: ws.name }),
      ws.initialized ? null : el('span', { class: 'tag', text: 'sem senha' }),
    ]));
  }
  if (!shown) {
    list.appendChild(el('p', { class: 'hint', text: 'Nenhum workspace.' }));
  }
}

function selectWorkspace(name) {
  selectedWorkspace = name;
  renderWorkspaceList();
  syncUnlockMode();
  $('#field-password').focus();
}

function selectedWorkspaceInfo() {
  return workspacesCache.find((ws) => ws.name === selectedWorkspace) || null;
}

function syncUnlockMode() {
  const info = selectedWorkspaceInfo();
  const needsInit = newWorkspaceMode || !info || !info.initialized;
  $('#field-confirm-wrap').classList.toggle('hidden', !needsInit);
  $('#workspace-select-wrap').classList.toggle('hidden', newWorkspaceMode);
  $('#btn-new-workspace').classList.toggle('hidden', newWorkspaceMode);
  $('#btn-cancel-new-workspace').classList.toggle('hidden', !newWorkspaceMode);
  const submitLabel = needsInit ? 'Criar workspace' : 'Desbloquear';
  $('#unlock-submit').textContent = submitLabel;
  $('#unlock-subtitle').textContent = newWorkspaceMode
    ? 'Novo workspace: escolha um nome e crie a senha mestra dele.'
    : needsInit
      ? `Workspace ${info ? info.name : ''} ainda não tem senha. Defina a senha mestra dele.`
      : 'Digite a senha mestra para desbloquear o cofre.';
}

function enterNewWorkspaceMode() {
  newWorkspaceMode = true;
  $('#field-new-workspace-wrap').classList.remove('hidden');
  syncUnlockMode();
  $('#field-new-workspace').focus();
}

function exitNewWorkspaceMode() {
  newWorkspaceMode = false;
  $('#field-new-workspace-wrap').classList.add('hidden');
  syncUnlockMode();
}

$('#btn-new-workspace').addEventListener('click', () => enterNewWorkspaceMode());
$('#btn-cancel-new-workspace').addEventListener('click', () => exitNewWorkspaceMode());
$('#field-workspace-filter').addEventListener('input', renderWorkspaceList);

$('#btn-remove-workspace').addEventListener('click', async () => {
  const name = selectedWorkspace;
  if (!name) {
    return;
  }
  const ok = await confirmModalCountdown(
    'Remover workspace',
    `Remover o workspace ${name}? Todas as instâncias e conexões salvas nele serão apagadas definitivamente. Nada nos servidores PostgreSQL é afetado.`,
    'Remover',
  );
  if (!ok) {
    return;
  }
  try {
    await apiDelete('/api/workspaces/' + encodeURIComponent(name));
    if (localStorage.getItem(WORKSPACE_KEY) === name) {
      localStorage.removeItem(WORKSPACE_KEY);
    }
    setStatus(`Workspace '${name}' removido.`);
    await refreshWorkspaces();
  } catch (e) {
    setStatus(e.message);
  }
});

$('#unlock-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const password = $('#field-password').value;
  const workspace = newWorkspaceMode
    ? $('#field-new-workspace').value.trim()
    : selectedWorkspace;
  const info = workspacesCache.find((ws) => ws.name === workspace);
  const needsInit = newWorkspaceMode || !info || !info.initialized;
  const errEl = $('#unlock-error');
  const submitBtn = $('#unlock-submit');
  errEl.classList.add('hidden');
  submitBtn.disabled = true;
  const originalLabel = submitBtn.textContent;
  submitBtn.textContent = needsInit ? 'Criando workspace...' : 'Desbloqueando...';
  try {
    let tokenResp;
    if (needsInit) {
      tokenResp = await apiPost('/api/init', {
        workspace,
        password,
        confirm: $('#field-confirm').value,
      });
    } else {
      tokenResp = await apiPost('/api/unlock', { workspace, password });
    }
    token = tokenResp.token;
    localStorage.setItem(TOKEN_KEY, token);
    localStorage.setItem(WORKSPACE_KEY, workspace);
    await loadDashboard();
  } catch (err) {
    errEl.textContent = err.message;
    errEl.classList.remove('hidden');
    submitBtn.disabled = false;
    submitBtn.textContent = originalLabel;
  }
});

$('#btn-lock').addEventListener('click', async () => {
  try {
    if (token) {
      await apiPost('/api/lock', {});
    }
  } catch { /* ignore */ }
  lockLocal();
  await showUnlock();
});

function lockLocal() {
  token = '';
  localStorage.removeItem(TOKEN_KEY);
  localStorage.removeItem(WORKSPACE_KEY);
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async function loadDashboard(showOverlay = true) {
  if (showOverlay) {
    showLoading('Carregando painel...');
  }
  try {
    dashboard = await apiGet('/api/dashboard');
  } catch (e) {
    hideLoading();
    showView('view-unlock');
    return;
  }
  hideLoading();
  showView('view-dashboard');
  renderDashboard();
  renderTotals();
}

function renderTotals() {
  const t = dashboard.totals;
  $('#totals').textContent =
    `${t.databases} banco(s) e ${t.extra_users} usuário(s) extra(s) em ${dashboard.instances.length} instância(s).`;
}

function healthBadge(health) {
  const status = health ? health.status : 'unknown';
  const label = {
    ok: health && health.latency_ms != null ? `OK ${health.latency_ms}ms` : 'OK',
    error: 'ERRO',
    checking: 'Verificando...',
    unknown: 'Desconhecido',
  }[status] || 'Desconhecido';
  return el('span', { class: 'health ' + status, text: label });
}

function renderDashboard() {
  renderInstanceList();
  renderPagination();
}

function filteredInstances() {
  const query = ($('#instance-filter').value || '').trim().toLowerCase();
  if (!query) {
    return dashboard.instances;
  }
  const matches = [];
  for (const inst of dashboard.instances) {
    if (inst.name.toLowerCase().includes(query) || hostLabel(inst).toLowerCase().includes(query)) {
      matches.push(inst);
    }
  }
  return matches;
}

function renderInstanceList() {
  const container = $('#instance-cards');
  container.innerHTML = '';
  if (!dashboard.instances.length) {
    container.appendChild(el('p', { class: 'empty', text: 'Nenhuma instância. Clique em "+ Instância" para começar.' }));
    return;
  }
  const instances = filteredInstances();
  if (!instances.length) {
    container.appendChild(el('p', { class: 'empty', text: 'Nenhuma instância corresponde ao filtro.' }));
    return;
  }
  const pageCount = Math.max(1, Math.ceil(instances.length / INSTANCES_PER_PAGE));
  if (instancePage > pageCount) {
    instancePage = pageCount;
  }
  const start = (instancePage - 1) * INSTANCES_PER_PAGE;
  for (const inst of instances.slice(start, start + INSTANCES_PER_PAGE)) {
    container.appendChild(instanceCard(inst));
  }
}

function renderPagination() {
  const box = $('#inst-pagination');
  box.innerHTML = '';
  const total = filteredInstances().length;
  if (total <= INSTANCES_PER_PAGE) {
    return;
  }
  const pageCount = Math.ceil(total / INSTANCES_PER_PAGE);
  const goto = (page) => {
    instancePage = Math.min(pageCount, Math.max(1, page));
    renderInstanceList();
    renderPagination();
  };
  box.appendChild(el('button', {
    class: 'small ghost',
    type: 'button',
    text: '←',
    disabled: instancePage <= 1 ? 'disabled' : null,
    onclick: () => goto(instancePage - 1),
  }));
  box.appendChild(el('span', { class: 'page-info', text: `Página ${instancePage} de ${pageCount} · ${total} instância(s)` }));
  box.appendChild(el('button', {
    class: 'small ghost',
    type: 'button',
    text: '→',
    disabled: instancePage >= pageCount ? 'disabled' : null,
    onclick: () => goto(instancePage + 1),
  }));
}

function toggleInstance(name) {
  expandedInstance = expandedInstance === name ? null : name;
  renderInstanceList();
}

function instanceCard(inst) {
  const expanded = expandedInstance === inst.name;
  const dbCount = inst.databases.filter((d) => d.kind === 'db').length;
  const userCount = inst.databases.filter((d) => d.kind === 'user').length;
  const summary = `${dbCount} banco(s) · ${userCount} usuário(s) extra(s)`;

  const stop = (fn) => (e) => { e.stopPropagation(); fn(); };
  const head = el('div', { class: 'card-head clickable', onclick: () => toggleInstance(inst.name) }, [
    el('span', { class: 'caret', text: expanded ? '▾' : '▸' }),
    el('span', { class: 'inst-name', text: inst.name }),
    healthBadge(inst.health),
    el('span', { class: 'inst-summary', text: summary }),
    el('span', { class: 'inst-host', text: hostLabel(inst) }),
    el('div', { class: 'inst-actions' }, [
      el('button', { class: 'small ghost', type: 'button', onclick: stop(() => testInstance(inst.name)), text: 'Testar' }),
      el('button', { class: 'small ghost', type: 'button', onclick: stop(() => revealInstanceUrl(inst.name)), text: 'URL base' }),
      el('button', { class: 'small ghost', type: 'button', onclick: stop(() => rotateInstance(inst.name)), text: 'Rotacionar' }),
      el('button', { class: 'small danger', type: 'button', onclick: stop(() => removeInstance(inst.name)), text: 'Remover' }),
    ]),
  ]);

  const children = [head];
  if (expanded) {
    const tbody = el('tbody');
    for (const db of inst.databases) {
      tbody.appendChild(dbRow(inst.name, db));
    }
    const table = el('table', {}, [
      el('thead', {}, [
        el('tr', {}, [
          el('th', { text: 'Banco' }), el('th', { text: 'Aplicação' }),
          el('th', { text: 'Role' }), el('th', { text: '' }),
        ]),
      ]),
      tbody,
    ]);

    // Filters the rows directly in the DOM so the input never loses focus.
    const dbFilter = el('input', {
      class: 'db-filter',
      type: 'search',
      placeholder: 'Filtrar bancos por nome, aplicação ou role...',
      autocomplete: 'off',
    });
    dbFilter.addEventListener('input', () => {
      const query = dbFilter.value.trim().toLowerCase();
      for (const row of tbody.rows) {
        const match = !query || row.textContent.toLowerCase().includes(query);
        row.style.display = match ? '' : 'none';
      }
    });

    const foot = el('div', { class: 'card-foot' }, [
      el('button', { class: 'ghost', type: 'button', onclick: () => openBackupModal(inst.name), text: 'Backup' }),
      el('button', { class: 'ghost', type: 'button', onclick: () => openAdoptModal(inst.name), text: 'Adicionar banco existente' }),
      el('button', { class: 'primary', type: 'button', onclick: () => openProvisionModal(inst.name), text: '+ Provisionar banco nesta instância' }),
    ]);

    children.push(dbFilter, table, foot);
  }

  return el('div', { class: 'card' + (expanded ? ' expanded' : '') }, children);
}

function hostLabel(inst) {
  if (inst.host) {
    return `${inst.host}:${inst.port}  /  ${inst.base_database || ''}`;
  }
  return 'host desconhecido';
}

function dbRow(instanceName, db) {
  const tagClass = db.kind === 'db' ? 'owner' : 'user';
  const tagText = db.kind === 'db' ? 'owner' : 'user';
  const nameCell = el('td', {}, [
    el('span', { text: db.database_name }),
    el('span', { class: 'tag ' + tagClass, text: tagText }),
  ]);
  const actions = el('td', { class: 'row-actions' }, [
    el('button', { class: 'link', type: 'button', onclick: () => copyConnection(db), text: 'copiar' }),
    el('button', { class: 'link', type: 'button', onclick: () => revealConnection(db), text: 'ver' }),
    el('button', { class: 'link', type: 'button', onclick: () => openQueryView({ kind: db.kind, id: db.id }), text: 'consultar' }),
    el('button', { class: 'link', type: 'button', onclick: () => testConnection(db), text: 'testar' }),
    el('button', { class: 'link', type: 'button', onclick: () => rotateConnection(db), text: 'rotacionar' }),
    el('button', { class: 'link', type: 'button', onclick: () => editName(db), text: 'renomear' }),
    el('button', { class: 'link', type: 'button', onclick: () => deleteConnection(instanceName, db), text: 'excluir' }),
  ]);
  return el('tr', {}, [
    nameCell,
    el('td', { text: db.application_name }),
    el('td', { class: 'mono', text: db.role_or_username }),
    actions,
  ]);
}

async function fetchConnection(kind, id) {
  const data = await apiGet(`/api/connections/${kind}/${id}`);
  return data.connection_string;
}

async function copyConnection(db) {
  try {
    const cs = await fetchConnection(db.kind, db.id);
    await copyText(cs);
  } catch (e) {
    setStatus(e.message);
  }
}

async function revealConnection(db) {
  try {
    const cs = await fetchConnection(db.kind, db.id);
    openModal(`
      <h2>${escapeHtml(db.database_name)} — ${escapeHtml(db.role_or_username)}</h2>
      <div class="result-box">
        <div class="cs">${escapeHtml(cs)}</div>
        <div class="form-actions">
          <button id="rc-copy" class="primary">Copiar</button>
          <button id="rc-close" class="ghost">Fechar</button>
        </div>
      </div>
    `);
    $('#rc-copy').addEventListener('click', () => copyText(cs));
    $('#rc-close').addEventListener('click', closeModal);
  } catch (e) {
    setStatus(e.message);
  }
}

// Opens the health-check modal right away and renders the outcome of `run`
// (called with the AbortController's signal) inside it. The button reads
// "Cancelar" while the check is running — closing the modal aborts it — and
// becomes "Fechar" once a result is shown.
function openHealthCheckModal(title, controller, run) {
  openModal(`
    <h2>Testar conexão — ${escapeHtml(title)}</h2>
    <div class="health-check" id="hc-status" data-state="checking">
      <span class="spinner" id="hc-spinner"></span>
      <span id="hc-label">Verificando conexão...</span>
    </div>
    <div id="hc-detail" class="hint"></div>
    <div class="form-actions">
      <button id="hc-cancel" class="ghost">Cancelar</button>
    </div>
  `);
  activeHealthCheck = controller;

  const statusBox = $('#hc-status');
  const spinner = $('#hc-spinner');
  const label = $('#hc-label');
  const detail = $('#hc-detail');

  const showResult = (state, message, detailHtml) => {
    activeHealthCheck = null;
    statusBox.dataset.state = state;
    label.textContent = message;
    detail.innerHTML = detailHtml || '';
    spinner.classList.add('hidden');
    $('#hc-cancel').textContent = 'Fechar';
  };

  $('#hc-cancel').addEventListener('click', closeModal);

  Promise.resolve()
    .then(() => run(controller.signal))
    .then((info) => {
      if (controller.signal.aborted) {
        return;
      }
      if (info.status === 'ok') {
        showResult(
          'ok',
          `Conexão OK (${info.latency_ms}ms)`,
          info.version ? `<div class="cs">${escapeHtml(info.version)}</div>` : '',
        );
      } else if (info.timed_out) {
        showResult('timeout', 'Tempo esgotado', escapeHtml(info.error || ''));
      } else {
        showResult('error', 'Falha na conexão', escapeHtml(info.error || ''));
      }
    })
    .catch((error) => {
      if (controller.signal.aborted) {
        return;
      }
      const timedOut = error.message === 'Tempo esgotado aguardando o servidor.';
      showResult(
        timedOut ? 'timeout' : 'error',
        timedOut ? 'Tempo esgotado' : 'Falha na conexão',
        escapeHtml(error.message),
      );
    });
}

function testConnection(db) {
  const controller = new AbortController();
  openHealthCheckModal(db.database_name, controller, (signal) =>
    api(`/api/connections/${db.kind}/${db.id}/health`, { method: 'POST', signal }),
  );
}

// On-demand health check for a single instance. Opens the modal immediately
// (cancellable while the check runs) and updates the card badge with the
// outcome.
function testInstance(name) {
  const inst = dashboard.instances.find((entry) => entry.name === name);
  const controller = new AbortController();
  const previous = inst && inst.health && inst.health.status ? inst.health : { status: 'unknown' };
  if (inst) {
    inst.health = { status: 'checking' };
    renderInstanceList();
  }
  openHealthCheckModal(name, controller, async (signal) => {
    try {
      const info = await api(`/api/instances/${encodeURIComponent(name)}/health`, { method: 'POST', signal });
      if (inst) {
        inst.health = info;
        renderInstanceList();
      }
      return info;
    } catch (error) {
      if (inst) {
        inst.health = controller.signal.aborted ? previous : { status: 'error', error: error.message };
        renderInstanceList();
      }
      if (controller.signal.aborted) {
        setStatus('Verificação cancelada.');
      }
      throw error;
    }
  });
}

async function removeInstance(name) {
  const confirmed = await confirmModalCountdown(
    'Remover instância',
    `Remover a instância ${name} e todas as conexões salvas dela (bancos e usuários extras)? Apenas o catálogo local é removido — nada é apagado no servidor PostgreSQL.`,
    'Remover',
  );
  if (!confirmed) {
    return;
  }
  try {
    await apiDelete(`/api/instances/${encodeURIComponent(name)}`);
    if (expandedInstance === name) {
      expandedInstance = null;
    }
    await loadDashboard();
    setStatus(`Instância '${name}' removida do catálogo.`);
  } catch (e) {
    setStatus(e.message);
  }
}

async function rotateConnection(db) {
  const confirmed = await confirmModalCountdown(
    'Rotacionar credencial',
    `Rotacionar a senha de ${db.role_or_username} (${db.database_name})? A senha atual é invalidada imediatamente — serviços que a usam vão falhar até serem atualizados.`,
    'Rotacionar',
  );
  if (!confirmed) {
    return;
  }
  try {
    const data = await api(`/api/connections/${db.kind}/${db.id}/rotate`, { method: 'POST' });
    showRotatedCredential(data.connection_string, `${db.database_name} · ${data.role_name}`);
    await loadDashboard(false);
  } catch (error) {
    setStatus(error.message);
  }
}

async function rotateInstance(name) {
  const confirmed = await confirmModalCountdown(
    'Rotacionar credencial da instância',
    `Rotacionar a senha do usuário base da instância ${name}? A base DATABASE_URL salva no catálogo será atualizada com a nova senha.`,
    'Rotacionar',
  );
  if (!confirmed) {
    return;
  }
  try {
    const data = await api(`/api/instances/${encodeURIComponent(name)}/rotate`, { method: 'POST' });
    showRotatedCredential(data.connection_string, name);
    await loadDashboard(false);
  } catch (error) {
    setStatus(error.message);
  }
}

function showRotatedCredential(connectionString, title) {
  openModal(`
    <h2>Credencial rotacionada — ${escapeHtml(title)}</h2>
    <p class="hint">Nova connection string. Atualize o serviço que a utiliza.</p>
    <div class="result-box">
      <div class="cs">${escapeHtml(connectionString)}</div>
      <div class="form-actions">
        <button id="rot-copy" class="primary">Copiar</button>
        <button id="rot-close" class="ghost">Fechar</button>
      </div>
    </div>
  `);
  $('#rot-copy').addEventListener('click', () => copyText(connectionString));
  $('#rot-close').addEventListener('click', closeModal);
}

async function revealInstanceUrl(name) {
  try {
    const data = await apiGet('/api/connections/instance/' + encodeURIComponent(name));
    openModal(`
      <h2>URL base — ${escapeHtml(name)}</h2>
      <div class="result-box">
        <div class="cs">${escapeHtml(data.connection_string)}</div>
        <div class="form-actions">
          <button id="ri-copy" class="primary">Copiar</button>
          <button id="ri-close" class="ghost">Fechar</button>
        </div>
      </div>
    `);
    $('#ri-copy').addEventListener('click', () => copyText(data.connection_string));
    $('#ri-close').addEventListener('click', closeModal);
  } catch (e) {
    setStatus(e.message);
  }
}

async function editName(db) {
  openModal(`
    <h2>Renomear aplicação</h2>
    <div class="form-grid">
      <label>Banco / role: <span class="hint">${escapeHtml(db.database_name)} · ${escapeHtml(db.role_or_username)}</span></label>
      <label>
        Novo nome da aplicação
        <input id="en-name" type="text" value="${escapeHtml(db.application_name)}">
      </label>
      <div class="form-actions">
        <button id="en-cancel" class="ghost">Cancelar</button>
        <button id="en-save" class="primary">Salvar</button>
      </div>
    </div>
  `);
  $('#en-save').addEventListener('click', async () => {
    const name = $('#en-name').value.trim();
    if (!name) {
      return setStatus('Nome não pode ser vazio.');
    }
    try {
      await apiPut(`/api/connections/${db.kind}/${db.id}/name`, { name });
      closeModal();
      await loadDashboard();
      setStatus('Nome atualizado.');
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#en-cancel').addEventListener('click', closeModal);
  $('#en-name').focus();
}

async function deleteConnection(instanceName, db) {
  const ok = await confirmModalCountdown(
    'Excluir conexão',
    `Excluir a conexão ${db.database_name} (${db.role_or_username}) da instância ${instanceName}? Isso não remove o banco do PostgreSQL.`,
    'Excluir',
  );
  if (!ok) {
    return;
  }
  try {
    await apiDelete(`/api/connections/${db.kind}/${db.id}`);
    await loadDashboard();
    setStatus('Conexão excluída.');
  } catch (e) {
    setStatus(e.message);
  }
}

// ---------------------------------------------------------------------------
// Add instance
// ---------------------------------------------------------------------------

$('#btn-add-instance').addEventListener('click', () => {
  openModal(`
    <h2>Adicionar instância</h2>
    <div class="form-grid">
      <label>
        Nome da instância
        <input id="ai-name" type="text" placeholder="ex.: prod">
      </label>
      <label>
        Base DATABASE_URL
        <input id="ai-url" type="text" placeholder="postgresql://user:pass@host:5432/postgres">
      </label>
      <p class="hint">Ex.: postgresql://admin:secret@db.example.com:5432/postgres</p>
      <div class="form-actions">
        <button id="ai-cancel" class="ghost">Cancelar</button>
        <button id="ai-save" class="primary">Adicionar</button>
      </div>
    </div>
  `);
  $('#ai-save').addEventListener('click', async () => {
    const name = $('#ai-name').value.trim();
    const url = $('#ai-url').value.trim();
    if (!name || !url) {
      return setStatus('Preencha nome e URL.');
    }
    try {
      const info = await apiPost('/api/instances', { name, url });
      closeModal();
      await loadDashboard();
      setStatus(`Instância '${info.name}' adicionada (${info.host}:${info.port} / ${info.database}).`);
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#ai-cancel').addEventListener('click', closeModal);
  $('#ai-name').focus();
});

// ---------------------------------------------------------------------------
// Instance filter
// ---------------------------------------------------------------------------

$('#instance-filter').addEventListener('input', () => {
  instancePage = 1;
  renderInstanceList();
  renderPagination();
});

// ---------------------------------------------------------------------------
// Provision
// ---------------------------------------------------------------------------

function openProvisionModal(instanceName) {
  openModal(`
    <h2>Provisionar banco — ${escapeHtml(instanceName)}</h2>
    <div class="form-grid">
      <label>
        Nome do banco
        <input id="pv-db" type="text" placeholder="ex.: orders_api">
      </label>
      <label>
        Nome da aplicação (opcional)
        <input id="pv-app" type="text" placeholder="se vazio, usa o nome do banco">
      </label>
      <label class="check-row">
        <input id="pv-dedicated" type="checkbox" checked>
        <span>Criar role owner dedicada ({banco}_owner)</span>
      </label>
      <p class="hint">Desmarque para o banco pertencer ao usuário da URL base — nenhuma role nova é criada e a conexão salva reutiliza as credenciais da instância.</p>
      <label>
        Usuário extra (opcional)
        <input id="pv-extra-user" type="text" placeholder="ex.: orders_worker">
      </label>
      <label>
        App name do usuário extra (opcional)
        <input id="pv-extra-app" type="text" placeholder="se vazio, usa o nome do banco">
      </label>
      <p class="hint">Cria o banco no catálogo e gera connection strings. A role owner dedicada é opcional.</p>
      <div class="form-actions">
        <button id="pv-cancel" class="ghost">Cancelar</button>
        <button id="pv-start" class="primary">Provisionar</button>
      </div>
    </div>
  `);
  $('#pv-start').addEventListener('click', async () => {
    const body = {
      instance_name: instanceName,
      database_name: $('#pv-db').value.trim(),
      application_name: $('#pv-app').value.trim() || null,
      extra_username: $('#pv-extra-user').value.trim() || null,
      extra_application_name: $('#pv-extra-app').value.trim() || null,
      dedicated_owner: $('#pv-dedicated').checked,
    };
    if (!body.database_name) {
      return setStatus('Informe o nome do banco.');
    }
    try {
      const data = await apiPost('/api/provision', body);
      closeModal();
      startOperation(data.operation_id);
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#pv-cancel').addEventListener('click', closeModal);
  $('#pv-db').focus();
}

// ---------------------------------------------------------------------------
// Adopt existing database
// ---------------------------------------------------------------------------

async function openAdoptModal(instanceName) {
  openModal(`
    <h2>Adicionar banco existente — ${escapeHtml(instanceName)}</h2>
    <div class="wizard-step">
      <p class="hint">Bancos presentes na instância que ainda não estão no catálogo.</p>
      <div id="ad-dbs" class="checkboxes"></div>
      <label class="form-grid">
        Nome da aplicação (opcional)
        <input id="ad-app" type="text" placeholder="se vazio, usa o nome do banco">
      </label>
      <div class="form-actions">
        <button id="ad-cancel" class="ghost">Cancelar</button>
        <button id="ad-start" class="primary">Adicionar</button>
      </div>
    </div>
  `);

  const box = $('#ad-dbs');
  box.innerHTML = '<p class="hint">Descobrindo bancos...</p>';
  try {
    const instance = dashboard.instances.find((entry) => entry.name === instanceName);
    const known = new Set((instance ? instance.databases : []).map((db) => db.database_name));
    const discovered = await apiPost('/api/discover', { source: { kind: 'instance', name: instanceName } });
    const candidates = discovered.filter((db) => !known.has(db.name));
    box.innerHTML = '';
    if (!candidates.length) {
      box.innerHTML = '<p class="hint">Todos os bancos da instância já estão no catálogo.</p>';
    } else {
      for (const db of candidates) {
        box.appendChild(el('label', {}, [
          el('input', { type: 'checkbox', value: db.name, checked: true }),
          el('span', { text: `${db.name} (owner: ${db.owner})` }),
        ]));
      }
    }
  } catch (error) {
    box.innerHTML = `<span class="error">${escapeHtml(error.message)}</span>`;
  }

  $('#ad-start').addEventListener('click', async () => {
    const selected = Array.from(box.querySelectorAll('input[type=checkbox]:checked')).map((input) => input.value);
    if (!selected.length) {
      return setStatus('Selecione ao menos um banco.');
    }
    const applicationName = $('#ad-app').value.trim() || null;
    try {
      for (const databaseName of selected) {
        await apiPost('/api/adopt', {
          instance_name: instanceName,
          database_name: databaseName,
          application_name: applicationName,
        });
      }
      closeModal();
      await loadDashboard();
      setStatus(`Adicionado(s) ${selected.length} banco(s) ao catálogo.`);
    } catch (error) {
      setStatus(error.message);
    }
  });
  $('#ad-cancel').addEventListener('click', closeModal);
}

// ---------------------------------------------------------------------------
// Migrate wizard
// ---------------------------------------------------------------------------

$('#btn-migrate').addEventListener('click', openMigrateModal);

function openMigrateModal() {
  if (!dashboard.instances.length) {
    return setStatus('Adicione uma instância antes de migrar.');
  }

  openModal(`
    <h2>Migrar banco</h2>
    <div class="wizard-step">
      <p class="hint">1. Instância de origem</p>
      <select id="mg-src-inst">
        ${dashboard.instances.map((instance) => `<option value="${escapeHtml(instance.name)}">${escapeHtml(instance.name)}</option>`).join('')}
      </select>
      <p class="hint">2. Fonte (o banco que será migrado)</p>
      <div id="mg-sources"></div>
      <p class="hint">3. Instância destino</p>
      <select id="mg-dest">
        ${dashboard.instances.map((instance) => `<option value="${escapeHtml(instance.name)}">${escapeHtml(instance.name)}</option>`).join('')}
      </select>
      <label class="form-grid">
        Nome do banco no destino
        <input id="mg-db" type="text" placeholder="ex.: orders_clone">
      </label>
      <label class="check-row">
        <input id="mg-replace" type="checkbox">
        <span>Substituir se o banco já existir no destino (DROP DATABASE WITH FORCE)</span>
      </label>
      <div class="form-actions">
        <button id="mg-cancel" class="ghost">Cancelar</button>
        <button id="mg-start" class="primary">Migrar</button>
      </div>
    </div>
  `);

  let selectedSource = null;
  const renderSources = (instanceName) => {
    selectedSource = null;
    const box = $('#mg-sources');
    box.innerHTML = '';
    for (const source of buildSourceOptionsFor(instanceName)) {
      const item = el('div', { class: 'source-item', text: source.label });
      item.addEventListener('click', () => {
        for (const sibling of box.querySelectorAll('.source-item')) {
          sibling.classList.remove('selected');
        }
        item.classList.add('selected');
        selectedSource = source.value;
      });
      box.appendChild(item);
    }
  };

  renderSources($('#mg-src-inst').value);
  $('#mg-src-inst').addEventListener('change', (e) => renderSources(e.target.value));

  $('#mg-start').addEventListener('click', async () => {
    if (!selectedSource) {
      return setStatus('Selecione a fonte.');
    }
    const destInstance = $('#mg-dest').value;
    const destDbName = $('#mg-db').value.trim();
    if (!destDbName) {
      return setStatus('Informe o nome do banco no destino.');
    }
    try {
      const data = await apiPost('/api/migrate', {
        source: selectedSource,
        dest_instance: destInstance,
        dest_db_name: destDbName,
        replace_existing: $('#mg-replace').checked,
      });
      closeModal();
      startOperation(data.operation_id);
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#mg-cancel').addEventListener('click', closeModal);
}

function buildSourceOptionsFor(instanceName) {
  const inst = dashboard.instances.find((entry) => entry.name === instanceName);
  const opts = [];
  if (!inst) {
    return opts;
  }
  for (const db of inst.databases) {
    if (db.kind === 'db') {
      opts.push({ value: { kind: 'db', id: db.id }, label: `${db.database_name} (owner)` });
    } else {
      opts.push({ value: { kind: 'user', id: db.id }, label: `${db.database_name} (${db.role_or_username})` });
    }
  }
  opts.push({ value: { kind: 'instance', name: inst.name }, label: `${inst.name} (base URL)` });
  return opts;
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

$('#btn-backup').addEventListener('click', () => openBackupModal());

function openBackupModal(preselectInstance = null) {
  if (!dashboard.instances.length) {
    return setStatus('Adicione uma instância antes de criar um backup.');
  }
  const instanceOptions = dashboard.instances
    .map((instance) => `<option value="${escapeHtml(instance.name)}"${instance.name === preselectInstance ? ' selected' : ''}>${escapeHtml(instance.name)}</option>`)
    .join('');

  openModal(`
    <h2>Novo backup</h2>
    <div class="wizard-step">
      <p class="hint">1. Instância de origem</p>
      <select id="bk-src-inst">${instanceOptions}</select>
      <p class="hint">2. Fonte</p>
      <div id="bk-sources"></div>
      <div id="bk-db-wrap">
        <p class="hint">Bancos da instância</p>
        <div id="bk-dbs" class="checkboxes"></div>
      </div>
      <div id="bk-tables-wrap" class="hidden">
        <p class="hint">Tabelas</p>
        <div class="checkboxes" id="bk-table-mode">
          <label><input type="radio" name="bk-table-mode" value="all" checked> Todas as tabelas</label>
          <label><input type="radio" name="bk-table-mode" value="select"> Selecionar tabelas</label>
        </div>
        <p class="hint">Bancos com hypertables: o dado vive nos chunks internos — prefira "Todas as tabelas".</p>
        <div id="bk-tables" class="checkboxes hidden"></div>
      </div>
      <p class="hint">Opções</p>
      <div id="bk-config" class="checkboxes">
        <label><input id="bk-globals" type="checkbox" checked> Incluir roles e membros do cluster</label>
        <label><input id="bk-passwords" type="checkbox"> Incluir hashes de senha de roles</label>
        <label><input id="bk-flatten" type="checkbox" checked> Achatar tablespaces</label>
      </div>
      <div class="form-actions">
        <button id="bk-cancel" class="ghost">Cancelar</button>
        <button id="bk-start" class="primary">Fazer backup</button>
      </div>
    </div>
  `);

  let selectedSource = null;
  const renderSources = (instanceName) => {
    selectedSource = null;
    const box = $('#bk-sources');
    box.innerHTML = '';
    for (const source of buildSourceOptionsFor(instanceName)) {
      const item = el('div', { class: 'source-item', text: source.label });
      item.addEventListener('click', () => {
        for (const sibling of box.querySelectorAll('.source-item')) {
          sibling.classList.remove('selected');
        }
        item.classList.add('selected');
        selectedSource = source.value;
        updateBackupDbs(selectedSource);
        updateBackupTables(selectedSource);
      });
      box.appendChild(item);
    }
  };

  renderSources($('#bk-src-inst').value);
  $('#bk-src-inst').addEventListener('change', (e) => renderSources(e.target.value));

  function updateBackupTables(source) {
    const wrap = $('#bk-tables-wrap');
    const modeBox = $('#bk-table-mode');
    const list = $('#bk-tables');
    if (!source || source.kind === 'instance') {
      wrap.classList.add('hidden');
      list.innerHTML = '';
      return;
    }
    wrap.classList.remove('hidden');
    const radios = modeBox.querySelectorAll('input[type=radio]');
    radios.forEach((radio) => {
      radio.checked = radio.value === 'all';
    });
    list.classList.add('hidden');
    list.innerHTML = '';

    modeBox.addEventListener('change', () => {
      const selectMode = $('#bk-table-mode input[value=select]').checked;
      list.classList.toggle('hidden', !selectMode);
      if (selectMode && !list.dataset.loaded) {
        loadBackupTables(source, list);
      }
    });
  }

  async function loadBackupTables(source, list) {
    list.innerHTML = '<p class="hint">Carregando tabelas...</p>';
    try {
      const tables = await apiPost('/api/query/tables', { source });
      list.innerHTML = '';
      const visible = tables.filter(
        (t) => !t.schema.startsWith('_timescaledb') && !t.schema.startsWith('timescaledb_'),
      );
      if (!visible.length) {
        list.appendChild(el('p', { class: 'hint', text: 'Nenhuma tabela de usuário.' }));
        list.dataset.loaded = '1';
        return;
      }
      for (const t of visible) {
        list.appendChild(el('label', {}, [
          el('input', { type: 'checkbox', value: `${t.schema}.${t.name}`, checked: true }),
          el('span', { text: `${t.schema}.${t.name} (${formatBytes(t.size_bytes)})` }),
        ]));
      }
      list.dataset.loaded = '1';
    } catch (e) {
      list.innerHTML = '';
      list.appendChild(el('p', { class: 'error', text: e.message }));
    }
  }

  async function updateBackupDbs(source) {
    const wrap = $('#bk-db-wrap');
    const box = $('#bk-dbs');
    if (!source || source.kind !== 'instance') {
      wrap.classList.add('hidden');
      box.innerHTML = '';
      return;
    }
    wrap.classList.remove('hidden');
    box.innerHTML = '<p class="hint">Descobrindo bancos...</p>';
    try {
      const dbs = await apiPost('/api/discover', { source });
      box.innerHTML = '';
      for (const db of dbs) {
        box.appendChild(el('label', {}, [
          el('input', { type: 'checkbox', value: db.name, checked: true }),
          el('span', { text: `${db.name} (owner: ${db.owner})` }),
        ]));
      }
    } catch (e) {
      box.innerHTML = `<span class="error">${escapeHtml(e.message)}</span>`;
    }
  }

  $('#bk-start').addEventListener('click', async () => {
    if (!selectedSource) {
      return setStatus('Selecione a fonte.');
    }
    const databases = Array.from($('#bk-dbs').querySelectorAll('input[type=checkbox]:checked')).map((i) => i.value);
    if (selectedSource.kind === 'instance' && !databases.length) {
      return setStatus('Selecione ao menos um banco.');
    }
    let tables = null;
    const tablesWrap = $('#bk-tables-wrap');
    const selectMode = !tablesWrap.classList.contains('hidden')
      && $('#bk-table-mode input[value=select]').checked;
    if (selectMode) {
      tables = Array.from($('#bk-tables input[type=checkbox]:checked')).map((i) => i.value);
      if (!tables.length) {
        return setStatus('Selecione ao menos uma tabela.');
      }
    }
    try {
      const data = await apiPost('/api/backup', {
        source: selectedSource,
        databases,
        tables,
        include_globals: $('#bk-globals').checked,
        include_role_passwords: $('#bk-passwords').checked,
        flatten_tablespaces: $('#bk-flatten').checked,
      });
      closeModal();
      startOperation(data.operation_id);
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#bk-cancel').addEventListener('click', closeModal);
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

$('#btn-restore').addEventListener('click', async () => {
  if (!dashboard.instances.length) {
    return setStatus('Adicione uma instância antes de restaurar.');
  }
  let backups;
  try {
    backups = await apiGet('/api/backups');
  } catch (e) {
    return setStatus(e.message);
  }
  if (!backups.length) {
    return setStatus('Nenhum backup encontrado em ' + '(diretório de backups).');
  }
  const options = backups
    .map((b) => `<option value="${escapeHtml(b.filename)}">${escapeHtml(b.filename)} (${formatBytes(b.size)})</option>`)
    .join('');

  openModal(`
    <h2>Restaurar</h2>
    <div class="wizard-step">
      <label class="form-grid">
        Arquivo de backup
        <select id="rs-file">${options}</select>
      </label>
      <p class="hint">2. Instância destino</p>
      <select id="rs-dest">
        ${dashboard.instances.map((i) => `<option value="${escapeHtml(i.name)}">${escapeHtml(i.name)}</option>`).join('')}
      </select>
      <div id="rs-detail" class="form-grid"></div>
      <div class="form-actions">
        <button id="rs-cancel" class="ghost">Cancelar</button>
        <button id="rs-start" class="primary">Restaurar</button>
      </div>
    </div>
  `);

  async function loadPreview() {
    const file = $('#rs-file').value;
    const detail = $('#rs-detail');
    detail.innerHTML = '<p class="hint">Carregando preview...</p>';
    try {
      const preview = await apiPost('/api/restore/preview', { file });
      if (preview.type === 'bundle') {
        detail.innerHTML = `
          <p class="hint">Bundle de instância — fonte: ${escapeHtml(preview.source_instance || '?')} (${escapeHtml(preview.source_version || '?')})</p>
          <p class="hint">Bancos: ${escapeHtml(preview.databases.join(', '))}</p>
          <p class="hint">Globals do cluster: ${preview.includes_globals ? 'sim' : 'não'}</p>
          <label>
            Política de conflito
            <select id="rs-policy">
              <option value="skip" selected>Skip (não sobrescreve existentes)</option>
              <option value="fail">Fail (aborta se existir)</option>
            </select>
          </label>
        `;
      } else {
        const m = preview.metadata || {};
        detail.innerHTML = `
          <p class="hint">Backup simples — origem: ${escapeHtml(m.database_name || '?')} em ${escapeHtml(m.instance_name || '?')} (${escapeHtml(m.hostname || '?')}, ${escapeHtml(m.timestamp || '?')})</p>
          <label>
            Nome do banco no destino
            <input id="rs-dbname" type="text" value="${escapeHtml(m.database_name || '')}" placeholder="ex.: orders">
          </label>
          <label>
            Política de conflito
            <select id="rs-policy">
              <option value="fail" selected>Fail (aborta se existir)</option>
              <option value="skip">Skip (não sobrescreve existentes)</option>
            </select>
          </label>
        `;
      }
    } catch (e) {
      detail.innerHTML = `<span class="error">${escapeHtml(e.message)}</span>`;
    }
  }

  $('#rs-file').addEventListener('change', loadPreview);
  loadPreview();

  $('#rs-start').addEventListener('click', async () => {
    const file = $('#rs-file').value;
    const destInstance = $('#rs-dest').value;
    const dbNameInput = $('#rs-dbname');
    const destDbName = dbNameInput ? dbNameInput.value.trim() : '';
    const policy = $('#rs-policy').value;
    try {
      const data = await apiPost('/api/restore', {
        file,
        dest_instance: destInstance,
        dest_db_name: destDbName || null,
        conflict_policy: policy,
      });
      closeModal();
      startOperation(data.operation_id);
    } catch (e) {
      setStatus(e.message);
    }
  });
  $('#rs-cancel').addEventListener('click', closeModal);
});

// ---------------------------------------------------------------------------
// Backups view
// ---------------------------------------------------------------------------

$('#btn-backups').addEventListener('click', showBackups);
$('#back-to-dashboard').addEventListener('click', () => loadDashboard());
$('#btn-refresh-backups').addEventListener('click', showBackups);

async function showBackups() {
  showView('view-backups');
  await refreshBackups();
}

async function refreshBackups() {
  const tbody = $('#backups-tbody');
  tbody.innerHTML = '';
  let backups;
  try {
    backups = await apiGet('/api/backups');
  } catch (e) {
    setStatus(e.message);
    return;
  }
  $('#backups-empty').classList.toggle('hidden', backups.length > 0);
  for (const b of backups) {
    const row = el('tr', {}, [
      el('td', { class: 'mono', text: b.filename }),
      el('td', { text: formatBytes(b.size) }),
      el('td', { text: b.modified }),
      el('td', { class: 'row-actions' }, [
        el('button', { class: 'link', type: 'button', onclick: () => openRestoreFor(b.filename), text: 'restaurar' }),
      ]),
    ]);
    tbody.appendChild(row);
  }
}

function openRestoreFor(filename) {
  $('#btn-restore').click();
  // Set the file select to the chosen backup once the modal exists.
  setTimeout(() => {
    const sel = $('#rs-file');
    if (sel) {
      sel.value = filename;
      sel.dispatchEvent(new Event('change'));
    }
  }, 50);
}

function formatBytes(size) {
  if (size < 1024) {
    return `${size} B`;
  }
  if (size < 1024 * 1024) {
    return `${(size / 1024).toFixed(1)} KB`;
  }
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

// ---------------------------------------------------------------------------
// Query console
// ---------------------------------------------------------------------------

const QUERY_PAGE_SIZE = 200;
const QUERY_DATA_HINT = 'Selecione uma tabela na lateral.';

let queryState = {
  source: null,
  sources: [],
  tables: [],
  activeTable: null,
  offset: 0,
  hasMore: false,
  loadingMore: false,
};

function openQueryView(source = null) {
  const instanceSelect = $('#query-instance');
  instanceSelect.innerHTML = '';
  for (const inst of dashboard.instances) {
    instanceSelect.appendChild(el('option', { value: inst.name, text: inst.name }));
  }

  let instanceName = dashboard.instances.length ? dashboard.instances[0].name : '';
  if (source) {
    if (source.kind === 'instance') {
      instanceName = source.name;
    } else {
      for (const inst of dashboard.instances) {
        const match = inst.databases.some((db) => db.kind === source.kind && db.id === source.id);
        if (match) {
          instanceName = inst.name;
        }
      }
    }
  }
  instanceSelect.value = instanceName;
  fillQuerySources(instanceName, source);
  showView('view-query');
}

function fillQuerySources(instanceName, wanted) {
  const select = $('#query-source');
  select.innerHTML = '';
  queryState.sources = buildSourceOptionsFor(instanceName);
  let selected = queryState.sources.length ? queryState.sources.length - 1 : 0;
  for (const [index, source] of queryState.sources.entries()) {
    const option = el('option', { value: String(index), text: source.label });
    if (wanted && sourceMatches(source.value, wanted)) {
      selected = index;
    }
    select.appendChild(option);
  }
  select.value = String(selected);
  onQuerySourceChange();
}

function sourceMatches(candidate, wanted) {
  return candidate.kind === wanted.kind && candidate.id === wanted.id && candidate.name === wanted.name;
}

function onQuerySourceChange() {
  const index = Number($('#query-source').value) || 0;
  queryState.source = queryState.sources[index] ? queryState.sources[index].value : null;
  queryState.tables = [];
  queryState.activeTable = null;
  queryState.offset = 0;
  queryState.hasMore = false;
  clearQueryData();
  loadQueryTables();
}

function clearQueryData() {
  $('#query-data-table').innerHTML = '';
  $('#query-data-title').textContent = QUERY_DATA_HINT;
  $('#query-result-table').innerHTML = '';
  $('#query-sql-status').textContent = '';
}

async function loadQueryTables() {
  const list = $('#query-tables');
  list.innerHTML = '';
  if (!queryState.source) {
    return;
  }
  list.appendChild(el('p', { class: 'hint', text: 'Carregando tabelas...' }));
  try {
    const tables = await apiPost('/api/query/tables', { source: queryState.source });
    queryState.tables = tables;
    renderQueryTables();
  } catch (e) {
    list.innerHTML = '';
    list.appendChild(el('p', { class: 'error', text: e.message }));
  }
}

function renderQueryTables() {
  const list = $('#query-tables');
  list.innerHTML = '';
  const query = ($('#query-table-filter').value || '').trim().toLowerCase();
  const kindLabels = { table: 'tb', view: 'vw', matview: 'mv' };
  let shown = 0;
  for (const t of queryState.tables) {
    const haystack = `${t.schema}.${t.name}`.toLowerCase();
    if (query && !haystack.includes(query)) {
      continue;
    }
    shown += 1;
    const active = queryState.activeTable
      && queryState.activeTable.schema === t.schema
      && queryState.activeTable.name === t.name;
    list.appendChild(el('div', {
      class: 'query-table-item' + (active ? ' active' : ''),
      onclick: () => selectQueryTable(t),
    }, [
      el('span', { class: 'qt-kind', text: kindLabels[t.kind] || '? ' }),
      el('span', { class: 'qt-name', text: `${t.schema}.${t.name}` }),
      el('span', { class: 'qt-meta', text: formatBytes(t.size_bytes) }),
    ]));
  }
  if (!shown) {
    list.appendChild(el('p', { class: 'hint', text: 'Nenhuma tabela.' }));
  }
}

function selectQueryTable(t) {
  queryState.activeTable = t;
  queryState.offset = 0;
  queryState.hasMore = false;
  switchQueryTab('data');
  renderQueryTables();
  $('#query-data-table').innerHTML = '';
  $('#query-data-title').textContent = `${t.schema}.${t.name} — carregando...`;
  fetchTablePage(false);
}

async function fetchTablePage(append) {
  const t = queryState.activeTable;
  if (!t || queryState.loadingMore) {
    return;
  }
  queryState.loadingMore = true;
  try {
    const data = await apiPost('/api/query/data', {
      source: queryState.source,
      schema: t.schema,
      table: t.name,
      offset: queryState.offset,
    });
    queryState.offset += data.rows.length;
    queryState.hasMore = data.rows.length >= QUERY_PAGE_SIZE;
    appendQueryGridRows($('#query-data-table'), data);
    $('#query-data-title').textContent =
      `${t.schema}.${t.name} — ${queryState.offset} linha(s) carregada(s) · ~${t.rows_estimate} estimada(s)`;
  } catch (e) {
    setStatus(e.message);
  } finally {
    queryState.loadingMore = false;
  }
}

function appendQueryGridRows(grid, data) {
  let body = grid.querySelector('tbody');
  if (!body) {
    const headRow = el('tr');
    for (const column of data.columns) {
      headRow.appendChild(el('th', { text: column }));
    }
    grid.appendChild(el('thead', {}, [headRow]));
    body = el('tbody');
    grid.appendChild(body);
  }
  for (const row of data.rows) {
    const tr = el('tr');
    for (const value of row) {
      if (value === null || value === undefined) {
        tr.appendChild(el('td', { class: 'null', text: 'NULL' }));
      } else {
        tr.appendChild(el('td', { text: value }));
      }
    }
    body.appendChild(tr);
  }
}

function switchQueryTab(tab) {
  const isData = tab === 'data';
  $('#query-data-pane').classList.toggle('hidden', !isData);
  $('#query-sql-pane').classList.toggle('hidden', isData);
  $('#tab-data').classList.toggle('active', isData);
  $('#tab-sql').classList.toggle('active', !isData);
}

async function runQuerySql() {
  const sql = $('#query-sql-input').value;
  if (!sql.trim()) {
    return setStatus('Escreva uma query primeiro.');
  }
  if (!queryState.source) {
    return setStatus('Selecione uma conexão.');
  }
  const readOnly = $('#query-read-only').checked;
  $('#query-sql-status').textContent = 'Executando...';
  try {
    const data = await apiPost('/api/query/run', {
      source: queryState.source,
      sql,
      read_only: readOnly,
    });
    renderSqlResult(data);
  } catch (e) {
    $('#query-sql-status').textContent = e.message;
  }
}

function renderSqlResult(data) {
  const grid = $('#query-result-table');
  const statusEl = $('#query-sql-status');
  grid.innerHTML = '';
  statusEl.innerHTML = '';
  if (data.error) {
    statusEl.appendChild(el('span', { class: 'query-error', text: data.error }));
    return;
  }
  const headRow = el('tr');
  for (const column of data.columns) {
    headRow.appendChild(el('th', { text: column }));
  }
  grid.appendChild(el('thead', {}, [headRow]));
  grid.appendChild(el('tbody'));
  appendQueryGridRows(grid, { columns: data.columns, rows: data.rows });
  let status = `${data.row_count} linha(s) em ${data.duration_ms}ms`;
  if (data.truncated) {
    status += ' · truncado em 500 linhas (refine com LIMIT)';
  }
  if (!data.row_count && !data.columns.length) {
    status = `Comando executado (sem result set). ${status}`;
  }
  statusEl.textContent = status;
}

$('#query-back').addEventListener('click', () => loadDashboard());
$('#query-instance').addEventListener('change', (e) => fillQuerySources(e.target.value, null));
$('#query-source').addEventListener('change', onQuerySourceChange);
$('#query-table-filter').addEventListener('input', renderQueryTables);
$('#tab-data').addEventListener('click', () => switchQueryTab('data'));
$('#tab-sql').addEventListener('click', () => switchQueryTab('sql'));
$('#query-run').addEventListener('click', runQuerySql);
$('#query-sql-input').addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    e.preventDefault();
    runQuerySql();
  }
});
$('#query-data-wrap').addEventListener('scroll', () => {
  const wrap = $('#query-data-wrap');
  const nearBottom = wrap.scrollTop + wrap.clientHeight >= wrap.scrollHeight - 60;
  if (nearBottom && queryState.hasMore && !queryState.loadingMore) {
    fetchTablePage(true);
  }
});

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

function startOperation(opId) {
  openModal(`
    <h2>Operação em andamento</h2>
    <pre id="op-logs" class="logs"></pre>
    <div id="op-result"></div>
  `);

  let finished = false;

  function stopPoll() {
    opPollTimers.delete(timer);
    clearInterval(timer);
  }

  async function poll() {
    const logsEl = $('#op-logs');
    if (!logsEl) {
      stopPoll();
      return;
    }
    let data;
    try {
      data = await apiGet('/api/operations/' + opId);
    } catch (e) {
      stopPoll();
      const resultEl = $('#op-result');
      if (resultEl) {
        resultEl.innerHTML = `<span class="error">${escapeHtml(e.message)}</span>`;
      }
      return;
    }
    logsEl.textContent = (data.logs || []).join('\n');
    logsEl.scrollTop = logsEl.scrollHeight;

    if (data.status === 'done' && !finished) {
      finished = true;
      stopPoll();
      renderOperationResult(data.result);
      try { apiDelete('/api/operations/' + opId); } catch { /* ignore */ }
      loadDashboard(false);
    }
  }

  const timer = setInterval(poll, 800);
  opPollTimers.add(timer);
  poll();
}

function renderOperationResult(result) {
  const box = $('#op-result');
  if (!result) {
    box.innerHTML = '<p class="hint">Operação concluída.</p>';
    return;
  }
  if (!result.ok) {
    box.innerHTML = `<div class="result-box"><span class="error">${escapeHtml(result.error)}</span></div>`;
    return;
  }
  const v = result.value;
  let html = '<div class="result-box">';
  switch (v.type) {
    case 'provision':
      html += `<p><strong>${escapeHtml(v.database_name)}</strong> provisionado.</p>`;
      html += `<p class="hint">Role owner: ${escapeHtml(v.role_name)}</p>`;
      html += `<p class="hint">Connection string do banco:</p><div class="cs">${escapeHtml(v.connection_string)}</div>`;
      if (v.extra_username) {
        html += `<p class="hint">Usuário extra: ${escapeHtml(v.extra_username)}</p>`;
        html += `<div class="cs">${escapeHtml(v.extra_connection_string || '')}</div>`;
      }
      html += '<div class="form-actions"><button id="op-copy" class="primary">Copiar conexão do banco</button></div>';
      break;
    case 'migrate':
      html += `<p><strong>${escapeHtml(v.database_name)}</strong> migrado para ${escapeHtml(v.instance_name)}.</p>`;
      html += `<div class="cs">${escapeHtml(v.connection_string)}</div>`;
      html += '<div class="form-actions"><button id="op-copy" class="primary">Copiar conexão</button></div>';
      break;
    case 'backup':
      html += `<p>Backup salvo em:</p><div class="cs">${escapeHtml(v.file_path)}</div>`;
      html += `<p class="hint">${v.database_names.join(', ')}</p>`;
      break;
    case 'restore':
      if (v.restored.length) {
        html += `<p><strong>Restaurados:</strong> ${escapeHtml(v.restored.join(', '))}</p>`;
      }
      if (v.skipped.length) {
        html += `<p class="hint">Pulados (já existiam): ${escapeHtml(v.skipped.join(', '))}</p>`;
      }
      if (!v.restored.length && !v.skipped.length) {
        html += '<p>Nada a restaurar.</p>';
      }
      break;
    default:
      html += '<p>Operação concluída.</p>';
  }
  html += '</div>';
  box.innerHTML = html;

  const copyBtn = $('#op-copy');
  if (copyBtn) {
    const csToCopy = v.connection_string || '';
    copyBtn.addEventListener('click', () => copyText(csToCopy));
  }
}

// ---------------------------------------------------------------------------
// Global wiring
// ---------------------------------------------------------------------------

$('#modal-close').addEventListener('click', closeModal);
$('#modal-overlay').addEventListener('click', (e) => {
  if (e.target === $('#modal-overlay')) {
    closeModal();
  }
});

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('#modal-overlay').classList.contains('hidden')) {
    closeModal();
  }
});

boot();
