// ============================================================
//  tgState 文件管理 / 图床 页面交互
//  所有来自服务端 / Telegram 的值进入 innerHTML 前都经 escapeHtml。
// ============================================================

// 不要再声明顶层 const escapeHtml：ui.js 已导出 window.escapeHtml。
// 经典脚本共享全局词法作用域，重复 const 会让本文件整段解析失败，
// 导致复制/密码/删除按钮全部无响应。

document.addEventListener('DOMContentLoaded', () => {
    const Toast = window.Toast, Modal = window.Modal, Utils = window.Utils;
    const escapeHtml = window.escapeHtml || ((v) => String(v == null ? '' : v).replace(/[&<>"']/g, (c) => ({
        '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
    }[c])));
    if (!Toast || !Modal || !Utils) {
        console.error('ui.js not loaded');
        return;
    }

    const grid = document.getElementById('image-grid');
    const isGallery = !!grid;
    const listBody = document.getElementById('file-list-disk');
    const zone = document.getElementById('upload-zone');
    const picker = document.getElementById('file-picker');
    const progZone = document.getElementById('prog-zone');
    const doneZone = document.getElementById('done-zone');

    // 受信 SVG 图标常量（用于 SSE 动态渲染的表格行）
    const ICO = {
        file: '<svg class="ficon" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"></path><polyline points="13 2 13 9 20 9"></polyline></svg>',
        download: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>',
        copy: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>',
        lock: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2"></rect><path d="M7 11V7a5 5 0 0 1 10 0v4"></path></svg>',
        lockBadge: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2"></rect><path d="M7 11V7a5 5 0 0 1 10 0v4"></path></svg>',
        trash: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path></svg>',
    };

    const fmtDate = (v) => {
        if (!v) return '';
        const d = new Date(v);
        if (!isNaN(d.getTime())) return d.toISOString().split('T')[0];
        return String(v).split(' ')[0].split('T')[0];
    };

    function rowDomId(fileId) {
        return 'file-item-' + String(fileId || '').split(':').join('-');
    }

    function removeItem(fileId) {
        if (!fileId) return;
        let el = document.getElementById(rowDomId(fileId));
        if (!el) {
            const id = String(fileId);
            el = document.querySelector('.file-item[data-file-id="' + CSS.escape(id) + '"]')
                || document.querySelector('.file-item[data-short-id="' + CSS.escape(id) + '"]');
        }
        if (el) el.remove();
        if (typeof updateBatch === 'function') updateBatch();
    }

    function apiIdFor(itemOrId) {
        if (!itemOrId) return '';
        if (typeof itemOrId === 'string') return itemOrId;
        // Prefer composite Telegram file_id for delete (needs msg:file).
        // short_id is fine for share-password (DB accepts either).
        return itemOrId.dataset.fileId || itemOrId.dataset.shortId || '';
    }

    function absoluteFileUrl(item) {
        let url = (item && item.dataset.fileUrl) || '';
        if (!url && item && item.dataset.shortId) url = '/d/' + item.dataset.shortId;
        if (!url && item && item.dataset.fileId) url = '/d/' + item.dataset.fileId;
        if (url.startsWith('/')) url = window.location.origin + url;
        return url;
    }

    // ---- SSE 实时新增（仅文件管理页，事件来自 Bot 摄取，含真实 composite file_id） ----
    function addNewFileElement(file) {
        if (!listBody) return;
        const ph = listBody.querySelector('td[colspan]');
        if (ph) { const tr = ph.closest('tr'); if (tr) tr.remove(); }

        const rawId = file.file_id || '';
        const fid = escapeHtml(rawId);
        const fn = escapeHtml(file.filename);
        const url = escapeHtml('/d/' + (file.short_id || file.file_id));
        const size = escapeHtml(((file.filesize || 0) / 1048576).toFixed(2) + ' MB');
        const date = escapeHtml(fmtDate(file.upload_date));
        const locked = !!file.has_password;

        const tr = document.createElement('tr');
        tr.className = 'file-item';
        tr.id = rowDomId(rawId);
        tr.dataset.fileId = rawId;
        tr.dataset.shortId = file.short_id || '';
        tr.dataset.filename = file.filename || '';
        tr.dataset.fileUrl = '/d/' + (file.short_id || file.file_id);
        tr.dataset.hasPassword = locked ? 'true' : 'false';
        tr.innerHTML = `
            <td><input type="checkbox" class="checkbox file-checkbox" data-file-id="${fid}"></td>
            <td><div class="fname">${ICO.file}<span class="ftext">${fn}</span>
                <span class="badge badge-lock js-lock-badge" title="已设访问密码" ${locked ? '' : 'style="display:none;"'}>${ICO.lockBadge}</span></div></td>
            <td class="text-sm muted">${size}</td>
            <td class="text-sm muted">${date}</td>
            <td class="col-actions"><div class="row-actions">
                <a href="${url}" class="btn btn-ghost btn-icon btn-sm" title="下载">${ICO.download}</a>
                <button type="button" class="btn btn-ghost btn-icon btn-sm copy-link-btn" title="复制链接">${ICO.copy}</button>
                <button type="button" class="btn btn-ghost btn-icon btn-sm js-lock" title="分享密码">${ICO.lock}</button>
                <button type="button" class="btn btn-ghost btn-icon btn-sm js-delete" data-file-id="${fid}" title="删除">${ICO.trash}</button>
            </div></td>`;
        listBody.prepend(tr);
    }

    // ---- 删除 ----
    async function deleteFile(fileId, filename) {
        if (!fileId) { Toast.show('缺少文件标识', 'error'); return; }
        const ok = await Modal.confirm('删除文件', '确定删除「' + (filename || fileId) + '」？此操作不可恢复。', { danger: true, okText: '删除' });
        if (!ok) return;
        Toast.show('正在删除…');
        try {
            const res = await fetch('/api/files/' + encodeURIComponent(fileId), {
                method: 'DELETE',
                credentials: 'same-origin'
            });
            if (res.status === 401) {
                Toast.show('请先登录', 'error');
                setTimeout(() => { location.href = '/pwd.html'; }, 500);
                return;
            }
            const data = await res.json().catch(() => ({}));
            if (res.ok && data.status === 'ok') {
                removeItem(fileId);
                // also try remove by short id/file id variants already handled in removeItem
                Toast.show(data.message || '已删除');
            } else {
                Toast.show((data.detail && data.detail.message) || data.message || ('删除失败(' + res.status + ')'), 'error');
            }
        } catch (e) { Toast.show('网络错误', 'error'); }
    }

 // ---- 分享密码 ----
    async function setSharePassword(item) {
        const fid = apiIdFor(item);
        if (!fid) { Toast.show('缺少文件标识', 'error'); return; }
        const has = item.dataset.hasPassword === 'true';
        const val = await Modal.prompt(
            has ? '修改分享密码' : '设置分享密码',
            has ? '输入新密码；留空并确定可清除密码。' : '设置后，访客打开分享链接需输入密码。',
            { placeholder: has ? '留空 = 清除密码' : '输入分享密码', okText: '保存', inputType: 'password' }
        );
        if (val === null) return;
        try {
            const res = await fetch('/api/files/' + encodeURIComponent(fid) + '/share-password', {
                method: 'POST',
                credentials: 'same-origin',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ password: val }),
            });
            if (res.status === 401) {
                Toast.show('请先登录', 'error');
                setTimeout(() => { location.href = '/pwd.html'; }, 500);
                return;
            }
            const data = await res.json().catch(() => ({}));
            if (res.ok) {
                const nowHas = !!data.has_password;
                item.dataset.hasPassword = nowHas ? 'true' : 'false';
                const badge = item.querySelector('.js-lock-badge');
                if (badge) badge.style.display = nowHas ? '' : 'none';
                Toast.show(nowHas ? '已设置分享密码' : '已清除分享密码');
            } else {
                Toast.show((data.detail && data.detail.message) || data.message || ('设置失败(' + res.status + ')'), 'error');
            }
        } catch (e) { Toast.show('网络错误', 'error'); }
    }

 // ---- 行内操作委托（复制 / 密码 / 删除） ----
    document.addEventListener('click', (e) => {
        const copyBtn = e.target.closest('.copy-link-btn');
        if (copyBtn) {
            e.preventDefault();
            const it = copyBtn.closest('.file-item');
            if (!it) return;
            const url = absoluteFileUrl(it);
            if (!url) { Toast.show('无可用链接', 'error'); return; }
            Utils.copy(url);
            return;
        }
        const lockBtn = e.target.closest('.js-lock');
        if (lockBtn) {
            e.preventDefault();
            const it = lockBtn.closest('.file-item');
            if (it) setSharePassword(it);
            return;
        }
        const delBtn = e.target.closest('.js-delete');
        if (delBtn) {
            e.preventDefault();
            const it = delBtn.closest('.file-item');
            const id = apiIdFor(it) || delBtn.dataset.fileId;
            deleteFile(id, it && it.dataset.filename);
        }
    });

    // ---- 搜索过滤 ----
    const search = document.getElementById('file-search');
    if (search) search.addEventListener('input', () => {
        const term = search.value.toLowerCase();
        document.querySelectorAll('.file-item').forEach((it) => {
            const name = (it.dataset.filename || '').toLowerCase();
            it.style.display = name.includes(term) ? '' : 'none';
        });
    });

    // ---- 批量选择 / 复制 / 删除 ----
    const selectAll = document.getElementById('select-all-checkbox');
    const batchBar = document.getElementById('batch-actions-bar');
    const counter = document.getElementById('selection-counter');
    const batchDelete = document.getElementById('batch-delete-btn');
    const copyLinks = document.getElementById('copy-links-btn');

    const allChecks = () => Array.from(document.querySelectorAll('.file-checkbox'));
    const sel = () => allChecks().filter((c) => c.checked);
    function updateBatch() {
        const n = sel().length;
        if (counter) counter.textContent = n;
        if (batchBar) batchBar.classList.toggle('hidden', n === 0);
        if (selectAll) selectAll.checked = n > 0 && n === allChecks().length;
    }
    document.addEventListener('change', (e) => {
        if (e.target.classList && e.target.classList.contains('file-checkbox')) updateBatch();
    });
    if (selectAll) selectAll.addEventListener('change', () => {
        allChecks().forEach((c) => { c.checked = selectAll.checked; });
        updateBatch();
    });

    document.querySelectorAll('#format-chips .chip').forEach((ch) => ch.addEventListener('click', () => {
        document.querySelectorAll('#format-chips .chip').forEach((c) => c.classList.remove('active'));
        ch.classList.add('active');
    }));

    if (copyLinks) copyLinks.addEventListener('click', () => {
        const items = sel();
        if (!items.length) return;
        const fmtEl = document.querySelector('#format-chips .chip.active');
        const fmt = fmtEl ? fmtEl.dataset.format : 'url';
        const links = items.map((c) => {
            const it = c.closest('.file-item');
            let url = it.dataset.fileUrl || ('/d/' + it.dataset.shortId);
            if (url.startsWith('/')) url = window.location.origin + url;
            const name = it.dataset.filename || '';
            if (fmt === 'markdown') return '![' + name + '](' + url + ')';
            if (fmt === 'html') return '<img src="' + url + '" alt="' + name + '">';
            return url;
        });
        Utils.copy(links.join('\n'));
    });

    if (batchDelete) batchDelete.addEventListener('click', async () => {
        const items = sel();
        if (!items.length) return;
        const ok = await Modal.confirm('批量删除', '确定删除选中的 ' + items.length + ' 个文件吗？', { danger: true, okText: '删除' });
        if (!ok) return;
        const ids = items.map((c) => c.dataset.fileId);
        try {
            const res = await fetch('/api/batch_delete', {
                method: 'POST', headers: { 'Content-Type': 'application/json' },
                credentials: 'same-origin',
                body: JSON.stringify({ file_ids: ids }),
            });
            const data = await res.json().catch(() => ({}));
            (data.deleted || []).forEach(removeItem);
            Toast.show('已删除 ' + (data.deleted || []).length + ' 个文件');
            updateBatch();
        } catch (e) { Toast.show('网络错误', 'error'); }
    });

    // ---- 上传 ----
    if (zone && picker) {
        picker.addEventListener('click', (e) => e.stopPropagation());
        zone.addEventListener('click', () => picker.click());
        zone.addEventListener('dragover', (e) => { e.preventDefault(); zone.classList.add('dragover'); });
        zone.addEventListener('dragleave', () => zone.classList.remove('dragover'));
        zone.addEventListener('drop', (e) => {
            e.preventDefault();
            zone.classList.remove('dragover');
            if (e.dataTransfer.files.length) handleFiles(e.dataTransfer.files);
        });
        picker.addEventListener('change', (e) => { if (e.target.files.length) handleFiles(e.target.files); });
    }

    const queue = [];
    let busy = false;
    function handleFiles(files) {
        for (const f of files) queue.push(f);
        pump();
    }
    function pump() {
        if (busy || !queue.length) return;
        busy = true;
        uploadOne(queue.shift()).then(() => { busy = false; pump(); });
    }
    function uploadOne(file) {
        return new Promise((resolve) => {
            const fd = new FormData();
            fd.append('file', file, file.name);
            const xhr = new XMLHttpRequest();
            xhr.open('POST', '/api/upload', true);
            const pid = 'up-' + Math.random().toString(36).slice(2, 9);
            if (progZone) {
                const row = document.createElement('div');
                row.className = 'up-item';
                row.id = pid;
                row.innerHTML = '<div class="between"><span class="up-name">' + escapeHtml(file.name) +
                    '</span><span class="percent text-xs muted">0%</span></div><div class="up-bar"><i></i></div>';
                progZone.appendChild(row);
            }
            const bar = document.querySelector('#' + pid + ' .up-bar > i');
            const pct = document.querySelector('#' + pid + ' .percent');
            xhr.upload.onprogress = (ev) => {
                if (!ev.total) return;
                const p = Math.floor((ev.loaded / ev.total) * 100);
                if (bar) bar.style.width = p + '%';
                if (pct) pct.textContent = p + '%';
            };
            xhr.onload = () => {
                const row = document.getElementById(pid);
                if (row) row.remove();
                if (xhr.status === 200) {
                    let resp = {};
                    try { resp = JSON.parse(xhr.responseText); } catch (e) { /* ignore */ }
                    Toast.show(file.name + ' 上传成功');
                    if (isGallery) {
                        setTimeout(() => window.location.reload(), 800);
                    } else if (doneZone) {
                        const url = resp.url || ('/d/' + (resp.short_id || ''));
                        const full = window.location.origin + url;
                        const card = document.createElement('div');
                        card.className = 'up-item up-done';
                        card.innerHTML = '<div class="up-name">' + escapeHtml(file.name) + '</div>' +
                            '<a href="' + escapeHtml(url) + '" target="_blank" rel="noopener">' + escapeHtml(full) + '</a>';
                        doneZone.prepend(card);
                    }
                } else {
                    let msg = '上传失败';
                    try { const j = JSON.parse(xhr.responseText); msg = (j.detail && j.detail.message) || j.message || msg; } catch (e) { /* ignore */ }
                    Toast.show(msg, 'error');
                }
                resolve();
            };
            xhr.onerror = () => {
                const row = document.getElementById(pid);
                if (row) row.remove();
                Toast.show('网络错误', 'error');
                resolve();
            };
            xhr.send(fd);
        });
    }

    // ---- SSE 实时更新（仅文件管理页） ----
    if (listBody && !isGallery) {
        let es = null;
        const connect = () => {
            if (es) es.close();
            es = new EventSource('/api/file-updates');
            es.onmessage = (ev) => {
                let m = {};
                try { m = JSON.parse(ev.data); } catch (e) { return; }
                if (m.action === 'delete') { removeItem(m.file_id); updateBatch(); }
                else addNewFileElement(m);
            };
            es.onerror = () => { try { es.close(); } catch (e) { /* ignore */ } setTimeout(connect, 5000); };
        };
        connect();
    }
});
