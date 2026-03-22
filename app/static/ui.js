// Toast Notification
const Toast = {
    show(message, type = 'success') {
        const container = document.getElementById('toast-container') || this.createContainer();
        
        const toast = document.createElement('div');
        toast.className = `toast toast-${type}`;
        toast.style.cssText = `
            background: var(--bg-surface); color: var(--text-primary);
            padding: 12px 24px; border-radius: 8px; margin-top: 12px;
            box-shadow: 0 4px 12px rgba(0,0,0,0.15); font-size: 14px;
            display: flex; align-items: center; gap: 8px;
            transform: translateY(-20px); opacity: 0; transition: all 0.3s;
            border-left: 4px solid ${type === 'error' ? 'var(--danger-color)' : 'var(--success-color)'};
        `;
        
        // Icon
        const icon = type === 'error' 
            ? '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="var(--danger-color)" stroke-width="2"><circle cx="12" cy="12" r="10"></circle><line x1="12" y1="8" x2="12" y2="12"></line><line x1="12" y1="16" x2="12.01" y2="16"></line></svg>'
            : '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="var(--success-color)" stroke-width="2"><polyline points="20 6 9 17 4 12"></polyline></svg>';
            
        toast.innerHTML = `${icon}<span>${message}</span>`;
        
        container.appendChild(toast);
        
        // Animate in
        requestAnimationFrame(() => {
            toast.style.transform = 'translateY(0)';
            toast.style.opacity = '1';
        });
        
        // Auto remove
        setTimeout(() => {
            toast.style.opacity = '0';
            toast.style.transform = 'translateY(-20px)';
            setTimeout(() => toast.remove(), 300);
        }, 3000);
    },
    
    createContainer() {
        const div = document.createElement('div');
        div.id = 'toast-container';
        div.style.cssText = `
            position: fixed; top: 20px; left: 50%; transform: translateX(-50%);
            z-index: 2100; display: flex; flex-direction: column; align-items: center;
        `;
        document.body.appendChild(div);
        return div;
    }
};

// Modal System
const Modal = {
    init() {
        if (document.getElementById('app-modal-overlay')) return;
        
        const overlay = document.createElement('div');
        overlay.id = 'app-modal-overlay';
        overlay.style.cssText = `
            position: fixed; top: 0; left: 0; right: 0; bottom: 0;
            background: rgba(0, 0, 0, 0.5); z-index: 2000;
            display: none; align-items: center; justify-content: center;
            opacity: 0; transition: opacity 0.2s;
        `;
        
        const card = document.createElement('div');
        card.id = 'app-modal-card';
        card.style.cssText = `
            background: var(--bg-surface); width: 90%; max-width: 320px;
            border-radius: var(--radius-lg); padding: 24px;
            box-shadow: var(--shadow-md); transform: scale(0.95);
            transition: transform 0.2s;
        `;
        
        card.innerHTML = `
            <h3 id="modal-title" style="font-size: 18px; font-weight: 600; margin-bottom: 12px;"></h3>
            <p id="modal-msg" style="color: var(--text-secondary); font-size: 14px; margin-bottom: 24px; line-height: 1.5;"></p>
            <div style="display: flex; gap: 12px; justify-content: flex-end;">
                <button id="modal-cancel" class="btn btn-secondary" style="flex: 1;">取消</button>
                <button id="modal-confirm" class="btn btn-primary" style="flex: 1;">确定</button>
            </div>
        `;
        
        overlay.appendChild(card);
        document.body.appendChild(overlay);
        
        this.overlay = overlay;
        this.card = card;
        this.titleEl = card.querySelector('#modal-title');
        this.msgEl = card.querySelector('#modal-msg');
        this.cancelBtn = card.querySelector('#modal-cancel');
        this.confirmBtn = card.querySelector('#modal-confirm');
    },
    
    confirm(title, message) {
        this.init();
        return new Promise((resolve) => {
            this.titleEl.textContent = title;
            this.msgEl.textContent = message;
            
            this.overlay.style.display = 'flex';
            // Force reflow
            this.overlay.offsetHeight;
            this.overlay.style.opacity = '1';
            this.card.style.transform = 'scale(1)';
            
            const close = (result) => {
                this.overlay.style.opacity = '0';
                this.card.style.transform = 'scale(0.95)';
                setTimeout(() => {
                    this.overlay.style.display = 'none';
                    resolve(result);
                }, 200);
            };
            
            this.confirmBtn.onclick = () => close(true);
            this.cancelBtn.onclick = () => close(false);
            this.overlay.onclick = (e) => {
                if (e.target === this.overlay) close(false);
            };
        });
    }
};

// 通用工具
const Utils = {
    async copy(text) {
        // 如果 text 是相对路径，自动转换为完整 URL
        if (text.startsWith('/')) {
            text = window.location.origin + text;
        }

        const successToast = () => Toast.show('已复制到剪贴板');
        const failToast = () => Toast.show('复制失败，请手动复制', 'error');

        // 优先使用 navigator.clipboard
        if (navigator.clipboard && navigator.clipboard.writeText) {
            try {
                await navigator.clipboard.writeText(text);
                successToast();
                return true;
            } catch (err) {
                console.warn('Clipboard API failed, trying fallback...', err);
            }
        }

        // Fallback: document.execCommand('copy')
        try {
            const textarea = document.createElement('textarea');
            textarea.value = text;
            // 必须可见才能被 select，使用 fixed 移出可视区但保持渲染
            textarea.style.position = 'fixed';
            textarea.style.left = '0';
            textarea.style.top = '0';
            textarea.style.opacity = '0.01';
            textarea.style.pointerEvents = 'none';
            textarea.setAttribute('readonly', ''); // 防止软键盘弹出
            
            document.body.appendChild(textarea);
            textarea.focus();
            textarea.select();
            
            const successful = document.execCommand('copy');
            document.body.removeChild(textarea);
            
            if (successful) {
                successToast();
                return true;
            }
        } catch (fallbackErr) {
            console.error('Fallback copy failed:', fallbackErr);
        }
        
        // Final fallback: Modal with text
        try {
            const modal = document.createElement('div');
            modal.style.cssText = `
                position: fixed; top: 0; left: 0; right: 0; bottom: 0;
                background: rgba(0,0,0,0.5); z-index: 9999;
                display: flex; align-items: center; justify-content: center;
            `;
            const content = document.createElement('div');
            content.style.cssText = `
                background: var(--bg-surface); padding: 24px; border-radius: 12px;
                width: 90%; max-width: 320px; text-align: center;
                box-shadow: 0 4px 12px rgba(0,0,0,0.15);
            `;
            content.innerHTML = `
                <p style="margin-bottom: 12px; font-weight: 500; color: var(--text-primary);">复制失败，请长按手动复制：</p>
                <div style="background: var(--bg-body); padding: 8px; border-radius: 6px; margin-bottom: 16px; border: 1px solid var(--border-color);">
                    <div style="word-break: break-all; font-family: monospace; font-size: 13px; color: var(--text-primary); user-select: text;">${text}</div>
                </div>
                <button class="btn btn-primary" style="width: 100%;">关闭</button>
            `;
            modal.appendChild(content);
            document.body.appendChild(modal);
            
            const closeBtn = content.querySelector('button');
            const close = () => modal.remove();
            
            closeBtn.onclick = close;
            modal.onclick = (e) => { if(e.target === modal) close(); };
        } catch (e) {}
        
        return false;
    },
    
    setLoading(btn, isLoading) {
        if (!btn) return;
        if (isLoading) {
            btn.dataset.originalText = btn.innerHTML;
            btn.innerHTML = `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="spin"><path d="M21 12a9 9 0 1 1-6.219-8.56"></path></svg> 处理中...`;
            btn.classList.add('loading');
            btn.disabled = true;
        } else {
            btn.innerHTML = btn.dataset.originalText || btn.innerHTML;
            btn.classList.remove('loading');
            btn.disabled = false;
        }
    }
};

// 复制文件链接的辅助函数
window.copyLink = (shortId, fileId, filename) => {
    // 回滚：只生成 /d/{id}，不带文件名/slug
    const id = (shortId && shortId !== 'None' && shortId !== '') ? shortId : fileId;
    const path = `/d/${id}`;
    Utils.copy(path);
};

// 认证系统
const Auth = {
    async logout() {
        const confirmed = await Modal.confirm('退出登录', '确定要退出当前账号吗？');
        if (!confirmed) return;
        
        try {
            const res = await fetch('/api/auth/logout', {
                method: 'POST',
                // 确保携带凭证（Cookies）
                credentials: 'include' 
            });
            
            if (res.ok) {
                // 清理可能存在的本地状态
                // localStorage.removeItem('some_key'); 
                
                // 强制跳转到登录页，并替换历史记录，防止后退
                window.location.replace('/login');
            } else {
                Toast.show('退出失败，请刷新重试', 'error');
            }
        } catch (e) {
            console.error(e);
            Toast.show('网络错误', 'error');
        }
    }
};

// Theme System
const Theme = {
    init() {
        const pref = localStorage.getItem('tgstate_theme_pref') || 'auto';
        this.apply(pref);
    },
    
    apply(mode) {
        let theme = mode;
        if (mode === 'auto') {
            theme = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
        }
        document.documentElement.setAttribute('data-theme', theme);
        
        // Update Toggle Button Text/Icon if needed (optional)
        const label = document.querySelector('.theme-label');
        if (label) {
            label.textContent = mode === 'auto' ? '跟随系统' : (mode === 'dark' ? '深色模式' : '浅色模式');
        }
    },
    
    cycle() {
        const current = localStorage.getItem('tgstate_theme_pref') || 'auto';
        const next = current === 'auto' ? 'light' : (current === 'light' ? 'dark' : 'auto');
        localStorage.setItem('tgstate_theme_pref', next);
        this.apply(next);
        
        const modeNames = { 'auto': '跟随系统', 'light': '浅色模式', 'dark': '深色模式' };
        Toast.show(`已切换到${modeNames[next]}`);
    }
};

// 初始化
document.addEventListener('DOMContentLoaded', () => {
    Theme.init();
    
    // 绑定主题切换按钮点击事件
    document.querySelectorAll('.theme-toggle-btn').forEach(btn => {
        btn.addEventListener('click', (e) => {
            e.preventDefault();
            Theme.cycle();
        });
    });

    // 侧边栏/移动端菜单切换

    const toggleBtn = document.querySelector('.menu-toggle');
    const sidebar = document.querySelector('.sidebar');
    const overlay = document.querySelector('.sidebar-overlay');
    
    if (toggleBtn && sidebar) {
        toggleBtn.addEventListener('click', () => {
            sidebar.classList.toggle('active');
            if (overlay) overlay.classList.toggle('active');
        });
    }
    
    if (overlay) {
        overlay.addEventListener('click', () => {
            sidebar.classList.remove('active');
            overlay.classList.remove('active');
        });
    }
});

// Expose to window for inline onclick handlers
window.Theme = Theme;
window.Auth = Auth;
window.Utils = Utils;
window.Modal = Modal;
window.Toast = Toast;
