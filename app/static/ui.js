// ============================================================
//  tgState 通用前端工具：Toast / Modal / 复制 / 主题 / 退出
//  所有不可信文本一律走 textContent 或 escapeHtml，杜绝 XSS。
// ============================================================

const escapeHtml = (v) => String(v == null ? '' : v).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
}[c]));

const ICONS = {
    ok: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg>',
    err: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"></circle><line x1="12" y1="8" x2="12" y2="12"></line><line x1="12" y1="16" x2="12.01" y2="16"></line></svg>',
    spin: '<svg class="spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 12a9 9 0 1 1-6.2-8.5"></path></svg>',
    sun: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"></circle><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"></path></svg>',
    moon: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.8A9 9 0 1 1 11.2 3 7 7 0 0 0 21 12.8z"></path></svg>',
};

const Toast = {
    wrap() {
        let w = document.getElementById('toast-wrap');
        if (!w) { w = document.createElement('div'); w.id = 'toast-wrap'; document.body.appendChild(w); }
        return w;
    },
    show(message, type = 'success') {
        const t = document.createElement('div');
        t.className = 'toast toast-' + type;
        t.innerHTML = type === 'error' ? ICONS.err : ICONS.ok; // 受信常量
        const span = document.createElement('span');
        span.textContent = message;                            // 不可信文本
        t.appendChild(span);
        this.wrap().appendChild(t);
        requestAnimationFrame(() => t.classList.add('show'));
        setTimeout(() => { t.classList.remove('show'); setTimeout(() => t.remove(), 280); }, 3000);
    },
};

const Modal = {
    ensure() {
        if (this.mask) return;
        const mask = document.createElement('div');
        mask.className = 'modal-mask';
        mask.innerHTML = `
            <div class="modal-card" role="dialog" aria-modal="true">
                <h3 id="m-title"></h3>
                <p id="m-msg"></p>
                <div id="m-field" class="field hidden"><input id="m-input" class="input"></div>
                <div class="modal-actions">
                    <button id="m-cancel" class="btn btn-secondary" type="button">取消</button>
                    <button id="m-ok" class="btn btn-primary" type="button">确定</button>
                </div>
            </div>`;
        document.body.appendChild(mask);
        this.mask = mask;
        this.card = mask.querySelector('.modal-card');
        this.titleEl = mask.querySelector('#m-title');
        this.msgEl = mask.querySelector('#m-msg');
        this.fieldEl = mask.querySelector('#m-field');
        this.inputEl = mask.querySelector('#m-input');
        this.okEl = mask.querySelector('#m-ok');
        this.cancelEl = mask.querySelector('#m-cancel');
    },
    open(opts) {
        this.ensure();
        this.titleEl.textContent = opts.title || '';
        this.msgEl.textContent = opts.message || '';
        this.msgEl.style.display = opts.message ? '' : 'none';
        const isPrompt = !!opts.prompt;
        this.fieldEl.classList.toggle('hidden', !isPrompt);
        if (isPrompt) {
            this.inputEl.type = opts.inputType || 'text';
            this.inputEl.placeholder = opts.placeholder || '';
            this.inputEl.value = opts.value || '';
        }
        this.okEl.textContent = opts.okText || '确定';
        this.okEl.className = 'btn ' + (opts.danger ? 'btn-danger' : 'btn-primary');
        this.mask.style.display = 'flex';
        this.mask.offsetHeight; // reflow
        this.mask.style.opacity = '1';
        this.card.style.transform = 'scale(1)';
        if (isPrompt) setTimeout(() => this.inputEl.focus(), 60);

        return new Promise((resolve) => {
            const done = (val) => {
                this.mask.style.opacity = '0';
                this.card.style.transform = 'scale(.96)';
                setTimeout(() => { this.mask.style.display = 'none'; }, 200);
                this.okEl.onclick = this.cancelEl.onclick = this.mask.onclick = this.inputEl.onkeydown = null;
                resolve(val);
            };
            this.okEl.onclick = () => done(isPrompt ? this.inputEl.value : true);
            this.cancelEl.onclick = () => done(isPrompt ? null : false);
            this.mask.onclick = (e) => { if (e.target === this.mask) done(isPrompt ? null : false); };
            if (isPrompt) this.inputEl.onkeydown = (e) => { if (e.key === 'Enter') done(this.inputEl.value); };
        });
    },
    confirm(title, message, opts = {}) {
        return this.open({ title, message, danger: opts.danger, okText: opts.okText });
    },
    prompt(title, message, opts = {}) {
        return this.open({
            title, message, prompt: true,
            placeholder: opts.placeholder, value: opts.value,
            inputType: opts.inputType, okText: opts.okText,
        });
    },
};

const Utils = {
    async copy(text) {
        if (text && text.startsWith('/')) text = window.location.origin + text;
        try {
            if (navigator.clipboard && navigator.clipboard.writeText) {
                await navigator.clipboard.writeText(text);
                Toast.show('已复制到剪贴板');
                return true;
            }
        } catch (e) { /* fall through */ }
        try {
            const ta = document.createElement('textarea');
            ta.value = text;
            ta.style.position = 'fixed';
            ta.style.opacity = '0';
            ta.setAttribute('readonly', '');
            document.body.appendChild(ta);
            ta.focus();
            ta.select();
            const ok = document.execCommand('copy');
            ta.remove();
            if (ok) { Toast.show('已复制到剪贴板'); return true; }
        } catch (e) { /* fall through */ }
        Toast.show('复制失败，请手动复制', 'error');
        return false;
    },
    setLoading(btn, on) {
        if (!btn) return;
        if (on) {
            btn.dataset.txt = btn.innerHTML;
            btn.innerHTML = ICONS.spin + ' 处理中';
            btn.classList.add('loading');
            btn.disabled = true;
        } else {
            btn.innerHTML = btn.dataset.txt || btn.innerHTML;
            btn.classList.remove('loading');
            btn.disabled = false;
        }
    },
};

const Theme = {
    init() {
        const pref = localStorage.getItem('tgstate_theme_pref');
        this.apply(pref === 'dark' ? 'dark' : 'light');
    },
    apply(mode) {
        const isDark = mode === 'dark';
        document.documentElement.setAttribute('data-theme', isDark ? 'dark' : 'light');
        // 图标 / 文案 / aria 都反映“再点一下会切到的目标模式”。
        document.querySelectorAll('.theme-toggle-btn').forEach((b) => {
            const ic = b.querySelector('.theme-ic');
            if (ic) ic.innerHTML = isDark ? ICONS.sun : ICONS.moon;
            const label = b.querySelector('.theme-label');
            if (label) label.textContent = isDark ? '浅色' : '深色';
            b.setAttribute('aria-label', isDark ? '切换到浅色' : '切换到深色');
            b.setAttribute('title', isDark ? '切换到浅色' : '切换到深色');
        });
    },
    toggle() {
        const cur = localStorage.getItem('tgstate_theme_pref') === 'dark' ? 'dark' : 'light';
        const next = cur === 'dark' ? 'light' : 'dark';
        localStorage.setItem('tgstate_theme_pref', next);
        this.apply(next);
        Toast.show(next === 'dark' ? '已切换深色' : '已切换浅色');
    },
};

const Auth = {
    async logout() {
        const ok = await Modal.confirm('退出登录', '确定退出当前账号吗？', { danger: true, okText: '退出' });
        if (!ok) return;
        try {
            const res = await fetch('/api/auth/logout', { method: 'POST', credentials: 'include' });
            if (res.ok) window.location.replace('/login');
            else Toast.show('退出失败，请重试', 'error');
        } catch (e) {
            Toast.show('网络错误', 'error');
        }
    },
};

document.addEventListener('DOMContentLoaded', () => {
    Theme.init();
    document.querySelectorAll('.theme-toggle-btn').forEach((b) =>
        b.addEventListener('click', (e) => { e.preventDefault(); Theme.toggle(); }));
    document.querySelectorAll('.js-logout').forEach((b) =>
        b.addEventListener('click', (e) => { e.preventDefault(); Auth.logout(); }));
});

window.Toast = Toast;
window.Modal = Modal;
window.Utils = Utils;
window.Theme = Theme;
window.Auth = Auth;
window.escapeHtml = escapeHtml;
