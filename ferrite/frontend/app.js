let editor = null;
let currentVimMode = null;
let currentRemote = '';
let currentPath = '/';
let currentMode = 'editor';

let openTabs = [];
let activeTabPath = null;
let tabModels = new Map();
let tabDirty = new Map();

let fileHashes = new Map();
let collabSockets = new Map();

const collabUserColors = ['#ec4899', '#8b5cf6', '#10b981', '#f59e0b', '#06b6d4', '#3b82f6'];
const collabUserName = 'User-' + Math.floor(1000 + Math.random() * 9000);
const collabUserColor = collabUserColors[Math.floor(Math.random() * collabUserColors.length)];

let pendingConflictPath = null;

let term = null;
let fitAddon = null;
let termWs = null;
let termManualClose = false;
let termConnecting = false;

let remotesData = [];

let currentUserRole = null;
let authedUsername = null;
let userPermissions = [];
let usersCache = [];
let selectedUsername = null;

window.MonacoEnvironment = {
    getWorkerUrl: function (moduleId, label) {
        return 'data:text/javascript;charset=utf-8,' + encodeURIComponent(
            'self.MonacoEnvironment = { baseUrl: "https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/" };' +
            'importScripts("https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs/base/worker/workerMain.js");'
        );
    }
};

function initMonacoEditor() {
    if (editor) return;

    editor = monaco.editor.create(document.getElementById('monaco-container'), {
        value: '// Select a file from the sidebar to start editing\n',
        language: 'plaintext',
        theme: 'vs-dark',
        automaticLayout: true,
        fontFamily: 'JetBrains Mono, monospace',
        fontSize: 13,
        minimap: { enabled: true },
        scrollBeyondLastLine: false,
    });

    editor.onDidChangeModelContent(function (e) {
        if (activeTabPath && !tabDirty.get(activeTabPath)) {
            tabDirty.set(activeTabPath, true);
            updateStatusBadge('Modified', 'modified');
            renderTabBar();
        }

        if (activeTabPath && e.isFlush !== true) {
            const ws = collabSockets.get(activeTabPath);
            if (ws && ws.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify({
                    type: 'edit',
                    content: editor.getValue()
                }));
            }
        }
    });

    editor.onDidChangeCursorPosition(function (e) {
        const statusPos = document.getElementById('status-cursor-pos');
        if (statusPos) {
            statusPos.textContent = `Ln ${e.position.lineNumber}, Col ${e.position.column}`;
        }

        if (!activeTabPath || !currentGitBranch) {
            const blameBar = document.getElementById('git-blame-bar');
            if (blameBar) blameBar.style.display = 'none';
            return;
        }
        fetchGitBlame(activeTabPath, e.position.lineNumber);
    });

    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, function () {
        saveCurrentFile();
    });

    setupDragAndDrop();

    const keybindSelect = document.getElementById('keybind-select');
    if (keybindSelect && keybindSelect.value === 'vim') {
        toggleVimMode(true);
    }
}

window.addEventListener('DOMContentLoaded', function () {
    loadCachedTheme();
    initApp();
    if (window.monaco) {
        initMonacoEditor();
    } else if (window.require && typeof window.require === 'function') {
        try {
            require.config({
                paths: {
                    'vs': 'https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs'
                }
            });
            require(['vs/editor/editor.main'], function () {
                initMonacoEditor();
            });
        } catch (_) {
            initMonacoEditor();
        }
    } else {
        initMonacoEditor();
    }
});

function loadCachedTheme() {
    const cachedColor = localStorage.getItem('ferrite_theme_color');
    if (cachedColor) {
        applyThemeAccent(cachedColor);
    }
}

function applyThemeAccent(color) {
    if (!color) return;
    document.documentElement.style.setProperty('--accent-color', color);
    document.documentElement.style.setProperty('--accent-white', color);

    const activeModeBtn = document.querySelector('.mode-btn.active');
    if (activeModeBtn) {
        activeModeBtn.style.backgroundColor = color;
    }

    const primaryBtns = document.querySelectorAll('.btn-primary');
    primaryBtns.forEach(btn => {
        btn.style.backgroundColor = color;
    });

    const themeInput = document.getElementById('theme-color-input');
    if (themeInput && themeInput.value !== color) {
        themeInput.value = color;
    }
}

async function loadThemeSettings() {
    loadCachedTheme();
    try {
        const res = await fetch('/api/settings');
        if (res.ok) {
            const settings = await res.json();
            if (settings.accent_color) {
                applyThemeAccent(settings.accent_color);
                localStorage.setItem('ferrite_theme_color', settings.accent_color);
            }
        }
    } catch (_) {}
}

async function saveThemeSettings(color) {
    applyThemeAccent(color);
    localStorage.setItem('ferrite_theme_color', color);
    try {
        await fetch('/api/settings', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ accent_color: color })
        });
    } catch (_) {}
}

let dragCounter = 0;

function setupDragAndDrop() {
    const overlay = document.getElementById('drag-drop-modal-overlay');
    const targetPathSpan = document.getElementById('drag-drop-target-path');

    window.addEventListener('dragenter', function (e) {
        if (e.dataTransfer && e.dataTransfer.types && (e.dataTransfer.types.includes('Files') || e.dataTransfer.types.includes('application/x-moz-file'))) {
            e.preventDefault();
            dragCounter++;
            if (targetPathSpan) targetPathSpan.textContent = currentPath || '/';
            if (overlay) overlay.classList.add('active');
        }
    });

    window.addEventListener('dragover', function (e) {
        if (e.dataTransfer && e.dataTransfer.types && (e.dataTransfer.types.includes('Files') || e.dataTransfer.types.includes('application/x-moz-file'))) {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'copy';
        }
    });

    window.addEventListener('dragleave', function (e) {
        if (e.dataTransfer && e.dataTransfer.types && (e.dataTransfer.types.includes('Files') || e.dataTransfer.types.includes('application/x-moz-file'))) {
            e.preventDefault();
            dragCounter--;
            if (dragCounter <= 0) {
                dragCounter = 0;
                if (overlay) overlay.classList.remove('active');
            }
        }
    });

    window.addEventListener('drop', function (e) {
        if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length > 0) {
            e.preventDefault();
            dragCounter = 0;
            if (overlay) overlay.classList.remove('active');
            uploadFiles(e.dataTransfer.files);
            return;
        }

        dragCounter = 0;
        if (overlay) overlay.classList.remove('active');

        const editorPane = document.querySelector('.editor-pane');
        if (editorPane && editorPane.contains(e.target)) {
            const filePath = e.dataTransfer ? e.dataTransfer.getData('text/plain') : null;
            if (filePath && !filePath.startsWith('blob:')) {
                openFile(filePath);
            }
        }
    });
}

function showToast(message, type = 'info') {
    const container = document.getElementById('toast-container');
    const toast = document.createElement('div');
    toast.className = `toast-item ${type}`;
    toast.textContent = message;

    toast.addEventListener('click', function () {
        toast.remove();
    });

    container.appendChild(toast);
    setTimeout(function () {
        if (toast.parentNode) {
            toast.remove();
        }
    }, 4000);
}

function customConfirm(message, title = 'Confirmation') {
    return new Promise(resolve => {
        const modal = document.getElementById('custom-dialog-modal');
        const titleEl = document.getElementById('custom-dialog-title');
        const bodyEl = document.getElementById('custom-dialog-body');
        const inputEl = document.getElementById('custom-dialog-input');
        const cancelBtn = document.getElementById('custom-dialog-cancel-btn');
        const confirmBtn = document.getElementById('custom-dialog-confirm-btn');

        titleEl.textContent = title;
        bodyEl.textContent = message;
        inputEl.style.display = 'none';
        cancelBtn.style.display = 'block';
        confirmBtn.textContent = 'Confirm';

        modal.style.display = 'flex';

        function cleanup() {
            modal.style.display = 'none';
            cancelBtn.removeEventListener('click', onCancel);
            confirmBtn.removeEventListener('click', onConfirm);
        }

        function onCancel() {
            cleanup();
            resolve(false);
        }

        function onConfirm() {
            cleanup();
            resolve(true);
        }

        cancelBtn.addEventListener('click', onCancel);
        confirmBtn.addEventListener('click', onConfirm);
    });
}

function customPrompt(title, defaultValue = '') {
    return new Promise(resolve => {
        const modal = document.getElementById('custom-dialog-modal');
        const titleEl = document.getElementById('custom-dialog-title');
        const bodyEl = document.getElementById('custom-dialog-body');
        const inputEl = document.getElementById('custom-dialog-input');
        const cancelBtn = document.getElementById('custom-dialog-cancel-btn');
        const confirmBtn = document.getElementById('custom-dialog-confirm-btn');

        titleEl.textContent = title;
        bodyEl.textContent = '';
        inputEl.style.display = 'block';
        inputEl.value = defaultValue;
        cancelBtn.style.display = 'block';
        confirmBtn.textContent = 'Submit';

        modal.style.display = 'flex';
        inputEl.focus();

        function cleanup() {
            modal.style.display = 'none';
            cancelBtn.removeEventListener('click', onCancel);
            confirmBtn.removeEventListener('click', onConfirm);
            inputEl.removeEventListener('keydown', onKeyDown);
        }

        function onCancel() {
            cleanup();
            resolve(null);
        }

        function onConfirm() {
            const val = inputEl.value;
            cleanup();
            resolve(val);
        }

        function onKeyDown(e) {
            if (e.key === 'Enter') {
                onConfirm();
            } else if (e.key === 'Escape') {
                onCancel();
            }
        }

        cancelBtn.addEventListener('click', onCancel);
        confirmBtn.addEventListener('click', onConfirm);
        inputEl.addEventListener('keydown', onKeyDown);
    });
}

function applyRoleUI(role, username, permissions, allowShell) {
    currentUserRole = role;
    authedUsername = username || null;
    userPermissions = permissions || [];
    const isAdmin = !role || role === 'admin';

    const usersSection = document.getElementById('settings-manage-users-section');
    if (usersSection) usersSection.style.display = isAdmin ? '' : 'none';
    const btnAddRemote = document.getElementById('btn-add-remote');
    if (btnAddRemote) btnAddRemote.style.display = isAdmin ? '' : 'none';

    const canUseShell = isAdmin || !!allowShell;
    const btnModeTerminal = document.getElementById('btn-mode-terminal');
    if (btnModeTerminal) btnModeTerminal.style.display = canUseShell ? '' : 'none';
    const btnTermReconnect = document.getElementById('btn-term-reconnect');
    if (btnTermReconnect) btnTermReconnect.style.display = canUseShell ? '' : 'none';
}

// For restricted (non-admin) users, browsing should start inside a folder
// they're actually granted rather than at "/", which they typically can't
// see and which otherwise trips a confusing "auth required" prompt.
function defaultPathForRemote(remoteName) {
    if (!currentUserRole || currentUserRole === 'admin') return '/';
    const grant = userPermissions.find(g => g.remote === remoteName);
    return grant ? grant.path : '/';
}

async function checkAuthStatus() {
    try {
        const res = await fetch('/api/auth/status');
        if (!res.ok) return;
        const data = await res.json();
        if (data.version && data.codename) {
            const brandBadge = document.querySelector('.brand-badge');
            if (brandBadge) {
                brandBadge.textContent = `v${data.version} (${data.codename})`;
                brandBadge.title = `Ferrite v${data.version} "${data.codename}"`;
            }
        }
        if (data.auth_required && !data.authenticated) {
            showLoginModal();
        } else {
            applyRoleUI(data.role, data.username, data.permissions, data.allow_shell);
        }
    } catch (_) {}
}

function showLoginModal() {
    const modal = document.getElementById('login-modal');
    if (modal) modal.style.display = 'flex';
}

function hideLoginModal() {
    const modal = document.getElementById('login-modal');
    if (modal) modal.style.display = 'none';
}

async function handleLogin() {
    const usernameInput = document.getElementById('login-username');
    const passwordInput = document.getElementById('login-password');
    const totpInput = document.getElementById('login-totp');
    if (!usernameInput || !passwordInput) return;

    try {
        const res = await fetch('/api/auth/login', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                username: usernameInput.value.trim(),
                password: passwordInput.value,
                totp_code: totpInput ? totpInput.value.trim() : null
            })
        });

        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Login failed');
        }

        await checkAuthStatus();

        hideLoginModal();
        showToast('Login successful', 'success');
        fetchRemotes();
    } catch (err) {
        showToast(`Login failed: ${err.message}`, 'error');
    }
}

async function initApp() {
    const remoteSelect = document.getElementById('remote-select');
    const pathInput = document.getElementById('path-input');
    const goBtn = document.getElementById('go-btn');
    const saveBtn = document.getElementById('save-btn');
    const btnEditor = document.getElementById('btn-mode-editor');
    const btnTerminal = document.getElementById('btn-mode-terminal');
    const keybindSelect = document.getElementById('keybind-select');

    const themeColorInput = document.getElementById('theme-color-input');
    const btnAddRemote = document.getElementById('btn-add-remote');

    const btnNewFile = document.getElementById('btn-new-file');
    const btnNewFolder = document.getElementById('btn-new-folder');
    const btnRefreshDir = document.getElementById('btn-refresh-dir');

    const loginSubmitBtn = document.getElementById('login-submit-btn');
    if (loginSubmitBtn) {
        loginSubmitBtn.addEventListener('click', handleLogin);
    }
    const loginPasswordInput = document.getElementById('login-password');
    if (loginPasswordInput) {
        loginPasswordInput.addEventListener('keydown', function (e) {
            if (e.key === 'Enter') handleLogin();
        });
    }

    checkAuthStatus();


    const searchInput = document.getElementById('search-input');
    const searchBtn = document.getElementById('search-btn');

    const diffCancelBtn = document.getElementById('diff-cancel-btn');
    const diffOverwriteBtn = document.getElementById('diff-overwrite-btn');

    const remoteModal = document.getElementById('remote-modal');
    const remoteModalCancelBtn = document.getElementById('remote-modal-cancel-btn');
    const remoteModalSaveBtn = document.getElementById('remote-modal-save-btn');
    const remoteFormProtocol = document.getElementById('remote-form-protocol');

    themeColorInput.addEventListener('input', function () {
        document.documentElement.style.setProperty('--accent-color', themeColorInput.value);
        saveThemeSettings(themeColorInput.value);
    });

    btnAddRemote.addEventListener('click', function () {
        openRemoteModal();
    });

    remoteModalCancelBtn.addEventListener('click', function () {
        remoteModal.style.display = 'none';
    });

    remoteFormProtocol.addEventListener('change', function () {
        const proto = remoteFormProtocol.value;
        const s3Fields = document.getElementById('remote-form-s3-fields');
        const portInput = document.getElementById('remote-form-port');

        if (proto === 's3') {
            s3Fields.style.display = 'flex';
            portInput.value = 443;
        } else {
            s3Fields.style.display = 'none';
            if (proto === 'sftp') portInput.value = 22;
            else if (proto === 'ftp') portInput.value = 21;
            else if (proto === 'webdav') portInput.value = 443;
            else if (proto === 'local') portInput.value = 0;
        }
    });

    remoteModalSaveBtn.addEventListener('click', async function () {
        await saveRemoteConfig();
    });

    const settingsManageUsersBtn = document.getElementById('settings-manage-users-btn');
    if (settingsManageUsersBtn) {
        settingsManageUsersBtn.addEventListener('click', function () {
            const settingsModal = document.getElementById('settings-modal');
            if (settingsModal) settingsModal.style.display = 'none';
            openUsersModal();
        });
    }
    const usersModalCloseX = document.getElementById('users-modal-close-x');
    if (usersModalCloseX) usersModalCloseX.addEventListener('click', closeUsersModal);
    const usersModalCancelBtn = document.getElementById('users-modal-cancel-btn');
    if (usersModalCancelBtn) usersModalCancelBtn.addEventListener('click', closeUsersModal);
    const usersNewBtn = document.getElementById('users-new-btn');
    if (usersNewBtn) usersNewBtn.addEventListener('click', () => selectUser(null));
    const usersPermAddBtn = document.getElementById('users-perm-add-btn');
    if (usersPermAddBtn) usersPermAddBtn.addEventListener('click', () => addPermRow());
    const usersCreateBtn = document.getElementById('users-create-btn');
    if (usersCreateBtn) usersCreateBtn.addEventListener('click', createUserSubmit);
    const usersSaveBtn = document.getElementById('users-save-btn');
    if (usersSaveBtn) usersSaveBtn.addEventListener('click', saveUserSubmit);
    const usersDeleteBtn = document.getElementById('users-delete-btn');
    if (usersDeleteBtn) usersDeleteBtn.addEventListener('click', deleteUserSubmit);

    remoteSelect.addEventListener('change', function () {
        currentRemote = remoteSelect.value;
        if (currentRemote) {
            currentPath = defaultPathForRemote(currentRemote);
            pathInput.value = currentPath;
            loadDirectory(currentRemote, currentPath);
        }
    });

    const termReconnectBtn = document.getElementById('btn-term-reconnect');
    if (termReconnectBtn) {
        termReconnectBtn.addEventListener('click', function () {
            if (currentMode !== 'terminal') {
                switchMode('terminal');
            } else {
                if (!term) {
                    initTerminal();
                }
                connectTerminal(true);
            }
        });
    }

    goBtn.addEventListener('click', function () {
        if (currentRemote) {
            currentPath = normalizePath(pathInput.value.trim());
            pathInput.value = currentPath;
            loadDirectory(currentRemote, currentPath);
        }
    });

    pathInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' && currentRemote) {
            currentPath = normalizePath(pathInput.value.trim());
            pathInput.value = currentPath;
            loadDirectory(currentRemote, currentPath);
        }
    });

    searchBtn.addEventListener('click', function () {
        performSearch();
    });

    searchInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') {
            performSearch();
        }
    });

    btnNewFile.addEventListener('click', function () {
        createNewFile();
    });

    btnNewFolder.addEventListener('click', function () {
        createNewFolder();
    });

    btnRefreshDir.addEventListener('click', function () {
        if (currentRemote) {
            loadDirectory(currentRemote, currentPath);
        }
    });

    saveBtn.addEventListener('click', function () {
        saveCurrentFile();
    });

    diffCancelBtn.addEventListener('click', function () {
        document.getElementById('diff-modal').style.display = 'none';
        pendingConflictPath = null;
    });

    diffOverwriteBtn.addEventListener('click', function () {
        document.getElementById('diff-modal').style.display = 'none';
        if (pendingConflictPath) {
            forceSaveFile(pendingConflictPath);
            pendingConflictPath = null;
        }
    });

    btnEditor.addEventListener('click', function () {
        switchMode('editor');
    });

    btnTerminal.addEventListener('click', function () {
        switchMode('terminal');
    });

    keybindSelect.addEventListener('change', function () {
        toggleVimMode(keybindSelect.value === 'vim');
    });

    window.addEventListener('resize', function () {
        if (currentMode === 'terminal' && fitAddon && termWs) {
            fitAddon.fit();
            sendTerminalResize();
        }
    });

    document.addEventListener('click', hideContextMenu);

    const btnUpload = document.getElementById('btn-upload-file');
    const hiddenFileInput = document.getElementById('hidden-file-input');

    if (btnUpload && hiddenFileInput) {
        btnUpload.addEventListener('click', function () {
            hiddenFileInput.click();
        });

        hiddenFileInput.addEventListener('change', function () {
            if (hiddenFileInput.files.length > 0) {
                uploadFiles(hiddenFileInput.files);
            }
        });
    }

    const ctxDl = document.getElementById('ctx-download');
    if (ctxDl) {
        ctxDl.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath && !ctxTargetIsDir) {
                downloadFile(ctxTargetPath);
            }
        });
    }

    const ctxCopyPath = document.getElementById('ctx-copy-path');
    if (ctxCopyPath) {
        ctxCopyPath.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath) {
                navigator.clipboard.writeText(ctxTargetPath).then(() => {
                    showToast('Path copied to clipboard', 'info');
                });
            }
        });
    }

    const ctxDup = document.getElementById('ctx-duplicate');
    if (ctxDup) {
        ctxDup.addEventListener('click', async function () {
            hideContextMenu();
            if (ctxTargetPath && currentRemote) {
                const extPos = ctxTargetPath.lastIndexOf('.');
                let newPath = '';
                if (extPos > 0) {
                    newPath = ctxTargetPath.substring(0, extPos) + '_copy' + ctxTargetPath.substring(extPos);
                } else {
                    newPath = ctxTargetPath + '_copy';
                }
                try {
                    const res = await fetch('/api/copy', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ remote: currentRemote, from: ctxTargetPath, to: newPath })
                    });
                    if (!res.ok) throw new Error('Failed to duplicate');
                    showToast('Duplicated successfully', 'success');
                    loadDirectory(currentRemote, currentPath);
                } catch (err) {
                    showToast(`Duplicate error: ${err.message}`, 'error');
                }
            }
        });
    }

    const ctxRename = document.getElementById('ctx-rename');
    if (ctxRename) {
        ctxRename.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath) {
                renameEntry(ctxTargetPath);
            }
        });
    }

    const ctxDelete = document.getElementById('ctx-delete');
    if (ctxDelete) {
        ctxDelete.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath) {
                deleteEntry(ctxTargetPath);
            }
        });
    }

    const btnSettings = document.getElementById('btn-settings');
    const settingsModal = document.getElementById('settings-modal');
    const settingsCancelBtn = document.getElementById('settings-cancel-btn');
    const settingsSaveBtn = document.getElementById('settings-save-btn');

    if (btnSettings && settingsModal) {
        btnSettings.addEventListener('click', function () {
            settingsModal.style.display = 'flex';
        });
    }
    if (settingsCancelBtn && settingsModal) {
        settingsCancelBtn.addEventListener('click', function () {
            settingsModal.style.display = 'none';
        });
    }
    if (settingsSaveBtn) {
        settingsSaveBtn.addEventListener('click', handlePasswordChange);
    }

    initSidebarResizer();

    const btnBulkDl = document.getElementById('btn-bulk-download');
    if (btnBulkDl) btnBulkDl.addEventListener('click', downloadSelectedZip);

    const btnBulkDel = document.getElementById('btn-bulk-delete');
    if (btnBulkDel) btnBulkDel.addEventListener('click', deleteSelectedItems);

    const btnBulkClear = document.getElementById('btn-bulk-clear');
    if (btnBulkClear) btnBulkClear.addEventListener('click', clearSelection);

    const ctxBulkDl = document.getElementById('ctx-bulk-download');
    if (ctxBulkDl) {
        ctxBulkDl.addEventListener('click', function () {
            hideContextMenu();
            downloadSelectedZip();
        });
    }

    const ctxBulkDel = document.getElementById('ctx-bulk-delete');
    if (ctxBulkDel) {
        ctxBulkDel.addEventListener('click', function () {
            hideContextMenu();
            deleteSelectedItems();
        });
    }

    const btnGlobalSearch = document.getElementById('btn-global-search');
    if (btnGlobalSearch) btnGlobalSearch.addEventListener('click', openGlobalSearchModal);

    const btnExpandAll = document.getElementById('btn-expand-all');
    if (btnExpandAll) btnExpandAll.addEventListener('click', expandAllFolders);

    const btnCollapseAll = document.getElementById('btn-collapse-all');
    if (btnCollapseAll) btnCollapseAll.addEventListener('click', collapseAllFolders);

    const gsCloseBtn = document.getElementById('global-search-close-btn');
    if (gsCloseBtn) {
        gsCloseBtn.addEventListener('click', function() {
            const modal = document.getElementById('global-search-modal');
            if (modal) modal.style.display = 'none';
        });
    }

    const gsExecBtn = document.getElementById('global-search-exec-btn');
    if (gsExecBtn) gsExecBtn.addEventListener('click', performGlobalSearch);

    const gsQueryInput = document.getElementById('global-search-query-input');
    if (gsQueryInput) {
        gsQueryInput.addEventListener('keydown', function (e) {
            if (e.key === 'Enter') performGlobalSearch();
        });
    }

    window.addEventListener('keydown', function (e) {
        if ((e.ctrlKey || e.metaKey) && e.shiftKey && (e.key === 'F' || e.key === 'f')) {
            e.preventDefault();
            openGlobalSearchModal();
        }
        if ((e.ctrlKey || e.metaKey) && e.shiftKey && (e.key === 'G' || e.key === 'g')) {
            e.preventDefault();
            openGitLensModal();
        }
        if ((e.ctrlKey || e.metaKey) && (e.key === 'B' || e.key === 'b')) {
            e.preventDefault();
            toggleWorkspaceSidebar();
        }
    });

    const btnGitLens = document.getElementById('btn-gitlens');
    if (btnGitLens) btnGitLens.addEventListener('click', openGitLensModal);

    const gitBar = document.getElementById('git-status-bar');
    if (gitBar) gitBar.addEventListener('click', openGitLensModal);

    const statusGitBranch = document.getElementById('status-git-branch');
    if (statusGitBranch) statusGitBranch.addEventListener('click', openGitLensModal);

    const glCloseX = document.getElementById('gitlens-modal-close-x');
    if (glCloseX) glCloseX.addEventListener('click', () => {
        const modal = document.getElementById('gitlens-modal');
        if (modal) modal.style.display = 'none';
    });

    const glTabChanges = document.getElementById('gl-tab-changes');
    if (glTabChanges) glTabChanges.addEventListener('click', () => switchGitLensTab('changes'));

    const glTabHistory = document.getElementById('gl-tab-history');
    if (glTabHistory) glTabHistory.addEventListener('click', () => switchGitLensTab('history'));

    const glTabFileHistory = document.getElementById('gl-tab-file-history');
    if (glTabFileHistory) glTabFileHistory.addEventListener('click', () => switchGitLensTab('file-history'));

    const glTabBranches = document.getElementById('gl-tab-branches');
    if (glTabBranches) glTabBranches.addEventListener('click', () => switchGitLensTab('branches'));

    const btnGitCommit = document.getElementById('btn-gitlens-commit');
    if (btnGitCommit) btnGitCommit.addEventListener('click', commitGitChanges);

    const commitMsgInput = document.getElementById('gitlens-commit-msg');
    if (commitMsgInput) {
        commitMsgInput.addEventListener('keydown', (e) => {
            if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
                e.preventDefault();
                commitGitChanges();
            }
        });
    }

    const ctxOpenWs = document.getElementById('ctx-open-workspace');
    if (ctxOpenWs) {
        ctxOpenWs.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath && ctxTargetIsDir) {
                openFolderAsWorkspace(ctxTargetPath);
            }
        });
    }

    const wsCloseBtn = document.getElementById('btn-ws-close');
    if (wsCloseBtn) {
        wsCloseBtn.addEventListener('click', function () {
            closeWorkspace();
        });
    }

    const wsToggleBtn = document.getElementById('btn-ws-toggle-hide');
    if (wsToggleBtn) {
        wsToggleBtn.addEventListener('click', toggleWorkspaceSidebar);
    }

    const btnToggleWsHeader = document.getElementById('btn-toggle-ws-sidebar');
    if (btnToggleWsHeader) {
        btnToggleWsHeader.addEventListener('click', toggleWorkspaceSidebar);
    }

    const wsCollapseBtn = document.getElementById('btn-ws-collapse-all');
    if (wsCollapseBtn) {
        wsCollapseBtn.addEventListener('click', function () {
            expandedFolders.clear();
            if (currentWorkspacePath && currentWorkspacePath !== '/') {
                fetchWorkspaceDirectory(currentRemote, currentWorkspacePath);
            }
        });
    }

    // Resizer for Editor Workspace Sidebar
    const wsResizer = document.getElementById('editor-workspace-resizer');
    const wsSidebar = document.getElementById('editor-workspace-sidebar');
    if (wsResizer && wsSidebar) {
        let isResizing = false;
        wsResizer.addEventListener('mousedown', function (e) {
            isResizing = true;
            document.body.style.cursor = 'col-resize';
            document.body.style.userSelect = 'none';
        });

        document.addEventListener('mousemove', function (e) {
            if (!isResizing) return;
            const rect = wsSidebar.getBoundingClientRect();
            const newWidth = e.clientX - rect.left;
            if (newWidth >= 160 && newWidth <= 450) {
                wsSidebar.style.width = `${newWidth}px`;
                if (editor) editor.layout();
            }
        });

        document.addEventListener('mouseup', function () {
            if (isResizing) {
                isResizing = false;
                document.body.style.cursor = '';
                document.body.style.userSelect = '';
                if (editor) editor.layout();
            }
        });
    }

    const btnZoomIn = document.getElementById('btn-media-zoom-in');
    if (btnZoomIn) {
        btnZoomIn.addEventListener('click', () => {
            currentMediaZoom += 0.2;
            const img = document.querySelector('#media-preview-viewport img');
            if (img) img.style.transform = `scale(${currentMediaZoom})`;
        });
    }
    const btnZoomOut = document.getElementById('btn-media-zoom-out');
    if (btnZoomOut) {
        btnZoomOut.addEventListener('click', () => {
            currentMediaZoom = Math.max(0.2, currentMediaZoom - 0.2);
            const img = document.querySelector('#media-preview-viewport img');
            if (img) img.style.transform = `scale(${currentMediaZoom})`;
        });
    }
    const btnZoomReset = document.getElementById('btn-media-zoom-reset');
    if (btnZoomReset) {
        btnZoomReset.addEventListener('click', () => {
            currentMediaZoom = 1.0;
            const img = document.querySelector('#media-preview-viewport img');
            if (img) img.style.transform = 'scale(1.0)';
        });
    }

    const ctxToggleSelect = document.getElementById('ctx-toggle-select');
    if (ctxToggleSelect) {
        ctxToggleSelect.addEventListener('click', function () {
            hideContextMenu();
            if (ctxTargetPath) {
                if (selectedPaths.has(ctxTargetPath)) {
                    selectedPaths.delete(ctxTargetPath);
                } else {
                    selectedPaths.add(ctxTargetPath);
                }
                updateSelectionUI();
            }
        });
    }

    const ctxSelectAll = document.getElementById('ctx-select-all');
    if (ctxSelectAll) {
        ctxSelectAll.addEventListener('click', function () {
            hideContextMenu();
            const items = document.querySelectorAll('.file-item');
            items.forEach(el => {
                const path = el.getAttribute('data-path');
                if (path) selectedPaths.add(path);
            });
            updateSelectionUI();
        });
    }

    const ctxClearSelect = document.getElementById('ctx-clear-select');
    if (ctxClearSelect) {
        ctxClearSelect.addEventListener('click', function () {
            hideContextMenu();
            clearSelection();
        });
    }

    await loadThemeSettings();
    await fetchRemotes();
}

async function handlePasswordChange() {
    const currInput = document.getElementById('settings-curr-pass');
    const newInput = document.getElementById('settings-new-pass');
    if (!newInput || !newInput.value.trim()) {
        showToast('Please enter a new password', 'error');
        return;
    }

    try {
        const res = await fetch('/api/settings/password', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                current_password: currInput ? currInput.value : null,
                new_password: newInput.value
            })
        });

        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to update password');
        }

        showToast('Password updated successfully', 'success');
        document.getElementById('settings-modal').style.display = 'none';
        if (currInput) currInput.value = '';
        newInput.value = '';
    } catch (err) {
        showToast(`Password update error: ${err.message}`, 'error');
    }
}

function openRemoteModal(existingName) {
    const modal = document.getElementById('remote-modal');
    const nameInput = document.getElementById('remote-form-name');
    const protoSelect = document.getElementById('remote-form-protocol');
    const hostInput = document.getElementById('remote-form-host');
    const portInput = document.getElementById('remote-form-port');
    const userInput = document.getElementById('remote-form-username');
    const passInput = document.getElementById('remote-form-password');
    const bucketInput = document.getElementById('remote-form-bucket');
    const regionInput = document.getElementById('remote-form-region');
    const s3Fields = document.getElementById('remote-form-s3-fields');

    const existing = remotesData.find(r => r.name === existingName);
    if (existing) {
        nameInput.value = existing.name;
        protoSelect.value = existing.protocol;
        hostInput.value = existing.host;
        portInput.value = existing.port;
        userInput.value = existing.username;
        passInput.value = '';
        bucketInput.value = '';
        regionInput.value = '';
    } else {
        nameInput.value = '';
        protoSelect.value = 'sftp';
        hostInput.value = '127.0.0.1';
        portInput.value = '22';
        userInput.value = 'root';
        passInput.value = '';
        bucketInput.value = '';
        regionInput.value = '';
    }

    if (protoSelect.value === 's3') {
        s3Fields.style.display = 'flex';
    } else {
        s3Fields.style.display = 'none';
    }

    modal.style.display = 'flex';
}

async function saveRemoteConfig() {
    const modal = document.getElementById('remote-modal');
    const nameInput = document.getElementById('remote-form-name');
    const protoSelect = document.getElementById('remote-form-protocol');
    const hostInput = document.getElementById('remote-form-host');
    const portInput = document.getElementById('remote-form-port');
    const userInput = document.getElementById('remote-form-username');
    const passInput = document.getElementById('remote-form-password');
    const bucketInput = document.getElementById('remote-form-bucket');
    const regionInput = document.getElementById('remote-form-region');

    const name = nameInput.value.trim();
    if (!name) {
        showToast('Connection name is required', 'error');
        return;
    }

    const payload = {
        name: name,
        protocol: protoSelect.value,
        host: hostInput.value.trim() || '127.0.0.1',
        port: parseInt(portInput.value) || 22,
        username: userInput.value.trim() || 'root',
        password: passInput.value || null,
        private_key: null,
        bucket: bucketInput.value.trim() || null,
        region: regionInput.value.trim() || null
    };

    try {
        const res = await fetch('/api/remotes', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload)
        });

        if (!res.ok) {
            throw new Error('Failed to save remote configuration');
        }

        modal.style.display = 'none';
        showToast(`Remote '${name}' saved`, 'success');
        await fetchRemotes();

        const remoteSelect = document.getElementById('remote-select');
        remoteSelect.value = name;
        currentRemote = name;
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        showToast(err.message, 'error');
    }
}

function normalizePath(p) {
    if (!p) return '/';
    let clean = p.replace(/\\/g, '/');
    if (!clean.startsWith('/')) {
        clean = '/' + clean;
    }
    return clean;
}

function getBasename(path) {
    if (!path) return '';
    const parts = path.split('/');
    return parts[parts.length - 1] || path;
}

async function performSearch() {
    if (!currentRemote) return;
    const searchInput = document.getElementById('search-input');
    const searchCheck = document.getElementById('search-content-check');
    const resultsContainer = document.getElementById('search-results');

    const query = searchInput.value.trim();
    if (!query) {
        resultsContainer.innerHTML = '';
        return;
    }

    resultsContainer.innerHTML = '<div class="search-result-item">Searching...</div>';

    try {
        const isContent = searchCheck.checked;
        const url = `/api/search?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(currentPath)}&query=${encodeURIComponent(query)}&content=${isContent}`;
        const res = await fetch(url);
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Search failed');
        }

        const matches = await res.json();
        resultsContainer.innerHTML = '';

        if (matches.length === 0) {
            resultsContainer.innerHTML = '<div class="search-result-item">No matches found</div>';
            return;
        }

        matches.forEach(m => {
            const item = document.createElement('div');
            item.className = 'search-result-item';

            const lineStr = m.line_number ? `:${m.line_number}` : '';
            const snippetStr = m.snippet ? `<div class="match-snippet">${escapeHtml(m.snippet)}</div>` : '';

            item.innerHTML = `
                <div class="match-path">${escapeHtml(m.path)}${lineStr}</div>
                ${snippetStr}
            `;

            item.addEventListener('click', function () {
                openFile(m.path, m.line_number);
            });

            resultsContainer.appendChild(item);
        });
    } catch (err) {
        resultsContainer.innerHTML = `<div class="search-result-item" style="color: #ef4444;">${escapeHtml(err.message)}</div>`;
    }
}

function switchMode(mode) {
    currentMode = mode;
    const btnEditor = document.getElementById('btn-mode-editor');
    const btnTerminal = document.getElementById('btn-mode-terminal');
    const monacoBox = document.getElementById('monaco-container');
    const terminalBox = document.getElementById('terminal-container');
    const editorHeader = document.getElementById('editor-header-bar');
    const tabBar = document.getElementById('tab-bar');

    if (mode === 'editor') {
        btnEditor.classList.add('active');
        btnTerminal.classList.remove('active');
        monacoBox.style.display = 'block';
        terminalBox.style.display = 'none';
        editorHeader.style.display = 'flex';
        tabBar.style.display = 'flex';
        if (editor) editor.layout();
    } else {
        btnTerminal.classList.add('active');
        btnEditor.classList.remove('active');
        monacoBox.style.display = 'none';
        terminalBox.style.display = 'block';
        editorHeader.style.display = 'none';
        tabBar.style.display = 'none';

        if (!term) {
            initTerminal();
        }
        connectTerminal();
    }
}

function renderTabBar() {
    const tabBar = document.getElementById('tab-bar');
    tabBar.innerHTML = '';

    openTabs.forEach(filePath => {
        const tab = document.createElement('div');
        const isActive = filePath === activeTabPath;
        const isDirty = tabDirty.get(filePath) || false;
        const fileName = getBasename(filePath);

        tab.className = `tab-item ${isActive ? 'active' : ''}`;
        tab.innerHTML = `
            <span>${escapeHtml(fileName)}${isDirty ? ' *' : ''}</span>
            <span class="close-tab" data-path="${escapeHtml(filePath)}">&times;</span>
        `;

        tab.addEventListener('click', function (e) {
            if (e.target.classList.contains('close-tab')) {
                e.stopPropagation();
                closeTab(filePath);
            } else {
                switchTab(filePath);
            }
        });

        tabBar.appendChild(tab);
    });

    renderOpenEditorsList();
}

function renderOpenEditorsList() {
    const list = document.getElementById('open-editors-list');
    const countEl = document.getElementById('open-editors-count');
    if (!list) return;

    list.innerHTML = '';
    if (countEl) countEl.textContent = openTabs.length;

    openTabs.forEach(filePath => {
        const item = document.createElement('div');
        const isActive = filePath === activeTabPath;
        const isDirty = tabDirty.get(filePath) || false;
        item.className = `open-editor-item ${isActive ? 'active' : ''}`;

        const fileIconSvg = getFileIconSvg(getBasename(filePath), false, false);

        item.innerHTML = `
            <div style="display: flex; align-items: center; gap: 6px; overflow: hidden;">
                ${fileIconSvg}
                <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(getBasename(filePath))}</span>
                ${isDirty ? '<span style="color: var(--accent-color); font-size: 10px;">●</span>' : ''}
            </div>
            <span class="close-editor-btn" style="font-size: 14px; opacity: 0.6; cursor: pointer;">&times;</span>
        `;

        item.addEventListener('click', (e) => {
            if (e.target.classList.contains('close-editor-btn')) {
                e.stopPropagation();
                closeTab(filePath);
            } else {
                switchTab(filePath);
            }
        });

        list.appendChild(item);
    });
}

let currentWorkspacePath = '/';

async function fetchWorkspaceDirectory(remote, path) {
    const wsTree = document.getElementById('workspace-file-tree');
    if (wsTree) {
        wsTree.innerHTML = '<div class="empty-state" style="padding: 12px; font-size: 11px;">Loading workspace...</div>';
    }

    try {
        const url = `/api/files?remote=${encodeURIComponent(remote)}&path=${encodeURIComponent(path)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to load workspace files');
        const files = await res.json();
        await fetchGitStatus(remote, path);
        renderWorkspaceFileTree(files, path);
        return files;
    } catch (err) {
        if (wsTree) {
            wsTree.innerHTML = `<div class="empty-state" style="padding: 12px; font-size: 11px; color: #ef4444;">${escapeHtml(err.message)}</div>`;
        }
        return [];
    }
}

function renderWorkspaceFileTree(files, path) {
    const wsTree = document.getElementById('workspace-file-tree');
    if (!wsTree) return;
    wsTree.innerHTML = '';
    if (files.length === 0) {
        wsTree.innerHTML = '<div class="empty-state" style="padding: 12px; font-size: 11px;">Workspace directory is empty</div>';
        return;
    }
    renderDirectoryItems(files, path, 0, wsTree);
}

async function openFolderAsWorkspace(folderPath) {
    if (!folderPath) return;

    if (currentMode !== 'editor') {
        switchMode('editor');
    }

    currentWorkspacePath = folderPath;
    expandedFolders.clear();
    folderChildrenMap.clear();

    const wsSidebar = document.getElementById('editor-workspace-sidebar');
    if (wsSidebar) wsSidebar.style.display = 'flex';

    const titleEl = document.getElementById('editor-ws-title');
    if (titleEl) {
        titleEl.textContent = `Workspace: ${getBasename(folderPath) || folderPath}`;
    }

    showToast(`Opening workspace: ${getBasename(folderPath)}...`, 'info');

    const files = await fetchWorkspaceDirectory(currentRemote, folderPath);

    if (Array.isArray(files) && files.length > 0) {
        const firstFile = files.find(f => !f.is_dir);
        if (firstFile) {
            await openFile(firstFile.path);
        }
    }

    showToast(`Opened workspace: ${getBasename(folderPath)}`, 'success');
}

function closeWorkspace() {
    const tabsToClose = [...openTabs];
    tabsToClose.forEach(filePath => {
        const ws = collabSockets.get(filePath);
        if (ws) {
            ws.close();
            collabSockets.delete(filePath);
        }
        const model = tabModels.get(filePath);
        if (model) {
            model.dispose();
            tabModels.delete(filePath);
        }
        tabDirty.delete(filePath);
    });
    openTabs = [];
    activeTabPath = null;

    if (editor) {
        editor.setModel(monaco.editor.createModel('// Select a file or open a workspace folder to start editing\n', 'plaintext'));
    }

    renderTabBar();
    renderOpenEditorsList();

    currentWorkspacePath = '/';
    const titleEl = document.getElementById('editor-ws-title');
    if (titleEl) {
        titleEl.textContent = 'Workspace: /';
    }

    const wsTree = document.getElementById('workspace-file-tree');
    if (wsTree) {
        wsTree.innerHTML = '<div class="empty-state" style="padding: 12px; font-size: 11px;">Right click a folder and select \'Open Folder in Editor\' to open a workspace</div>';
    }

    currentGitBranch = '';
    currentGitRepoRoot = '';
    currentGitStatuses.clear();
    const gitBar = document.getElementById('git-status-bar');
    if (gitBar) gitBar.style.display = 'none';
    const blameBar = document.getElementById('git-blame-bar');
    if (blameBar) blameBar.style.display = 'none';

    updateBreadcrumbs(null);
    updateVSCodeStatusBar(null);

    showToast('Closed workspace and all open editor files', 'info');
}

function toggleWorkspaceSidebar() {
    const sidebar = document.getElementById('editor-workspace-sidebar');
    const resizer = document.getElementById('editor-workspace-resizer');
    if (!sidebar) return;

    const isHidden = (sidebar.style.display === 'none');
    sidebar.style.display = isHidden ? 'flex' : 'none';
    if (resizer) resizer.style.display = isHidden ? 'block' : 'none';
    if (editor) editor.layout();
}

function updateBreadcrumbs(filePath) {
    const bar = document.getElementById('breadcrumb-content');
    if (!bar) return;
    const folderIconSvg = `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg" style="margin-right: 4px; vertical-align: -2px;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path></svg>`;
    if (!filePath) {
        bar.innerHTML = `${folderIconSvg} Select a file to edit`;
        return;
    }

    const normPath = filePath.replace(/\\/g, '/');
    const parts = normPath.split('/').filter(Boolean);
    let html = `<span class="crumb-item" onclick="loadDirectory(currentRemote, '/')">${folderIconSvg} /</span>`;

    let accum = '';
    parts.forEach((p, idx) => {
        accum += '/' + p;
        const isLast = idx === parts.length - 1;
        html += `<span class="crumb-sep">&gt;</span>`;
        if (isLast) {
            html += `<span style="color: var(--text-primary); font-weight: 500;">${escapeHtml(p)}</span>`;
        } else {
            const folderTarget = accum;
            html += `<span class="crumb-item" onclick="loadDirectory(currentRemote, '${escapeHtml(folderTarget)}')">${escapeHtml(p)}</span>`;
        }
    });

    bar.innerHTML = html;
}

function updateVSCodeStatusBar(filePath) {
    const langEl = document.getElementById('status-language');
    if (langEl && filePath) {
        langEl.textContent = getLanguageFromFilename(filePath);
    }
    const branchEl = document.getElementById('status-branch-name');
    if (branchEl) {
        branchEl.textContent = currentGitBranch || 'N/A';
    }
    const countsEl = document.getElementById('status-git-counts');
    if (countsEl) {
        let m = 0, u = 0;
        currentGitStatuses.forEach(st => {
            if (st === 'M') m++;
            if (st === 'U') u++;
        });
        countsEl.textContent = `[${m} M, ${u} U]`;
    }
}

function switchTab(filePath, lineNumber) {
    if (!openTabs.includes(filePath)) return;
    activeTabPath = filePath;
    const model = tabModels.get(filePath);
    if (model && editor) {
        editor.setModel(model);
        if (lineNumber) {
            editor.setPosition({ lineNumber: lineNumber, column: 1 });
            editor.revealLineInCenter(lineNumber);
        }
    }

    document.getElementById('active-file-path').textContent = filePath;
    document.getElementById('save-btn').disabled = false;

    const isDirty = tabDirty.get(filePath) || false;
    updateStatusBadge(isDirty ? 'Modified' : 'Saved', isDirty ? 'modified' : 'saved');

    // Automatically trigger Git status detection for the active file!
    fetchGitStatus(currentRemote, filePath);

    const toggleBtn = document.getElementById('btn-toggle-diff');
    if (toggleBtn) {
        const isGitMod = currentGitStatuses.has(filePath) || currentGitStatuses.has(getBasename(filePath));
        toggleBtn.style.display = isGitMod ? 'inline-block' : 'none';
        if (!isDiffActive) toggleBtn.textContent = 'View Git Diff';
    }

    if (editor) {
        const pos = editor.getPosition();
        const curLine = pos ? pos.lineNumber : 1;
        fetchGitBlame(filePath, curLine);
    }

    updateBreadcrumbs(filePath);
    updateVSCodeStatusBar(filePath);
    renderTabBar();
}

async function closeTab(filePath) {
    const isDirty = tabDirty.get(filePath) || false;
    if (isDirty) {
        const confirmed = await customConfirm(`File '${filePath}' has unsaved changes. Close anyway?`, 'Unsaved Changes');
        if (!confirmed) {
            return;
        }
    }

    const ws = collabSockets.get(filePath);
    if (ws) {
        ws.close();
        collabSockets.delete(filePath);
    }

    const index = openTabs.indexOf(filePath);
    if (index > -1) {
        openTabs.splice(index, 1);
    }

    const model = tabModels.get(filePath);
    if (model) {
        model.dispose();
        tabModels.delete(filePath);
    }
    tabDirty.delete(filePath);
    fileHashes.delete(filePath);

    if (activeTabPath === filePath) {
        if (openTabs.length > 0) {
            const nextTab = openTabs[Math.max(0, index - 1)];
            switchTab(nextTab);
        } else {
            activeTabPath = null;
            document.getElementById('active-file-path').textContent = 'No file active';
            document.getElementById('save-btn').disabled = true;
            updateStatusBadge('Saved', 'saved');
            if (editor) {
                const emptyModel = monaco.editor.createModel('// Select a file to edit\n', 'plaintext');
                editor.setModel(emptyModel);
            }
        }
    }

    renderTabBar();
}

let currentMediaZoom = 1.0;

async function openFile(filePath, lineNumber) {
    if (currentMode !== 'editor') {
        switchMode('editor');
    }

    const ext = filePath.split('.').pop().toLowerCase();
    const fileName = getBasename(filePath);

    const monacoContainer = document.getElementById('monaco-container');
    const diffContainer = document.getElementById('monaco-diff-container');
    const mediaContainer = document.getElementById('media-preview-container');
    const zipContainer = document.getElementById('zip-inspector-container');

    const imageExts = ['png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'ico', 'bmp'];
    const videoExts = ['mp4', 'webm', 'ogg', 'mov'];
    const audioExts = ['mp3', 'wav', 'flac', 'aac'];
    const zipExts = ['zip', 'tar.gz', 'tgz'];

    // Rich Media Preview (Images, Videos, Audio, PDF)
    if (imageExts.includes(ext) || videoExts.includes(ext) || audioExts.includes(ext) || ext === 'pdf') {
        if (monacoContainer) monacoContainer.style.display = 'none';
        if (diffContainer) diffContainer.style.display = 'none';
        if (zipContainer) zipContainer.style.display = 'none';
        if (mediaContainer) mediaContainer.style.display = 'flex';

        document.getElementById('active-file-path').textContent = filePath;
        document.getElementById('media-preview-title').textContent = fileName;

        const fileUrl = `/api/file?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
        const viewport = document.getElementById('media-preview-viewport');
        const dimBadge = document.getElementById('media-dimensions-badge');
        const sizeBadge = document.getElementById('media-size-badge');
        viewport.innerHTML = '';
        currentMediaZoom = 1.0;

        if (imageExts.includes(ext)) {
            const img = document.createElement('img');
            img.src = fileUrl;
            img.onload = () => {
                if (dimBadge) dimBadge.textContent = `${img.naturalWidth} × ${img.naturalHeight} px`;
            };
            viewport.appendChild(img);
        } else if (videoExts.includes(ext)) {
            const video = document.createElement('video');
            video.src = fileUrl;
            video.controls = true;
            video.autoplay = true;
            video.preload = 'metadata';
            video.style.width = '100%';
            video.style.maxHeight = '90%';
            video.onloadedmetadata = () => {
                const mins = Math.floor(video.duration / 60);
                const secs = Math.floor(video.duration % 60).toString().padStart(2, '0');
                if (dimBadge) dimBadge.textContent = `${video.videoWidth || 'HD'} × ${video.videoHeight || 'Video'} (${mins}:${secs})`;
            };
            viewport.appendChild(video);
        } else if (audioExts.includes(ext)) {
            const audio = document.createElement('audio');
            audio.src = fileUrl;
            audio.controls = true;
            audio.autoplay = true;
            audio.preload = 'metadata';
            audio.onloadedmetadata = () => {
                const mins = Math.floor(audio.duration / 60);
                const secs = Math.floor(audio.duration % 60).toString().padStart(2, '0');
                if (dimBadge) dimBadge.textContent = `Audio (${mins}:${secs})`;
            };
            viewport.appendChild(audio);
        } else if (ext === 'pdf') {
            const embed = document.createElement('embed');
            embed.src = fileUrl;
            embed.type = 'application/pdf';
            viewport.appendChild(embed);
            if (dimBadge) dimBadge.textContent = 'PDF Document';
        }

        if (!openTabs.includes(filePath)) {
            openTabs.push(filePath);
        }
        activeTabPath = filePath;
        renderTabBar();
        renderOpenEditorsList();
        updateBreadcrumbs(filePath);
        return;
    }

    // ZIP Archive Inspector
    if (zipExts.includes(ext)) {
        if (monacoContainer) monacoContainer.style.display = 'none';
        if (diffContainer) diffContainer.style.display = 'none';
        if (mediaContainer) mediaContainer.style.display = 'none';
        if (zipContainer) zipContainer.style.display = 'flex';

        document.getElementById('active-file-path').textContent = filePath;
        document.getElementById('zip-inspector-title').textContent = `Archive Inspector: ${fileName}`;

        inspectZipArchive(filePath);

        if (!openTabs.includes(filePath)) {
            openTabs.push(filePath);
        }
        activeTabPath = filePath;
        renderTabBar();
        renderOpenEditorsList();
        updateBreadcrumbs(filePath);
        return;
    }

    // Text & Code files (Monaco)
    if (mediaContainer) mediaContainer.style.display = 'none';
    if (zipContainer) zipContainer.style.display = 'none';
    if (monacoContainer) monacoContainer.style.display = 'block';

    if (openTabs.includes(filePath)) {
        switchTab(filePath, lineNumber);
        return;
    }

    updateStatusBadge('Loading...', '');
    try {
        const url = `/api/file?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
        const res = await fetch(url);
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to read file');
        }
        const text = await res.text();
        const hash = res.headers.get('x-content-hash');
        if (hash) {
            fileHashes.set(filePath, hash);
        }

        const language = getLanguageFromFilename(filePath);
        const model = monaco.editor.createModel(text, language);

        openTabs.push(filePath);
        tabModels.set(filePath, model);
        tabDirty.set(filePath, false);

        connectCollab(filePath);
        switchTab(filePath, lineNumber);
    } catch (err) {
        showToast(`Error reading file: ${err.message}`, 'error');
        updateStatusBadge('Error', 'modified');
    }
}

async function inspectZipArchive(zipPath) {
    const list = document.getElementById('zip-inspector-entries-list');
    const badge = document.getElementById('zip-inspector-count-badge');
    if (!list) return;
    list.innerHTML = '<div class="empty-state">Loading archive entries...</div>';

    try {
        const url = `/api/zip/inspect?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(zipPath)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to inspect ZIP file');

        const data = await res.json();
        list.innerHTML = '';
        if (badge) badge.textContent = `${data.entries.length} entries`;

        if (data.entries.length === 0) {
            list.innerHTML = '<div class="empty-state">Archive is empty</div>';
            return;
        }

        data.entries.forEach(entry => {
            const row = document.createElement('div');
            row.className = 'zip-entry-row';

            const fileIcon = getFileIconSvg(getBasename(entry.name), entry.is_dir, false);
            const sizeStr = entry.is_dir ? 'DIR' : formatBytes(entry.uncompressed_size);

            row.innerHTML = `
                <div style="display: flex; align-items: center; gap: 8px; overflow: hidden;">
                    ${fileIcon}
                    <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: ${entry.is_dir ? '600' : 'normal'};">${escapeHtml(entry.name)}</span>
                </div>
                <div style="display: flex; align-items: center; gap: 10px;">
                    <span style="color: var(--text-muted); font-size: 10px;">${sizeStr}</span>
                    ${!entry.is_dir ? `<button class="btn-subtle" style="font-size: 10px; padding: 2px 8px;">Extract & View</button>` : ''}
                </div>
            `;

            if (!entry.is_dir) {
                const btn = row.querySelector('button');
                if (btn) {
                    btn.addEventListener('click', () => extractAndOpenZipEntry(zipPath, entry.name));
                }
            }

            list.appendChild(row);
        });
    } catch (err) {
        list.innerHTML = `<div class="empty-state" style="color: #ef4444;">ZIP error: ${escapeHtml(err.message)}</div>`;
    }
}

async function extractAndOpenZipEntry(zipPath, entryName) {
    showToast(`Extracting ${getBasename(entryName)}...`, 'info');
    try {
        const url = `/api/zip/extract?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(zipPath)}&entry=${encodeURIComponent(entryName)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Extraction failed');
        const text = await res.text();

        const fakePath = `${zipPath} > ${entryName}`;
        const lang = getLanguageFromFilename(entryName);
        const model = monaco.editor.createModel(text, lang);

        openTabs.push(fakePath);
        tabModels.set(fakePath, model);
        tabDirty.set(fakePath, false);

        switchTab(fakePath);
        showToast(`Extracted ${getBasename(entryName)}`, 'success');
    } catch (err) {
        showToast(`Extract error: ${err.message}`, 'error');
    }
}



const collabDecorations = new Map();

function connectCollab(filePath) {
    if (collabSockets.has(filePath)) return;

    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}/ws/collab?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
    const ws = new WebSocket(wsUrl);

    const userName = authedUsername || collabUserName;
    const userColor = collabUserColor;

    ws.onopen = function() {
        ws.send(JSON.stringify({ type: 'presence', user: userName, color: userColor }));
    };

    ws.onmessage = function (event) {
        try {
            const data = JSON.parse(event.data);
            if (data.type === 'edit' && activeTabPath === filePath) {
                const model = tabModels.get(filePath);
                if (model && model.getValue() !== data.content) {
                    model.setValue(data.content);
                }
            } else if (data.type === 'cursor' && activeTabPath === filePath && data.user !== userName) {
                renderRemoteCursor(filePath, data);
            } else if (data.type === 'presence') {
                updateCollabPresenceBadge(data);
            }
        } catch (_) {}
    };

    collabSockets.set(filePath, ws);

    if (editor) {
        editor.onDidChangeCursorPosition((e) => {
            if (activeTabPath === filePath && ws.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify({
                    type: 'cursor',
                    user: userName,
                    color: userColor,
                    line: e.position.lineNumber,
                    column: e.position.column
                }));
            }
        });
    }
}

function renderRemoteCursor(filePath, data) {
    if (!editor || activeTabPath !== filePath) return;
    const oldDecs = collabDecorations.get(data.user) || [];

    const newDecs = editor.deltaDecorations(oldDecs, [
        {
            range: new monaco.Range(data.line, data.column, data.line, data.column + 1),
            options: {
                className: 'remote-cursor-decoration',
                hoverMessage: { value: `**${data.user}** is editing here` }
            }
        }
    ]);
    collabDecorations.set(data.user, newDecs);
}

function updateCollabPresenceBadge(data) {
    const list = document.getElementById('collab-users-list');
    if (!list) return;
    if (data.user) {
        let existing = document.getElementById(`collab-user-${data.user}`);
        if (!existing) {
            const badge = document.createElement('span');
            badge.id = `collab-user-${data.user}`;
            badge.className = 'media-badge';
            badge.style.background = data.color || 'var(--accent-color)';
            badge.style.color = '#ffffff';
            badge.style.fontWeight = 'bold';
            badge.textContent = data.user;
            list.appendChild(badge);
        }
    }
}

async function saveCurrentFile() {
    if (!activeTabPath || !currentRemote) return;

    const saveBtn = document.getElementById('save-btn');
    saveBtn.disabled = true;
    updateStatusBadge('Saving...', '');

    const targetPath = activeTabPath;
    const model = tabModels.get(targetPath);
    const content = model ? model.getValue() : editor.getValue();
    const currentHash = fileHashes.get(targetPath);

    const headers = { 'Content-Type': 'text/plain' };
    if (currentHash) {
        headers['If-Match'] = currentHash;
    }

    try {
        const url = `/api/file?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(targetPath)}`;
        const res = await fetch(url, {
            method: 'PUT',
            headers: headers,
            body: content
        });

        if (res.status === 412) {
            pendingConflictPath = targetPath;
            document.getElementById('diff-modal').style.display = 'flex';
            updateStatusBadge('Conflict', 'modified');
            return;
        }

        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to save file');
        }

        tabDirty.set(targetPath, false);
        updateStatusBadge('Saved', 'saved');
        showToast('File saved successfully', 'success');
        renderTabBar();

        openFile(targetPath);
    } catch (err) {
        showToast(`Error saving file: ${err.message}`, 'error');
        updateStatusBadge('Save Failed', 'modified');
    } finally {
        saveBtn.disabled = false;
    }
}

async function forceSaveFile(filePath) {
    if (!filePath || !currentRemote) return;
    const model = tabModels.get(filePath);
    const content = model ? model.getValue() : editor.getValue();

    try {
        const url = `/api/file?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
        const res = await fetch(url, {
            method: 'PUT',
            headers: { 'Content-Type': 'text/plain' },
            body: content
        });

        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to force save file');
        }

        tabDirty.set(filePath, false);
        updateStatusBadge('Saved', 'saved');
        showToast('File force saved successfully', 'success');
        renderTabBar();
    } catch (err) {
        showToast(`Error force saving file: ${err.message}`, 'error');
    }
}

async function createNewFile() {
    if (!currentRemote) return;
    const name = await customPrompt('Create New File', '');
    if (!name || !name.trim()) return;

    const fullPath = currentPath.endsWith('/') ? `${currentPath}${name.trim()}` : `${currentPath}/${name.trim()}`;
    try {
        const url = `/api/file?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(fullPath)}`;
        const res = await fetch(url, { method: 'POST' });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to create file');
        }
        showToast(`File created: ${name.trim()}`, 'success');
        loadDirectory(currentRemote, currentPath);
        openFile(fullPath);
    } catch (err) {
        showToast(`Error creating file: ${err.message}`, 'error');
    }
}

async function createNewFolder() {
    if (!currentRemote) return;
    const name = await customPrompt('Create New Directory', '');
    if (!name || !name.trim()) return;

    const fullPath = currentPath.endsWith('/') ? `${currentPath}${name.trim()}` : `${currentPath}/${name.trim()}`;
    try {
        const url = `/api/directory?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(fullPath)}`;
        const res = await fetch(url, { method: 'POST' });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to create directory');
        }
        showToast(`Directory created: ${name.trim()}`, 'success');
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        showToast(`Error creating directory: ${err.message}`, 'error');
    }
}

async function deleteEntry(path) {
    if (!currentRemote) return;
    const confirmed = await customConfirm(`Are you sure you want to delete '${path}'?`, 'Delete Entry');
    if (!confirmed) return;

    try {
        const url = `/api/entry?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(path)}`;
        const res = await fetch(url, { method: 'DELETE' });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to delete entry');
        }
        showToast(`Deleted: ${path}`, 'success');
        if (openTabs.includes(path)) {
            closeTab(path);
        }
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        showToast(`Error deleting entry: ${err.message}`, 'error');
    }
}

async function renameEntry(oldPath) {
    if (!currentRemote) return;
    const name = await customPrompt('Rename Entry Path', oldPath);
    if (!name || !name.trim() || name === oldPath) return;

    try {
        const res = await fetch('/api/rename', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                remote: currentRemote,
                from: oldPath,
                to: name.trim()
            })
        });

        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to rename entry');
        }

        showToast(`Renamed to ${name.trim()}`, 'success');
        if (openTabs.includes(oldPath)) {
            closeTab(oldPath);
            openFile(name.trim());
        }
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        showToast(`Error renaming entry: ${err.message}`, 'error');
    }
}

function initTerminal() {
    const container = document.getElementById('terminal-container');
    container.innerHTML = '';

    term = new Terminal({
        fontFamily: 'JetBrains Mono, monospace',
        fontSize: 13,
        theme: {
            background: '#050507',
            foreground: '#ffffff',
            cursor: '#ffffff',
            selectionBackground: '#27272a'
        },
        cursorBlink: true
    });

    fitAddon = new FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    term.open(container);
    fitAddon.fit();

    term.onData(data => {
        if (termWs && termWs.readyState === WebSocket.OPEN) {
            termWs.send(data);
        }
    });
}

function connectTerminal(force) {
    if (termConnecting && !force) return;
    const isLive = termWs && (termWs.readyState === WebSocket.OPEN || termWs.readyState === WebSocket.CONNECTING);
    if (isLive && !force) {
        return;
    }

    if (termWs) {
        termManualClose = true;
        const oldWs = termWs;
        termWs = null;
        oldWs.onopen = null;
        oldWs.onmessage = null;
        oldWs.onerror = null;
        oldWs.onclose = null;
        try {
            oldWs.close();
        } catch (_) {}
    }
    termManualClose = false;
    termConnecting = true;

    term.reset();
    term.write('\r\nConnecting to SSH terminal ...\r\n');

    fitAddon.fit();
    const cols = term.cols || 80;
    const rows = term.rows || 24;

    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}/ws/terminal?cols=${cols}&rows=${rows}`;

    termWs = new WebSocket(wsUrl);
    termWs.binaryType = 'arraybuffer';

    let firstDataReceived = false;

    termWs.onopen = function () {
        termConnecting = false;
        firstDataReceived = false;
        sendTerminalResize();
    };

    termWs.onmessage = function (event) {
        if (!firstDataReceived) {
            firstDataReceived = true;
            term.reset();
        }
        if (typeof event.data === 'string') {
            term.write(event.data);
        } else {
            const bytes = new Uint8Array(event.data);
            term.write(bytes);
        }
    };

    termWs.onclose = function () {
        termConnecting = false;
        termWs = null;
        if (termManualClose) {
            termManualClose = false;
            return;
        }
        term.write('\r\n[SSH terminal session closed. Click "Reconnect SSH" to start a new session.]\r\n');
    };

    termWs.onerror = function () {
        termConnecting = false;
        term.write('\r\n[Connection error - check the terminal host in ferrite.yaml]\r\n');
    };
}

function sendTerminalResize() {
    if (termWs && termWs.readyState === WebSocket.OPEN && term) {
        const resizePayload = JSON.stringify({
            type: 'resize',
            cols: term.cols,
            rows: term.rows
        });
        termWs.send(resizePayload);
    }
}

function attachVimModeEngine() {
    if (!editor) return;
    const vimStatusNode = document.getElementById('vim-status-node');
    const vimLib = window.MonacoVim || window.monacoVim;

    if (vimLib && typeof vimLib.initVimMode === 'function') {
        if (vimStatusNode) vimStatusNode.style.display = 'inline-block';
        if (!currentVimMode) {
            currentVimMode = vimLib.initVimMode(editor, vimStatusNode);
            const VimObj = vimLib.Vim || (currentVimMode && currentVimMode.Vim);
            if (VimObj && typeof VimObj.defineEx === 'function') {
                try {
                    VimObj.defineEx('write', 'w', function () {
                        saveCurrentFile();
                    });
                } catch (_) {}
            }
        }
    } else {
        if (document.getElementById('monaco-vim-script')) return;
        const script = document.createElement('script');
        script.id = 'monaco-vim-script';
        script.src = 'https://cdn.jsdelivr.net/npm/monaco-vim@0.3.4/dist/monaco-vim.min.js';
        script.onload = function () {
            if (window.require && typeof window.require === 'function' && !window.MonacoVim) {
                try {
                    window.require(['monaco-vim'], function (m) {
                        if (m) window.MonacoVim = m;
                        attachVimModeEngine();
                    });
                } catch (_) {
                    attachVimModeEngine();
                }
            } else {
                attachVimModeEngine();
            }
        };
        document.body.appendChild(script);
    }
}

function toggleVimMode(enabled) {
    const keybindSelect = document.getElementById('keybind-select');
    if (keybindSelect && enabled !== undefined) {
        keybindSelect.value = enabled ? 'vim' : 'default';
    } else if (enabled === undefined && keybindSelect) {
        enabled = (keybindSelect.value === 'vim');
    }

    const vimStatusNode = document.getElementById('vim-status-node');

    if (enabled) {
        updateStatusBadge('Vim Active', 'saved');
        attachVimModeEngine();
    } else {
        updateStatusBadge('Saved', 'saved');
        if (currentVimMode) {
            try {
                currentVimMode.dispose();
            } catch (_) {}
            currentVimMode = null;
        }
        if (vimStatusNode) {
            vimStatusNode.style.display = 'none';
            vimStatusNode.textContent = '';
        }
    }
}

async function fetchRemotes() {
    const remoteSelect = document.getElementById('remote-select');
    try {
        const res = await fetch('/api/remotes');
        if (!res.ok) throw new Error('Failed to fetch remotes');
        remotesData = await res.json();
        remoteSelect.innerHTML = '';
        if (remotesData.length === 0) {
            remoteSelect.innerHTML = '<option value="">No remotes configured</option>';
            return;
        }

        remotesData.forEach(r => {
            const opt = document.createElement('option');
            opt.value = r.name;
            opt.textContent = `[${r.protocol.toUpperCase()}] ${r.name} (${r.host})`;
            remoteSelect.appendChild(opt);
        });

        if (!currentRemote || !remotesData.some(r => r.name === currentRemote)) {
            currentRemote = remotesData[0].name;
            currentPath = defaultPathForRemote(currentRemote);
            const pathInput = document.getElementById('path-input');
            if (pathInput) pathInput.value = currentPath;
        }
        remoteSelect.value = currentRemote;
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        console.error(err);
        remoteSelect.innerHTML = '<option value="">Error loading remotes</option>';
    }
}

async function loadDirectory(remote, path) {
    const tree = document.getElementById('file-tree');
    tree.innerHTML = '<div class="empty-state">Loading directory...</div>';

    try {
        const url = `/api/files?remote=${encodeURIComponent(remote)}&path=${encodeURIComponent(path)}`;
        const res = await fetch(url);
        if (res.status === 403) {
            const errData = await res.json().catch(() => ({}));
            const msg = errData.error || "You don't have permission to view this folder.";
            showToast(msg, 'error');
            tree.innerHTML = `
                <div class="empty-state" style="padding: 20px; text-align: center; gap: 8px;">
                    <div style="font-weight: 600; color: #ef4444;">Access Denied</div>
                    <div style="font-size: 11px; color: var(--text-muted); max-width: 260px; word-break: break-word;">${escapeHtml(msg)}</div>
                </div>`;
            return [];
        }
        if (res.status === 401) {
            showToast(`Auth required for remote '${remote}'`, 'error');
            openRemoteModal(remote);
            tree.innerHTML = `
                <div class="empty-state" style="padding: 20px; text-align: center; gap: 8px;">
                    <div style="font-weight: 600; color: #ef4444;">Authentication Required</div>
                    <div style="font-size: 11px; color: var(--text-muted);">Update username and password to connect.</div>
                    <button class="btn-subtle" onclick="openRemoteModal('${escapeHtml(remote)}')" style="margin-top: 6px;">Configure Credentials</button>
                </div>`;
            return [];
        }
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to list directory');
        }
        const files = await res.json();
        await fetchGitStatus(remote, path);
        renderFileTree(files, path);
        return files;
    } catch (err) {
        let msg = err.message;
        if (msg.includes('10061') || msg.includes('refused')) {
            msg = `Connection refused at target host. Make sure an SSH/SFTP server is running on port 22 or update connection settings.`;
        } else if (msg.includes('10060') || msg.includes('timed out')) {
            msg = `Connection timed out. Target host is unreachable or blocked by firewall. Update connection settings.`;
        }

        tree.innerHTML = `
            <div class="empty-state" style="padding: 20px; text-align: center; gap: 8px;">
                <div style="font-weight: 600; color: #ef4444;">Connection Failed</div>
                <div style="font-size: 11px; color: var(--text-muted); max-width: 260px; word-break: break-word;">${escapeHtml(msg)}</div>
                <button class="btn-subtle" onclick="openRemoteModal('${escapeHtml(remote)}')" style="margin-top: 6px;">Configure Connection</button>
            </div>`;
        return [];
    }
}

const expandedFolders = new Set();
const folderChildrenMap = new Map();
let currentRootFiles = [];

function renderFileTree(files, path) {
    const tree = document.getElementById('file-tree');
    tree.innerHTML = '';
    currentRootFiles = files;
    folderChildrenMap.set(path, files);

    if (path !== '/' && path !== '') {
        const parentItem = document.createElement('div');
        parentItem.className = 'file-item is-dir';
        parentItem.innerHTML = `<span class="tag">DIR</span><span class="name">..</span>`;
        parentItem.addEventListener('click', function () {
            const parts = path.split('/').filter(Boolean);
            parts.pop();
            const parentPath = '/' + parts.join('/');
            currentPath = parentPath;
            document.getElementById('path-input').value = currentPath;
            loadDirectory(currentRemote, currentPath);
        });
        tree.appendChild(parentItem);
    }

    if (!files || files.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'empty-state';
        empty.textContent = 'Directory is empty';
        tree.appendChild(empty);
        return;
    }

    renderDirectoryItems(files, path, 0, tree);
}

function getFileIconSvg(name, isDir, isExpanded) {
    if (isDir) {
        if (isExpanded) {
            return `<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-color)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path><line x1="9" y1="13" x2="15" y2="13"></line></svg>`;
        }
        return `<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--accent-color)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path></svg>`;
    }

    const ext = name.split('.').pop().toLowerCase();
    const lowerName = name.toLowerCase();

    if (lowerName === 'cargo.toml' || lowerName === 'cargo.lock') {
        return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#f97316" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><circle cx="12" cy="12" r="3"></circle><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path></svg>`;
    }
    if (lowerName === '.gitignore' || lowerName.includes('git')) {
        return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#f43f5e" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>`;
    }
    if (lowerName.includes('docker') || lowerName.endsWith('.dockerfile')) {
        return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#38bdf8" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><path d="M2 13h20"></path><path d="M20 13v6a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-6"></path><path d="M4 13V9a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v4"></path></svg>`;
    }

    switch (ext) {
        case 'rs':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#ea580c" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><polygon points="12 2 2 7 12 12 22 7 12 2"></polygon><polyline points="2 17 12 22 22 17"></polyline><polyline points="2 12 12 17 22 12"></polyline></svg>`;
        case 'js':
        case 'jsx':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#eab308" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><rect x="3" y="3" width="18" height="18" rx="2"></rect><path d="M16 8v8"></path><path d="M12 16v-4"></path></svg>`;
        case 'ts':
        case 'tsx':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#3b82f6" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><rect x="3" y="3" width="18" height="18" rx="2"></rect><path d="M8 8h4v8"></path></svg>`;
        case 'html':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#f97316" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><polyline points="16 18 22 12 16 6"></polyline><polyline points="8 6 2 12 8 18"></polyline></svg>`;
        case 'css':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#06b6d4" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><path d="M4 3h16l-1.5 14L12 20l-6.5-3L4 3z"></path></svg>`;
        case 'md':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#60a5fa" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><rect x="3" y="5" width="18" height="14" rx="2"></rect><path d="M7 15V9l3 3 3-3v6"></path><path d="M17 12l-2 3h4"></path></svg>`;
        case 'json':
        case 'toml':
        case 'yaml':
        case 'yml':
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#a855f7" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><circle cx="12" cy="12" r="3"></circle><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path></svg>`;
        default:
            return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#94a3b8" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"></path><polyline points="13 2 13 9 20 9"></polyline></svg>`;
    }
}

function renderDirectoryItems(files, dirPath, level, container) {
    const sorted = [...files].sort((a, b) => {
        if (a.is_dir && !b.is_dir) return -1;
        if (!a.is_dir && b.is_dir) return 1;
        return a.name.localeCompare(b.name);
    });

    sorted.forEach(f => {
        const itemContainer = document.createElement('div');
        itemContainer.className = 'tree-item-wrapper';

        const item = document.createElement('div');
        item.className = `file-item ${f.is_dir ? 'is-dir' : ''}`;
        item.setAttribute('data-path', f.path);
        if (selectedPaths.has(f.path)) {
            item.classList.add('selected');
        }
        item.draggable = !f.is_dir;

        let indentHtml = '';
        for (let i = 0; i < level; i++) {
            indentHtml += `<span class="tree-indent-guide"></span>`;
        }

        const isExpanded = expandedFolders.has(f.path);
        const foldIconHtml = f.is_dir
            ? `<span class="tree-fold-icon ${isExpanded ? 'expanded' : ''}" data-folder="${escapeHtml(f.path)}"><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" class="tree-fold-svg"><polyline points="9 18 15 12 9 6"></polyline></svg></span>`
            : `<span style="display:inline-block; width: 16px;"></span>`;

        const fileIconSvg = getFileIconSvg(f.name, f.is_dir, isExpanded);
        const sizeStr = f.is_dir ? '' : formatBytes(f.size);

        let gitSt = currentGitStatuses.get(f.path) || currentGitStatuses.get(f.name);
        if (!gitSt && currentGitRepoRoot) {
            let normFPath = f.path.replace(/\\/g, '/');
            let normRoot = currentGitRepoRoot.replace(/\\/g, '/');
            let relPath = normFPath;
            if (normFPath.startsWith(normRoot)) {
                relPath = normFPath.slice(normRoot.length).replace(/^\/+/, '');
            }
            gitSt = currentGitStatuses.get(relPath);
            if (!gitSt) {
                for (let [p, st] of currentGitStatuses.entries()) {
                    let normP = p.replace(/\\/g, '/');
                    if (normP === relPath || normP === f.name || normP.endsWith('/' + f.name)) {
                        gitSt = st;
                        break;
                    }
                }
            }
        }

        const gitBadgeHtml = gitSt ? `<span class="git-badge git-${gitSt.toLowerCase()}">${gitSt}</span>` : '';
        const gitDiffBtnHtml = gitSt === 'M' ? `<span class="btn-mini btn-diff" title="View Git Diff">Diff</span>` : '';
        const isChecked = selectedPaths.has(f.path) ? 'checked' : '';
        const nameGitClass = gitSt ? `git-file-${gitSt.toLowerCase()}` : '';

        item.innerHTML = `
            ${indentHtml}
            ${foldIconHtml}
            <span class="file-icon-badge">${fileIconSvg}</span>
            <input type="checkbox" class="file-checkbox" ${isChecked}>
            <span class="name ${nameGitClass}">${escapeHtml(f.name)}</span>
            ${gitBadgeHtml}
            <span class="size">${sizeStr}</span>
            <div class="actions-mini">
                ${gitDiffBtnHtml}
                <span class="btn-mini btn-rename">R</span>
                <span class="btn-mini btn-delete">D</span>
            </div>
        `;

        const foldIcon = item.querySelector('.tree-fold-icon');
        if (foldIcon) {
            foldIcon.addEventListener('click', async function (e) {
                e.stopPropagation();
                await toggleFolderExpansion(f.path, itemContainer, level);
            });
        }

        const diffBtn = item.querySelector('.btn-diff');
        if (diffBtn) {
            diffBtn.addEventListener('click', function (e) {
                e.stopPropagation();
                openGitDiff(f.path);
            });
        }

        const checkbox = item.querySelector('.file-checkbox');
        checkbox.addEventListener('click', function (e) { e.stopPropagation(); });
        checkbox.addEventListener('change', function (e) {
            e.stopPropagation();
            if (this.checked) {
                selectedPaths.add(f.path);
            } else {
                selectedPaths.delete(f.path);
            }
            isExplicitMultiSelectMode = true;
            lastClickedPath = f.path;
            updateSelectionUI();
        });

        item.addEventListener('dragstart', function (e) {
            if (!f.is_dir) {
                e.dataTransfer.setData('text/plain', f.path);
            }
        });

        const renameBtn = item.querySelector('.btn-rename');
        const deleteBtn = item.querySelector('.btn-delete');
        renameBtn.addEventListener('click', function (e) {
            e.stopPropagation();
            renameEntry(f.path);
        });
        deleteBtn.addEventListener('click', function (e) {
            e.stopPropagation();
            deleteEntry(f.path);
        });

        item.addEventListener('contextmenu', function (e) {
            e.preventDefault();
            e.stopPropagation();
            showContextMenu(e.clientX, e.clientY, f.path, f.is_dir, f.name);
        });

        item.addEventListener('click', async function (e) {
            if (e.ctrlKey || e.metaKey || e.shiftKey) {
                e.stopPropagation();
                toggleSelection(f.path, e.ctrlKey || e.metaKey, e.shiftKey, files);
                return;
            }

            if (selectedPaths.size > 0) {
                e.stopPropagation();
                if (selectedPaths.has(f.path)) {
                    selectedPaths.delete(f.path);
                } else {
                    selectedPaths.add(f.path);
                }
                updateSelectionUI();
                return;
            }

            selectedPaths.clear();
            updateSelectionUI();

            if (f.is_dir) {
                const isWorkspaceTree = container && (container.id === 'workspace-file-tree' || container.closest('#workspace-file-tree'));
                if (isWorkspaceTree) {
                    await toggleFolderExpansion(f.path, itemContainer, level);
                } else {
                    currentPath = f.path;
                    document.getElementById('path-input').value = currentPath;
                    loadDirectory(currentRemote, currentPath);
                }
            } else {
                openFile(f.path);
            }
        });

        itemContainer.appendChild(item);

        if (f.is_dir && isExpanded) {
            const subContainer = document.createElement('div');
            subContainer.className = 'tree-subfolder';
            subContainer.setAttribute('data-parent', f.path);
            const children = folderChildrenMap.get(f.path) || [];
            renderDirectoryItems(children, f.path, level + 1, subContainer);
            itemContainer.appendChild(subContainer);
        }

        container.appendChild(itemContainer);
    });
}

async function toggleFolderExpansion(folderPath, itemContainer, level) {
    if (expandedFolders.has(folderPath)) {
        expandedFolders.delete(folderPath);
        const sub = itemContainer.querySelector('.tree-subfolder');
        if (sub) sub.remove();
        const foldIcon = itemContainer.querySelector('.tree-fold-icon');
        if (foldIcon) foldIcon.classList.remove('expanded');
    } else {
        expandedFolders.add(folderPath);
        const foldIcon = itemContainer.querySelector('.tree-fold-icon');
        if (foldIcon) foldIcon.classList.add('expanded');

        let children = folderChildrenMap.get(folderPath);
        if (!children) {
            try {
                const url = `/api/files?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(folderPath)}`;
                const res = await fetch(url);
                if (res.ok) {
                    children = await res.json();
                    folderChildrenMap.set(folderPath, children);
                } else {
                    children = [];
                }
            } catch (e) {
                children = [];
            }
        }

        let subContainer = itemContainer.querySelector('.tree-subfolder');
        if (!subContainer) {
            subContainer = document.createElement('div');
            subContainer.className = 'tree-subfolder';
            subContainer.setAttribute('data-parent', folderPath);
            itemContainer.appendChild(subContainer);
        } else {
            subContainer.innerHTML = '';
        }
        renderDirectoryItems(children || [], folderPath, level + 1, subContainer);
    }
}

async function expandAllFolders() {
    if (!currentRemote || !currentPath) return;
    showToast('Expanding all subfolders...', 'info');

    async function recursivelyExpand(dirPath) {
        expandedFolders.add(dirPath);
        let children = folderChildrenMap.get(dirPath);
        if (!children) {
            try {
                const res = await fetch(`/api/files?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(dirPath)}`);
                if (res.ok) {
                    children = await res.json();
                    folderChildrenMap.set(dirPath, children);
                }
            } catch (_) {}
        }
        if (children) {
            for (let c of children) {
                if (c.is_dir) {
                    await recursivelyExpand(c.path);
                }
            }
        }
    }

    const rootFiles = folderChildrenMap.get(currentPath) || currentRootFiles;
    for (let f of rootFiles) {
        if (f.is_dir) {
            await recursivelyExpand(f.path);
        }
    }

    renderFileTree(rootFiles, currentPath);
}

function collapseAllFolders() {
    expandedFolders.clear();
    const rootFiles = folderChildrenMap.get(currentPath) || currentRootFiles;
    renderFileTree(rootFiles, currentPath);
    showToast('Collapsed all folders', 'info');
}

const selectedPaths = new Set();
let lastClickedPath = null;
let isExplicitMultiSelectMode = false;

function toggleSelection(path, isCtrl, isShift, allFiles) {
    isExplicitMultiSelectMode = true;
    if (isShift && lastClickedPath && allFiles) {
        const idx1 = allFiles.findIndex(f => f.path === lastClickedPath);
        const idx2 = allFiles.findIndex(f => f.path === path);
        if (idx1 !== -1 && idx2 !== -1) {
            const start = Math.min(idx1, idx2);
            const end = Math.max(idx1, idx2);
            for (let i = start; i <= end; i++) {
                selectedPaths.add(allFiles[i].path);
            }
        }
    } else if (isCtrl) {
        if (selectedPaths.has(path)) {
            selectedPaths.delete(path);
        } else {
            selectedPaths.add(path);
        }
    } else {
        selectedPaths.clear();
        selectedPaths.add(path);
    }
    lastClickedPath = path;
    updateSelectionUI();
}

function clearSelection() {
    isExplicitMultiSelectMode = false;
    selectedPaths.clear();
    lastClickedPath = null;
    updateSelectionUI();
}

function updateSelectionUI() {
    const bar = document.getElementById('bulk-actions-bar');
    const label = document.getElementById('bulk-count-label');
    const tree = document.getElementById('file-tree');
    const count = selectedPaths.size;

    if (count > 0) {
        if (tree) tree.classList.add('has-selection');
        if (bar) bar.style.display = 'flex';
        if (label) label.textContent = `${count} selected`;
    } else {
        if (tree) tree.classList.remove('has-selection');
        if (bar) bar.style.display = 'none';
    }

    const items = document.querySelectorAll('.file-item');
    items.forEach(el => {
        const itemPath = el.getAttribute('data-path');
        const isSel = !!(itemPath && selectedPaths.has(itemPath));
        if (isSel) {
            el.classList.add('selected');
        } else {
            el.classList.remove('selected');
        }
        const cb = el.querySelector('.file-checkbox');
        if (cb) {
            cb.checked = isSel;
        }
    });
}

function initSidebarResizer() {
    const resizer = document.getElementById('sidebar-resizer');
    const sidebar = document.querySelector('.sidebar');
    if (!resizer || !sidebar) return;

    const savedWidth = localStorage.getItem('ferrite_sidebar_width');
    if (savedWidth) {
        sidebar.style.width = `${savedWidth}px`;
    }

    let isDragging = false;

    resizer.addEventListener('mousedown', function (e) {
        e.preventDefault();
        e.stopPropagation();
        isDragging = true;
        resizer.classList.add('is-dragging');
        document.body.classList.add('is-resizing');
    });

    document.addEventListener('mousemove', function (e) {
        if (!isDragging) return;
        e.preventDefault();
        const sidebarLeft = sidebar.getBoundingClientRect().left;
        const newWidth = Math.min(Math.max(e.clientX - sidebarLeft, 180), 700);
        sidebar.style.width = `${newWidth}px`;
        localStorage.setItem('ferrite_sidebar_width', newWidth);

        if (typeof editor !== 'undefined' && editor && editor.layout) {
            editor.layout();
        }
        if (typeof fitAddon !== 'undefined' && fitAddon && fitAddon.fit) {
            fitAddon.fit();
        }
    });

    document.addEventListener('mouseup', function () {
        if (isDragging) {
            isDragging = false;
            resizer.classList.remove('is-dragging');
            document.body.classList.remove('is-resizing');

            if (typeof editor !== 'undefined' && editor && editor.layout) {
                editor.layout();
            }
            if (typeof fitAddon !== 'undefined' && fitAddon && fitAddon.fit) {
                fitAddon.fit();
            }
        }
    });
}

async function downloadSelectedZip() {
    if (selectedPaths.size === 0 || !currentRemote) return;
    const paths = Array.from(selectedPaths);
    showToast(`Generating ZIP for ${paths.length} item(s)...`, 'info');

    try {
        const res = await fetch('/api/download/zip', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ remote: currentRemote, paths })
        });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'ZIP generation failed');
        }
        const blob = await res.blob();
        const url = window.URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = 'ferrite_archive.zip';
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        window.URL.revokeObjectURL(url);
        showToast('ZIP downloaded successfully', 'success');
    } catch (err) {
        showToast(`ZIP download error: ${err.message}`, 'error');
    }
}

async function deleteSelectedItems() {
    if (selectedPaths.size === 0 || !currentRemote) return;
    const paths = Array.from(selectedPaths);
    const confirmed = await customConfirm(`Delete ${paths.length} selected item(s)?`, 'Delete Confirmation');
    if (!confirmed) return;

    showToast(`Deleting ${paths.length} item(s)...`, 'info');
    let successCount = 0;
    for (const path of paths) {
        try {
            const url = `/api/entry?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(path)}`;
            const res = await fetch(url, { method: 'DELETE' });
            if (res.ok) successCount++;
        } catch (_) {}
    }
    showToast(`Deleted ${successCount} item(s)`, 'success');
    clearSelection();
    loadDirectory(currentRemote, currentPath);
}

let ctxTargetPath = null;
let ctxTargetIsDir = false;
let ctxTargetName = '';

function showContextMenu(x, y, path, isDir, name) {
    ctxTargetPath = path;
    ctxTargetIsDir = isDir;
    ctxTargetName = name;

    const menu = document.getElementById('context-menu');
    const dlItem = document.getElementById('ctx-download');
    const bulkDlItem = document.getElementById('ctx-bulk-download');
    const bulkDelItem = document.getElementById('ctx-bulk-delete');
    const toggleSelectBtn = document.getElementById('ctx-toggle-select');

    const openWsItem = document.getElementById('ctx-open-workspace');
    if (openWsItem) {
        openWsItem.style.display = (isDir && selectedPaths.size <= 1) ? 'block' : 'none';
    }

    if (toggleSelectBtn) {
        toggleSelectBtn.textContent = selectedPaths.has(path) ? 'Deselect Item' : 'Select Item';
    }

    if (selectedPaths.size > 1) {
        if (dlItem) dlItem.style.display = 'none';
        if (bulkDlItem) {
            bulkDlItem.style.display = 'block';
            bulkDlItem.textContent = `Download Selected (${selectedPaths.size} items as ZIP)`;
        }
        if (bulkDelItem) {
            bulkDelItem.style.display = 'block';
            bulkDelItem.textContent = `Delete Selected (${selectedPaths.size} items)`;
        }
    } else {
        if (dlItem) dlItem.style.display = isDir ? 'none' : 'block';
        if (bulkDlItem) bulkDlItem.style.display = 'none';
        if (bulkDelItem) bulkDelItem.style.display = 'none';
    }

    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    menu.style.display = 'block';
}

function hideContextMenu() {
    const menu = document.getElementById('context-menu');
    if (menu) menu.style.display = 'none';
}

async function uploadFiles(files) {
    if (!currentRemote) return;
    showToast(`Uploading ${files.length} file(s)...`, 'info');
    const formData = new FormData();
    for (let i = 0; i < files.length; i++) {
        formData.append('files', files[i]);
    }
    try {
        const url = `/api/upload?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(currentPath)}`;
        const res = await fetch(url, {
            method: 'POST',
            body: formData
        });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Upload failed');
        }
        showToast('Upload complete', 'success');
        loadDirectory(currentRemote, currentPath);
    } catch (err) {
        showToast(`Upload error: ${err.message}`, 'error');
    }
}

function downloadFile(filePath) {
    if (!currentRemote) return;
    const url = `/api/download?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
    const a = document.createElement('a');
    a.href = url;
    a.download = getBasename(filePath);
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
}

function updateStatusBadge(text, stateClass) {
    const badge = document.getElementById('status-badge');
    badge.style.display = 'inline-block';
    badge.textContent = text;
    badge.className = `status-badge ${stateClass}`;
}

function getLanguageFromFilename(filename) {
    const ext = filename.split('.').pop().toLowerCase();
    const map = {
        'js': 'javascript', 'ts': 'typescript', 'json': 'json',
        'html': 'html', 'css': 'css', 'rs': 'rust', 'py': 'python',
        'sh': 'shell', 'yaml': 'yaml', 'yml': 'yaml', 'toml': 'toml',
        'md': 'markdown', 'xml': 'xml', 'c': 'c', 'cpp': 'cpp',
        'go': 'go', 'sql': 'sql', 'dockerfile': 'dockerfile'
    };
    return map[ext] || 'plaintext';
}

function formatBytes(bytes) {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
}

function escapeHtml(str) {
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// --- GIT LENS & VISUAL DIFF MODULE ---
let currentGitBranch = '';
let currentGitRepoRoot = '';
let currentGitStatuses = new Map();
let diffEditorInstance = null;
let isDiffActive = false;
let blameDebounceTimer = null;

async function fetchGitStatus(remote, path) {
    const gitBar = document.getElementById('git-status-bar');
    if (!remote || !path) {
        if (gitBar) gitBar.style.display = 'none';
        return;
    }

    try {
        const url = `/api/git/status?remote=${encodeURIComponent(remote)}&path=${encodeURIComponent(path)}`;
        const res = await fetch(url);
        if (!res.ok) return;

        const data = await res.json();
        currentGitStatuses.clear();

        if (data.is_git) {
            currentGitBranch = data.branch;
            currentGitRepoRoot = data.repo_root || '';
            let mCount = 0, uCount = 0;
            data.statuses.forEach(s => {
                currentGitStatuses.set(s.path, s.status);
                if (s.status === 'M') mCount++;
                if (s.status === 'U') uCount++;
            });

            if (gitBar) {
                gitBar.style.display = 'flex';
                const branchEl = document.getElementById('git-branch-name');
                if (branchEl) branchEl.textContent = data.branch;
                const countsEl = document.getElementById('git-status-counts');
                if (countsEl) {
                    countsEl.textContent = `[${mCount} M, ${uCount} U]`;
                }
            }
            updateVSCodeStatusBar(activeTabPath);
        } else {
            currentGitBranch = '';
            if (gitBar) gitBar.style.display = 'none';
            updateVSCodeStatusBar(activeTabPath);
        }
    } catch (err) {
        console.error('Git status error:', err);
    }
}

async function openGitDiff(filePath) {
    try {
        const url = `/api/git/diff?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(filePath)}`;
        const res = await fetch(url);
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to fetch git diff');
        }

        const data = await res.json();

        if (currentMode !== 'editor') {
            switchMode('editor');
        }

        const monacoContainer = document.getElementById('monaco-container');
        const diffContainer = document.getElementById('monaco-diff-container');
        const toggleBtn = document.getElementById('btn-toggle-diff');

        if (monacoContainer) monacoContainer.style.display = 'none';
        if (diffContainer) diffContainer.style.display = 'block';
        if (toggleBtn) {
            toggleBtn.style.display = 'inline-block';
            toggleBtn.textContent = 'Close Git Diff';
        }

        if (!diffEditorInstance) {
            diffEditorInstance = monaco.editor.createDiffEditor(diffContainer, {
                readOnly: true,
                renderSideBySide: true,
                theme: 'vs-dark',
                automaticLayout: true,
                fontFamily: 'JetBrains Mono, monospace',
                fontSize: 13,
            });
        }

        const lang = getLanguageFromFilename(filePath);
        const originalModel = monaco.editor.createModel(data.original, lang);
        const modifiedModel = monaco.editor.createModel(data.modified, lang);

        diffEditorInstance.setModel({
            original: originalModel,
            modified: modifiedModel
        });

        isDiffActive = true;
        document.getElementById('active-file-path').textContent = `${filePath} (Git Diff)`;
    } catch (err) {
        showToast(`Git diff error: ${err.message}`, 'error');
    }
}

function closeGitDiff() {
    const monacoContainer = document.getElementById('monaco-container');
    const diffContainer = document.getElementById('monaco-diff-container');
    const toggleBtn = document.getElementById('btn-toggle-diff');

    if (diffContainer) diffContainer.style.display = 'none';
    if (monacoContainer) monacoContainer.style.display = 'block';
    if (toggleBtn) {
        toggleBtn.textContent = 'View Git Diff';
        let gitSt = currentGitStatuses.get(activeTabPath);
        if (!gitSt) {
            toggleBtn.style.display = 'none';
        }
    }
    isDiffActive = false;
    if (activeTabPath) {
        document.getElementById('active-file-path').textContent = activeTabPath;
    }
}

function toggleGitDiff() {
    if (isDiffActive) {
        closeGitDiff();
    } else if (activeTabPath) {
        openGitDiff(activeTabPath);
    }
}

function fetchGitBlame(filePath, lineNumber) {
    if (blameDebounceTimer) clearTimeout(blameDebounceTimer);
    blameDebounceTimer = setTimeout(async () => {
        if (!currentRemote || !currentPath || !filePath || !currentGitBranch) return;

        try {
            const url = `/api/git/blame?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(currentPath)}&file=${encodeURIComponent(filePath)}&line=${lineNumber}`;
            const res = await fetch(url);
            if (!res.ok) return;

            const data = await res.json();
            const blameBar = document.getElementById('git-blame-bar');
            if (blameBar) {
                blameBar.style.display = 'flex';
                document.getElementById('blame-author').textContent = data.author;
                document.getElementById('blame-time').textContent = data.time_ago;
                document.getElementById('blame-summary').textContent = `• "${data.summary}"`;
                document.getElementById('blame-hash').textContent = `#${data.hash}`;
            }
        } catch (_) {}
    }, 200);
}

function openGitModal() {
    const modal = document.getElementById('git-modal');
    const branchEl = document.getElementById('git-modal-branch');
    const bodyEl = document.getElementById('git-modal-body');

    if (!modal || !bodyEl) return;

    if (branchEl) branchEl.textContent = currentGitBranch || 'N/A';
    bodyEl.innerHTML = '';

    if (currentGitStatuses.size === 0) {
        bodyEl.innerHTML = '<div class="empty-state">No modified or untracked files detected in this repository.</div>';
    } else {
        currentGitStatuses.forEach((status, path) => {
            const row = document.createElement('div');
            row.style.cssText = 'display: flex; align-items: center; justify-content: space-between; padding: 6px 10px; background: var(--bg-hover); border-radius: 4px; font-family: var(--font-mono); font-size: 11px;';
            const statusClass = `git-${status.toLowerCase()}`;
            row.innerHTML = `
                <div style="display: flex; align-items: center; gap: 8px;">
                    <span class="git-badge ${statusClass}">${status}</span>
                    <span style="color: var(--text-primary);">${escapeHtml(path)}</span>
                </div>
                <button class="btn-subtle" style="font-size: 9px; padding: 2px 6px;" onclick="document.getElementById('git-modal').style.display='none'; openGitDiff('${escapeHtml(path)}');">View Diff</button>
            `;
            bodyEl.appendChild(row);
        });
    }

    modal.style.display = 'flex';
}

window.addEventListener('DOMContentLoaded', function () {
    const gitStatusBar = document.getElementById('git-status-bar');
    if (gitStatusBar) {
        gitStatusBar.addEventListener('click', openGitModal);
    }
    const btnToggleDiff = document.getElementById('btn-toggle-diff');
    if (btnToggleDiff) {
        btnToggleDiff.addEventListener('click', toggleGitDiff);
    }
    const gitModalCloseX = document.getElementById('git-modal-close-x');
    if (gitModalCloseX) {
        gitModalCloseX.addEventListener('click', () => {
            document.getElementById('git-modal').style.display = 'none';
        });
    }
    const gitModalCloseBtn = document.getElementById('git-modal-close-btn');
    if (gitModalCloseBtn) {
        gitModalCloseBtn.addEventListener('click', () => {
            document.getElementById('git-modal').style.display = 'none';
        });
    }
});

// --- FULL-TEXT GLOBAL SEARCH MODULE (Ctrl+Shift+F) ---
function openGlobalSearchModal() {
    const modal = document.getElementById('global-search-modal');
    if (!modal) return;
    modal.style.display = 'flex';
    const input = document.getElementById('global-search-input');
    if (input) {
        input.focus();
        input.select();
    }
}

function closeGlobalSearchModal() {
    const modal = document.getElementById('global-search-modal');
    if (modal) modal.style.display = 'none';
}

async function performGlobalSearch() {
    if (!currentRemote) {
        showToast('Please select a remote connection first', 'error');
        return;
    }

    const queryInput = document.getElementById('global-search-input');
    const query = queryInput ? queryInput.value.trim() : '';
    if (!query) {
        showToast('Please enter a search query', 'error');
        return;
    }

    const isCase = document.getElementById('global-search-case')?.checked || false;
    const isRegex = document.getElementById('global-search-regex')?.checked || false;
    const isContent = document.getElementById('global-search-content')?.checked || false;
    const includes = document.getElementById('global-search-includes')?.value.trim() || '';
    const excludes = document.getElementById('global-search-excludes')?.value.trim() || '';

    const summaryEl = document.getElementById('global-search-summary');
    const resultsList = document.getElementById('global-search-results-list');

    if (summaryEl) summaryEl.textContent = 'Searching workspace...';
    if (resultsList) resultsList.innerHTML = '<div class="empty-state">Scanning files across workspace...</div>';

    try {
        let url = `/api/search/global?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(currentPath)}&query=${encodeURIComponent(query)}&case_sensitive=${isCase}&is_regex=${isRegex}&content=${isContent}`;
        if (includes) url += `&includes=${encodeURIComponent(includes)}`;
        if (excludes) url += `&excludes=${encodeURIComponent(excludes)}`;

        let res = await fetch(url);
        let matches = [];

        if (res.ok) {
            matches = await res.json();
        } else {
            // Fallback to /api/search
            const fallbackUrl = `/api/search?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(currentPath)}&query=${encodeURIComponent(query)}&content=${isContent}`;
            const fbRes = await fetch(fallbackUrl);
            if (fbRes.ok) {
                const rawMatches = await fbRes.json();
                matches = rawMatches.map(m => ({
                    file: typeof m === 'string' ? m : (m.path || m.file || String(m)),
                    line: 1,
                    text: `Match found in ${typeof m === 'string' ? m : (m.name || m.path)}`
                }));
            } else {
                throw new Error('Search request failed');
            }
        }

        if (!matches || matches.length === 0) {
            if (summaryEl) summaryEl.textContent = `No results found for "${query}"`;
            if (resultsList) resultsList.innerHTML = '<div class="empty-state">No matching lines found.</div>';
            return;
        }

        if (summaryEl) summaryEl.textContent = `Found ${matches.length} result(s) for "${query}"`;
        resultsList.innerHTML = '';

        const grouped = new Map();
        matches.forEach(m => {
            const fileKey = m.file;
            if (!grouped.has(fileKey)) grouped.set(fileKey, []);
            grouped.get(fileKey).push(m);
        });

        grouped.forEach((fileMatches, filePath) => {
            const groupEl = document.createElement('div');
            groupEl.className = 'global-search-group';

            const headerEl = document.createElement('div');
            headerEl.className = 'global-search-file-header';
            const fileIconSvg = `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg" style="margin-right: 4px; vertical-align: -2px;"><path d="M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"></path><polyline points="13 2 13 9 20 9"></polyline></svg>`;
            headerEl.innerHTML = `${fileIconSvg} <span>${escapeHtml(filePath)}</span> <span style="font-size: 10px; color: var(--text-subtle);">(${fileMatches.length})</span>`;
            headerEl.addEventListener('click', () => {
                closeGlobalSearchModal();
                openFile(filePath);
            });
            groupEl.appendChild(headerEl);

            fileMatches.forEach(m => {
                const lineEl = document.createElement('div');
                lineEl.className = 'global-search-match-line';

                let highlightedSnippet = escapeHtml(m.text || '');
                if (query) {
                    try {
                        const flags = isCase ? 'g' : 'gi';
                        const re = isRegex ? new RegExp(query, flags) : new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), flags);
                        highlightedSnippet = highlightedSnippet.replace(re, match => `<span class="match-text">${match}</span>`);
                    } catch (_) {}
                }

                lineEl.innerHTML = `
                    <span class="line-num">L${m.line || 1}</span>
                    <span class="line-snippet">${highlightedSnippet}</span>
                `;

                lineEl.addEventListener('click', async () => {
                    closeGlobalSearchModal();
                    await openFile(filePath);
                    if (editor && m.line) {
                        editor.setPosition({ lineNumber: m.line, column: 1 });
                        editor.revealLineInCenter(m.line);
                    }
                });

                groupEl.appendChild(lineEl);
            });

            resultsList.appendChild(groupEl);
        });
    } catch (err) {
        if (summaryEl) summaryEl.textContent = 'Search error';
        if (resultsList) resultsList.innerHTML = `<div class="empty-state" style="color: #ef4444;">Search failed: ${escapeHtml(err.message)}</div>`;
    }
}

/* ==========================================================================
   GITLENS ADVANCED ENGINE
   ========================================================================== */

let currentGitLensTab = 'changes';

function openGitLensModal() {
    const modal = document.getElementById('gitlens-modal');
    const branchEl = document.getElementById('gitlens-modal-branch');
    if (!modal) return;

    if (branchEl) branchEl.textContent = currentGitBranch || 'main';
    modal.style.display = 'flex';

    switchGitLensTab('changes');
    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    if (path) {
        fetchGitStatus(currentRemote, path);
    }
}

function switchGitLensTab(tab) {
    currentGitLensTab = tab;
    const views = {
        'changes': document.getElementById('gl-view-changes'),
        'history': document.getElementById('gl-view-history'),
        'file-history': document.getElementById('gl-view-file-history'),
        'branches': document.getElementById('gl-view-branches'),
    };
    const tabs = {
        'changes': document.getElementById('gl-tab-changes'),
        'history': document.getElementById('gl-tab-history'),
        'file-history': document.getElementById('gl-tab-file-history'),
        'branches': document.getElementById('gl-tab-branches'),
    };

    Object.keys(views).forEach(k => {
        if (views[k]) views[k].style.display = (k === tab) ? 'flex' : 'none';
        if (tabs[k]) {
            if (k === tab) tabs[k].classList.add('active');
            else tabs[k].classList.remove('active');
        }
    });

    if (tab === 'changes') renderGitLensChanges();
    else if (tab === 'history') fetchGitRepoHistory();
    else if (tab === 'file-history') fetchGitActiveFileHistory();
    else if (tab === 'branches') fetchGitBranches();
}

function renderGitLensChanges() {
    const list = document.getElementById('gitlens-changes-list');
    if (!list) return;
    list.innerHTML = '';

    if (currentGitStatuses.size === 0) {
        list.innerHTML = '<div class="empty-state">No modified or untracked files detected in repository</div>';
        return;
    }

    currentGitStatuses.forEach((status, path) => {
        const item = document.createElement('div');
        item.style.cssText = 'display: flex; align-items: center; justify-content: space-between; padding: 6px 10px; border-bottom: 1px solid var(--border); font-family: var(--font-mono); font-size: 11px;';

        const statusClass = `git-${status.toLowerCase()}`;
        const fileIcon = getFileIconSvg(getBasename(path), false, false);

        item.innerHTML = `
            <div style="display: flex; align-items: center; gap: 8px; overflow: hidden;">
                <span class="git-badge ${statusClass}">${status}</span>
                ${fileIcon}
                <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" title="${escapeHtml(path)}">${escapeHtml(path)}</span>
            </div>
            <div style="display: flex; gap: 4px; align-items: center;">
                <button class="btn-subtle" style="font-size: 10px; padding: 2px 6px;" title="View Diff">Diff</button>
                <button class="btn-subtle" style="font-size: 10px; padding: 2px 6px; color: #10b981;" title="Stage File">+</button>
                <button class="btn-subtle" style="font-size: 10px; padding: 2px 6px; color: #ef4444;" title="Revert Changes">↺</button>
            </div>
        `;

        const btns = item.querySelectorAll('button');
        btns[0].addEventListener('click', () => {
            document.getElementById('gitlens-modal').style.display = 'none';
            openGitDiff(path);
        });
        btns[1].addEventListener('click', () => stageGitFile(path));
        btns[2].addEventListener('click', () => revertGitFile(path));

        list.appendChild(item);
    });
}

async function commitGitChanges() {
    const input = document.getElementById('gitlens-commit-msg');
    const msg = input ? input.value.trim() : '';
    if (!msg) {
        showToast('Please enter a commit message', 'warning');
        return;
    }

    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    showToast('Committing changes...', 'info');

    try {
        const res = await fetch('/api/git/commit', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, message: msg })
        });
        const data = await res.json();
        if (!res.ok) throw new Error(data.error || 'Commit failed');

        if (input) input.value = '';
        showToast(`Committed: "${msg}"`, 'success');
        await fetchGitStatus(currentRemote, path);
        renderGitLensChanges();
    } catch (err) {
        showToast(`Git commit error: ${err.message}`, 'error');
    }
}

async function stageGitFile(filePath) {
    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    try {
        const res = await fetch('/api/git/stage', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, file: filePath })
        });
        if (!res.ok) throw new Error('Failed to stage file');
        showToast(`Staged ${getBasename(filePath)}`, 'success');
        await fetchGitStatus(currentRemote, path);
        renderGitLensChanges();
    } catch (err) {
        showToast(`Stage error: ${err.message}`, 'error');
    }
}

async function revertGitFile(filePath) {
    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    const confirmed = await customConfirm(`Are you sure you want to revert changes in ${getBasename(filePath)}? This cannot be undone.`, 'Revert Changes');
    if (!confirmed) return;

    try {
        const res = await fetch('/api/git/revert', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, file: filePath })
        });
        if (!res.ok) throw new Error('Failed to revert file');
        showToast(`Reverted ${getBasename(filePath)}`, 'info');
        await fetchGitStatus(currentRemote, path);
        renderGitLensChanges();
    } catch (err) {
        showToast(`Revert error: ${err.message}`, 'error');
    }
}

async function fetchGitRepoHistory() {
    const list = document.getElementById('gitlens-history-list');
    if (!list) return;
    list.innerHTML = '<div class="empty-state">Loading repository commits...</div>';

    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    try {
        const url = `/api/git/history?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(path)}&file=`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to load history');
        const data = await res.json();

        list.innerHTML = '';
        if (!data.commits || data.commits.length === 0) {
            list.innerHTML = '<div class="empty-state">No commits found</div>';
            return;
        }

        data.commits.forEach(c => {
            const card = document.createElement('div');
            card.style.cssText = 'padding: 8px 10px; border-bottom: 1px solid var(--border); display: flex; flex-direction: column; gap: 4px; font-family: var(--font-sans);';
            card.innerHTML = `
                <div style="display: flex; justify-content: space-between; align-items: center;">
                    <div style="display: flex; align-items: center; gap: 6px;">
                        <span style="font-family: var(--font-mono); font-size: 10px; color: var(--accent-color); background: var(--bg-hover); padding: 1px 5px; border-radius: 3px; border: 1px solid var(--border);">${c.hash}</span>
                        <span style="font-weight: 600; font-size: 12px; color: var(--text-primary);">${escapeHtml(c.author)}</span>
                    </div>
                    <span style="font-family: var(--font-mono); font-size: 10px; color: var(--text-muted);">${escapeHtml(c.time_ago)}</span>
                </div>
                <div style="font-size: 12px; color: var(--text-primary); margin-left: 2px;">${escapeHtml(c.summary)}</div>
            `;
            list.appendChild(card);
        });
    } catch (err) {
        list.innerHTML = `<div class="empty-state" style="color: #ef4444;">${escapeHtml(err.message)}</div>`;
    }
}

async function fetchGitActiveFileHistory() {
    const list = document.getElementById('gitlens-file-history-list');
    const titleEl = document.getElementById('gitlens-file-history-title');
    if (!list) return;

    if (!activeTabPath) {
        list.innerHTML = '<div class="empty-state">No file currently active in editor</div>';
        return;
    }

    if (titleEl) titleEl.textContent = `History for: ${activeTabPath}`;
    list.innerHTML = '<div class="empty-state">Loading file commits...</div>';

    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    try {
        const url = `/api/git/history?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(path)}&file=${encodeURIComponent(activeTabPath)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to load file history');
        const data = await res.json();

        list.innerHTML = '';
        if (!data.commits || data.commits.length === 0) {
            list.innerHTML = '<div class="empty-state">No commits found for this file</div>';
            return;
        }

        data.commits.forEach(c => {
            const card = document.createElement('div');
            card.style.cssText = 'padding: 8px 10px; border-bottom: 1px solid var(--border); display: flex; flex-direction: column; gap: 4px; font-family: var(--font-sans);';
            card.innerHTML = `
                <div style="display: flex; justify-content: space-between; align-items: center;">
                    <div style="display: flex; align-items: center; gap: 6px;">
                        <span style="font-family: var(--font-mono); font-size: 10px; color: var(--accent-color); background: var(--bg-hover); padding: 1px 5px; border-radius: 3px; border: 1px solid var(--border);">${c.hash}</span>
                        <span style="font-weight: 600; font-size: 12px; color: var(--text-primary);">${escapeHtml(c.author)}</span>
                    </div>
                    <span style="font-family: var(--font-mono); font-size: 10px; color: var(--text-muted);">${escapeHtml(c.time_ago)}</span>
                </div>
                <div style="font-size: 12px; color: var(--text-primary); margin-left: 2px;">${escapeHtml(c.summary)}</div>
            `;
            list.appendChild(card);
        });
    } catch (err) {
        list.innerHTML = `<div class="empty-state" style="color: #ef4444;">${escapeHtml(err.message)}</div>`;
    }
}

async function fetchGitBranches() {
    const list = document.getElementById('gitlens-branches-list');
    if (!list) return;
    list.innerHTML = '<div class="empty-state">Loading branches...</div>';

    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    try {
        const url = `/api/git/branches?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(path)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Failed to load branches');
        const data = await res.json();

        list.innerHTML = '';
        if (!data.branches || data.branches.length === 0) {
            list.innerHTML = '<div class="empty-state">No branches found</div>';
            return;
        }

        data.branches.forEach(b => {
            const item = document.createElement('div');
            item.style.cssText = 'display: flex; align-items: center; justify-content: space-between; padding: 8px 12px; border-bottom: 1px solid var(--border); font-family: var(--font-mono); font-size: 12px; cursor: pointer;';
            if (b.is_current) {
                item.style.background = 'rgba(96, 165, 250, 0.1)';
                item.style.color = 'var(--accent-color)';
                item.style.fontWeight = 'bold';
            }

            item.innerHTML = `
                <div style="display: flex; align-items: center; gap: 8px;">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="icon-svg"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg>
                    <span>${escapeHtml(b.name)}</span>
                </div>
                ${b.is_current ? '<span style="font-size: 10px; color: var(--accent-color); font-weight: bold;">(Active)</span>' : '<button class="btn-subtle" style="font-size: 9px; padding: 2px 6px;">Checkout</button>'}
            `;

            if (!b.is_current) {
                const checkoutBtn = item.querySelector('button');
                if (checkoutBtn) {
                    checkoutBtn.addEventListener('click', () => checkoutGitBranch(b.name));
                }
            }

            list.appendChild(item);
        });
    } catch (err) {
        list.innerHTML = `<div class="empty-state" style="color: #ef4444;">${escapeHtml(err.message)}</div>`;
    }
}

async function checkoutGitBranch(branchName) {
    const path = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;
    showToast(`Checking out branch ${branchName}...`, 'info');

    try {
        const res = await fetch('/api/git/checkout', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, branch: branchName })
        });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Checkout failed');
        }

        showToast(`Switched to branch ${branchName}`, 'success');
        await fetchGitStatus(currentRemote, path);
        fetchGitBranches();
    } catch (err) {
        showToast(`Checkout error: ${err.message}`, 'error');
    }
}

function openGlobalSearchModal() {
    const modal = document.getElementById('global-search-modal');
    const input = document.getElementById('global-search-query-input');
    if (modal) {
        modal.style.display = 'flex';
        if (input) {
            input.focus();
            input.select();
        }
    }
}

async function performGlobalSearch() {
    const queryInput = document.getElementById('global-search-query-input');
    const list = document.getElementById('global-search-results-list');
    const caseSensitive = document.getElementById('gs-case-sensitive')?.checked || false;
    const isRegex = document.getElementById('gs-is-regex')?.checked || false;
    const includes = document.getElementById('gs-includes')?.value || '';
    const excludes = document.getElementById('gs-excludes')?.value || '';

    const q = queryInput ? queryInput.value.trim() : '';
    if (!q) {
        if (list) list.innerHTML = '<div class="empty-state">Please enter a search query</div>';
        return;
    }

    if (list) list.innerHTML = '<div class="empty-state">Searching workspace files...</div>';

    const searchPath = currentWorkspacePath && currentWorkspacePath !== '/' ? currentWorkspacePath : currentPath;

    try {
        const url = `/api/search/global?remote=${encodeURIComponent(currentRemote)}&path=${encodeURIComponent(searchPath)}&query=${encodeURIComponent(q)}&case_sensitive=${caseSensitive}&is_regex=${isRegex}&includes=${encodeURIComponent(includes)}&excludes=${encodeURIComponent(excludes)}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error('Search failed');

        const matches = await res.json();
        if (!list) return;
        list.innerHTML = '';

        if (!matches || matches.length === 0) {
            list.innerHTML = `<div class="empty-state">No matches found for "${escapeHtml(q)}"</div>`;
            return;
        }

        // Group matches by file
        const grouped = new Map();
        matches.forEach(m => {
            if (!grouped.has(m.file)) grouped.set(m.file, []);
            grouped.get(m.file).push(m);
        });

        grouped.forEach((fileMatches, filePath) => {
            const card = document.createElement('div');
            card.style.cssText = 'background: var(--bg-card); border: 1px solid var(--border); border-radius: 6px; overflow: hidden; font-family: var(--font-mono); font-size: 11px; margin-bottom: 6px;';

            const header = document.createElement('div');
            header.style.cssText = 'padding: 6px 10px; background: var(--bg-panel); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; font-weight: 600; color: var(--accent-color);';
            const icon = getFileIconSvg(getBasename(filePath), false, false);
            header.innerHTML = `
                <div style="display: flex; align-items: center; gap: 6px; overflow: hidden;">
                    ${icon}
                    <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" title="${escapeHtml(filePath)}">${escapeHtml(filePath)}</span>
                </div>
                <span class="media-badge">${fileMatches.length} match${fileMatches.length > 1 ? 'es' : ''}</span>
            `;
            card.appendChild(header);

            const linesContainer = document.createElement('div');
            linesContainer.style.cssText = 'display: flex; flex-direction: column;';

            fileMatches.forEach(m => {
                const lineRow = document.createElement('div');
                lineRow.style.cssText = 'padding: 5px 12px; border-bottom: 1px solid rgba(255,255,255,0.03); cursor: pointer; display: flex; align-items: center; gap: 10px; transition: background 0.15s ease;';
                lineRow.addEventListener('mouseover', () => lineRow.style.background = 'var(--bg-hover)');
                lineRow.addEventListener('mouseout', () => lineRow.style.background = 'transparent');

                lineRow.innerHTML = `
                    <span style="color: var(--text-muted); font-size: 10px; min-width: 35px;">:${m.line}</span>
                    <span style="color: var(--text-primary); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(m.text)}</span>
                `;

                lineRow.addEventListener('click', () => {
                    const modal = document.getElementById('global-search-modal');
                    if (modal) modal.style.display = 'none';
                    openFile(m.file, m.line);
                });

                linesContainer.appendChild(lineRow);
            });

            card.appendChild(linesContainer);
            list.appendChild(card);
        });
    } catch (err) {
        if (list) list.innerHTML = `<div class="empty-state" style="color: #ef4444;">Search error: ${escapeHtml(err.message)}</div>`;
    }
}

// ---- User management (admin only) ----

function openUsersModal() {
    const modal = document.getElementById('users-modal');
    if (modal) modal.style.display = 'flex';
    fetchUsers();
    selectUser(null);
}

function closeUsersModal() {
    const modal = document.getElementById('users-modal');
    if (modal) modal.style.display = 'none';
}

async function fetchUsers() {
    try {
        const res = await fetch('/api/users');
        if (!res.ok) throw new Error('Failed to fetch users');
        usersCache = await res.json();
        renderUsersList();
    } catch (err) {
        showToast(`Error loading users: ${err.message}`, 'error');
    }
}

function renderUsersList() {
    const list = document.getElementById('users-list');
    if (!list) return;
    list.innerHTML = '';
    usersCache.forEach(u => {
        const row = document.createElement('div');
        row.style.cssText = 'display: flex; align-items: center; justify-content: space-between; gap: 6px; padding: 6px 8px; border-radius: 6px; cursor: pointer; font-family: var(--font-mono); font-size: 11px;' +
            (u.username === selectedUsername ? ' background: var(--bg-hover); color: var(--accent-color);' : ' color: var(--text-primary);');
        row.innerHTML = `
            <span style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">${escapeHtml(u.username)}${u.enabled ? '' : ' <span style="color: var(--text-subtle);">(disabled)</span>'}</span>
            <span style="font-size: 9px; color: var(--text-muted); text-transform: uppercase;">${escapeHtml(u.role)}</span>
        `;
        row.addEventListener('click', () => selectUser(u.username));
        list.appendChild(row);
    });
}

function addPermRow(grant) {
    const rows = document.getElementById('users-perm-rows');
    if (!rows) return;
    const row = document.createElement('div');
    row.className = 'perm-row';
    row.style.cssText = 'display: flex; gap: 6px; align-items: center;';

    const remoteSelect = document.createElement('select');
    remoteSelect.className = 'perm-remote dialog-input';
    remoteSelect.style.cssText = 'flex: 1; height: 30px; font-size: 11px; padding: 4px 6px;';
    remotesData.forEach(r => {
        const opt = document.createElement('option');
        opt.value = r.name;
        opt.textContent = r.name;
        remoteSelect.appendChild(opt);
    });

    const pathInput = document.createElement('input');
    pathInput.type = 'text';
    pathInput.className = 'perm-path dialog-input';
    pathInput.placeholder = '/path (or / for whole remote)';
    pathInput.style.cssText = 'flex: 1.4; height: 30px; font-size: 11px; padding: 4px 8px;';

    const writeLabel = document.createElement('label');
    writeLabel.style.cssText = 'display: flex; align-items: center; gap: 3px; font-size: 10px; color: var(--text-muted); white-space: nowrap;';
    writeLabel.innerHTML = '<input type="checkbox" class="perm-write"> Write';

    const removeBtn = document.createElement('button');
    removeBtn.className = 'btn-subtle';
    removeBtn.style.cssText = 'padding: 2px 8px; font-size: 11px;';
    removeBtn.textContent = '×';
    removeBtn.addEventListener('click', () => row.remove());

    row.appendChild(remoteSelect);
    row.appendChild(pathInput);
    row.appendChild(writeLabel);
    row.appendChild(removeBtn);
    rows.appendChild(row);

    if (grant) {
        remoteSelect.value = grant.remote;
        pathInput.value = grant.path;
        writeLabel.querySelector('input').checked = !!grant.write;
    }
}

function collectPermRows() {
    const rows = document.querySelectorAll('#users-perm-rows .perm-row');
    const grants = [];
    rows.forEach(row => {
        const remote = row.querySelector('.perm-remote').value;
        const path = row.querySelector('.perm-path').value.trim() || '/';
        const write = row.querySelector('.perm-write').checked;
        if (remote) grants.push({ remote, path, write });
    });
    return grants;
}

function selectUser(username) {
    selectedUsername = username;
    renderUsersList();

    const empty = document.getElementById('users-form-empty');
    const form = document.getElementById('users-form');
    const deleteBtn = document.getElementById('users-delete-btn');
    const saveBtn = document.getElementById('users-save-btn');
    const createBtn = document.getElementById('users-create-btn');
    const permRows = document.getElementById('users-perm-rows');
    const passwordHint = document.getElementById('users-form-password-hint');
    const enabledWrap = document.getElementById('users-form-enabled-wrap');
    const usernameInput = document.getElementById('users-form-username');

    empty.style.display = 'none';
    form.style.display = 'flex';
    permRows.innerHTML = '';

    if (username === null) {
        usernameInput.value = '';
        usernameInput.disabled = false;
        document.getElementById('users-form-password').value = '';
        document.getElementById('users-form-role').value = 'user';
        document.getElementById('users-form-allow-shell').checked = false;
        document.getElementById('users-form-enabled').checked = true;
        passwordHint.textContent = '';
        enabledWrap.style.display = 'none';
        deleteBtn.style.display = 'none';
        saveBtn.style.display = 'none';
        createBtn.style.display = '';
        return;
    }

    const u = usersCache.find(x => x.username === username);
    if (!u) return;

    usernameInput.value = u.username;
    usernameInput.disabled = true;
    document.getElementById('users-form-password').value = '';
    document.getElementById('users-form-role').value = u.role;
    document.getElementById('users-form-allow-shell').checked = !!u.allow_shell;
    document.getElementById('users-form-enabled').checked = !!u.enabled;
    passwordHint.textContent = '(leave blank to keep unchanged)';
    enabledWrap.style.display = '';
    (u.permissions || []).forEach(g => addPermRow(g));

    deleteBtn.style.display = '';
    saveBtn.style.display = '';
    createBtn.style.display = 'none';
}

async function createUserSubmit() {
    const username = document.getElementById('users-form-username').value.trim();
    const password = document.getElementById('users-form-password').value;
    const role = document.getElementById('users-form-role').value;
    const allow_shell = document.getElementById('users-form-allow-shell').checked;

    try {
        const res = await fetch('/api/users', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ username, password, role, allow_shell })
        });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to create user');
        }
        const permissions = collectPermRows();
        if (permissions.length > 0) {
            await fetch(`/api/users/${encodeURIComponent(username)}`, {
                method: 'PUT',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ permissions })
            });
        }
        showToast(`User '${username}' created`, 'success');
        await fetchUsers();
        selectUser(username);
    } catch (err) {
        showToast(`Error: ${err.message}`, 'error');
    }
}

async function saveUserSubmit() {
    if (!selectedUsername) return;
    const password = document.getElementById('users-form-password').value;
    const role = document.getElementById('users-form-role').value;
    const allow_shell = document.getElementById('users-form-allow-shell').checked;
    const enabled = document.getElementById('users-form-enabled').checked;
    const permissions = collectPermRows();

    const payload = { role, allow_shell, enabled, permissions };
    if (password.trim()) payload.password = password;

    try {
        const res = await fetch(`/api/users/${encodeURIComponent(selectedUsername)}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload)
        });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to update user');
        }
        showToast(`User '${selectedUsername}' updated`, 'success');
        await fetchUsers();
        selectUser(selectedUsername);
    } catch (err) {
        showToast(`Error: ${err.message}`, 'error');
    }
}

async function deleteUserSubmit() {
    if (!selectedUsername) return;
    const confirmed = await customConfirm(`Delete user '${selectedUsername}'? This cannot be undone.`, 'Delete User');
    if (!confirmed) return;

    try {
        const res = await fetch(`/api/users/${encodeURIComponent(selectedUsername)}`, { method: 'DELETE' });
        if (!res.ok) {
            const errData = await res.json().catch(() => ({}));
            throw new Error(errData.error || 'Failed to delete user');
        }
        showToast('User deleted', 'success');
        await fetchUsers();
        selectUser(null);
    } catch (err) {
        showToast(`Error: ${err.message}`, 'error');
    }
}
