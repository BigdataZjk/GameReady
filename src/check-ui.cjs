// Run with Playwright installed; uses an isolated headless Edge and mocked Tauri IPC.
// No Steam process, saved credentials, or real NAS is accessed.
const assert = require('node:assert/strict');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

(async () => {
  const browser = await chromium.launch({ channel: 'msedge', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 480, height: 660 } });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => {
      const listeners = {};
      const clone = value => structuredClone(value);
      const snapshot = {
        state_version: 1,
        config: { enabled: true, host: 'nas.example.test', port: 2022, username: 'fixture',
          password: 'fixture-nas-password', remote_path: '/backup/accounts.txt' },
        config_verified: true, inventory_error: null, status: 'pending', message: '测试数据',
        last_success_ms: null, next_retry_ms: 0,
        queue: [{ account: 'ready', source: '登录确认', ts_ms: Date.now(), login_confirmed: true }],
        missing_password_accounts: ['missing', 'still_missing'],
        unconfirmed_password_accounts: ['unconfirmed']
      };
      const emit = (name, payload) => (listeners[name] || []).forEach(fn => fn({ payload: clone(payload) }));
      const publish = () => { snapshot.state_version++; emit('backup-changed', snapshot); };
      window.backupFixture = { snapshot, calls: [], accounts: [], loginResult: null, loginError: null,
        failSync: false, failSave: false, emit, publish };
      window.__TAURI__ = {
        event: { listen: async (name, fn) => { (listeners[name] ||= []).push(fn); return () => {}; } },
        dialog: { open: async () => null },
        window: { getCurrentWindow: () => ({ onCloseRequested: async () => {}, minimize() {}, hide() {}, close() {} }) },
        core: { invoke: async (command, args = {}) => {
          window.backupFixture.calls.push({ command, args: clone(args) });
          if(command === 'backup_get') return clone(snapshot);
          if(command === 'get_status') return { paths_valid: {}, steam_running: false, lol_guard_on: false };
          if(command === 'get_settings') return { accent: '#00e5ff', close_behavior: 'tray' };
          if(command === 'accounts_list') return clone(window.backupFixture.accounts);
          if(command === 'list_logs') return [];
          if(command === 'switch_account' || command === 'login_new') {
            await new Promise(resolve => setTimeout(resolve, 40));
            if(window.backupFixture.loginError) throw { message: window.backupFixture.loginError };
            return clone(window.backupFixture.loginResult);
          }
          if(command === 'backup_save_password') {
            await new Promise(resolve => setTimeout(resolve, 40));
            if(window.backupFixture.failSave) throw { message: '模拟本地保存失败' };
            snapshot.missing_password_accounts = snapshot.missing_password_accounts.filter(a => a !== args.account);
            snapshot.unconfirmed_password_accounts.push(args.account);
            snapshot.message = args.account + '：密码已补充，可点击同步';
            publish(); emit('steam-accounts-changed', null);
            return clone(snapshot);
          }
          if(command === 'backup_sync') {
            snapshot.status = 'syncing'; publish();
            await new Promise(resolve => setTimeout(resolve, 80));
            if(window.backupFixture.failSync) {
              snapshot.status = 'network_error'; snapshot.message = '模拟网络中断，记录保留'; publish();
              throw { message: snapshot.message };
            }
            snapshot.queue = snapshot.queue.filter(a => args.account && a.account !== args.account);
            snapshot.unconfirmed_password_accounts = snapshot.unconfirmed_password_accounts.filter(a => args.account && a !== args.account);
            snapshot.status = 'idle'; snapshot.last_success_ms = Date.now();
            snapshot.message = args.account ? args.account + '：同步成功' : '同步成功：已同步 2 个账号；1 个账号待补充密码';
            publish();
            return clone(snapshot);
          }
          throw new Error('Unexpected IPC: ' + command);
        } }
      };
    });
    await page.goto(pathToFileURL(path.resolve(__dirname, '../assets/index.html')).href + '#steam');
    // App startup intentionally selects the first game tab instead of the URL hash.
    await page.locator('#nav-steam').click();
    await page.locator('#openBackup').click();
    await page.locator('#backupQueueTab').click();
    const card = account => page.locator(`.backup-account[data-account="${account}"]`);
    async function assertFeedbackVisible(tone) {
      const feedback = page.locator('#backupFeedback');
      assert.equal(await feedback.getAttribute('data-tone'), tone);
      assert.equal(await feedback.evaluate(node => {
        const rect = node.getBoundingClientRect();
        const dialog = node.closest('.backup-modal').getBoundingClientRect();
        const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
        return rect.top >= dialog.top && rect.bottom <= dialog.bottom && (hit === node || node.contains(hit));
      }), true, 'Result must be visible inside the dialog, not clipped or covered');
    }
    assert.equal(await page.locator('#backupQueueCount').textContent(), '4');
    assert.equal(await card('unconfirmed').locator('.backup-state').textContent(), '待验证');
    assert.equal(await card('unconfirmed').locator('button').textContent(), '同步');
    assert.equal(await card('missing').locator('button').textContent(), '补充密码');
    assert.equal(await page.getByRole('button', { name: '验证登录', exact: true }).count(), 0);
    assert.equal(await page.locator('#backupSync').textContent(), '全部同步');

    await card('missing').getByRole('button', { name: '补充密码' }).click();
    await page.locator('#backupAccountPassword').fill(' saved ---- password ');
    await page.evaluate(() => window.backupFixture.publish());
    assert.equal(await page.locator('#backupAccountPassword').inputValue(), ' saved ---- password ');
    assert.equal(await page.locator('#backupAccountPassword').evaluate(el => document.activeElement === el), true);
    await page.screenshot({ path: path.resolve(__dirname, '../target/backup-password-ui.png') });
    await card('missing').getByRole('button', { name: '确认', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector('#backupAccountPassword'));
    assert.equal(await card('missing').locator('button').textContent(), '同步');
    assert.equal(await card('missing').locator('.backup-state').textContent(), '待验证');
    const passwordCalls = await page.evaluate(() => window.backupFixture.calls.filter(c => c.command === 'backup_save_password'));
    assert.deepEqual(passwordCalls[0].args, { account: 'missing', password: ' saved ---- password ' });
    assert.equal(await page.evaluate(() => window.backupFixture.calls.some(c => ['login_new','switch_account','backup_sync'].includes(c.command))), false);
    console.log('PASS password-only save, state labels, live button update, draft/focus preservation');

    await card('missing').getByRole('button', { name: '同步', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector('.backup-account[data-account="missing"]'));
    assert.equal(await card('unconfirmed').count(), 1);
    assert.equal(await page.locator('#backupQueueCount').textContent(), '3');
    assert.match(await page.locator('#backupFeedback').textContent(), /missing：同步成功/);
    assert.deepEqual(await page.evaluate(() => window.backupFixture.calls.filter(c => c.command === 'backup_sync').at(-1).args), { account: 'missing' });
    // A slow stale response must not reinsert an already synchronized account.
    await page.evaluate(() => {
      const f = window.backupFixture, old = structuredClone(f.snapshot);
      old.state_version--; old.unconfirmed_password_accounts.push('missing');
      f.emit('backup-changed', old);
    });
    assert.equal(await card('missing').count(), 0);
    await page.evaluate(() => {
      const f = window.backupFixture;
      f.snapshot.missing_password_accounts.push(...Array.from({ length: 12 }, (_, i) => 'history_' + i));
      f.snapshot.message = '后台正在同步其他账号';
      f.publish();
      document.querySelector('#backupMask .modal-body').scrollTop = 0;
    });
    assert.match(await page.locator('#backupFeedback').textContent(), /missing：同步成功/);
    await assertFeedbackVisible('success');
    await page.screenshot({ path: path.resolve(__dirname, '../target/backup-single-success-ui.png') });
    await page.evaluate(() => {
      const f = window.backupFixture;
      f.snapshot.missing_password_accounts = f.snapshot.missing_password_accounts.filter(a => !a.startsWith('history_'));
      f.publish();
    });
    console.log('PASS scoped single-account sync, result feedback, removal, stale event rejection');

    await page.evaluate(() => window.backupFixture.failSync = true);
    await card('unconfirmed').getByRole('button', { name: '同步', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('#backupFeedback').textContent.includes('模拟网络中断'));
    assert.equal(await card('unconfirmed').locator('button').isEnabled(), true);
    assert.equal(await page.locator('#backupQueueCount').textContent(), '3');
    await assertFeedbackVisible('error');
    await page.evaluate(() => window.backupFixture.failSync = false);
    await page.locator('#backupSync').click();
    await page.waitForFunction(() => document.querySelector('#backupQueueCount').textContent === '1');
    assert.equal(await card('still_missing').count(), 1);
    assert.equal(await page.locator('#backupSync').isDisabled(), true);
    assert.deepEqual(await page.evaluate(() => window.backupFixture.calls.filter(c => c.command === 'backup_sync').at(-1).args), { account: null });
    assert.match(await page.locator('#backupFeedback').textContent(), /同步成功/);
    await assertFeedbackVisible('success');
    await page.screenshot({ path: path.resolve(__dirname, '../target/backup-all-success-ui.png') });
    console.log('PASS failed sync retention/retry, all-unsynced request, missing-password retention');

    await card('still_missing').getByRole('button', { name: '补充密码' }).click();
    await page.locator('#backupAccountPassword').fill('retry-password');
    await page.evaluate(() => window.backupFixture.failSave = true);
    await page.locator('#backupAccountPassword').press('Enter');
    await page.waitForFunction(() => document.querySelector('#backupFeedback').textContent.includes('模拟本地保存失败'));
    assert.equal(await page.locator('#backupAccountPassword').inputValue(), 'retry-password');
    assert.equal(await card('still_missing').getByRole('button', { name: '确认', exact: true }).isEnabled(), true);
    await card('still_missing').getByRole('button', { name: '取消', exact: true }).click();
    assert.equal(await page.locator('#backupAccountPassword').count(), 0);
    assert.equal(await page.evaluate(() => backupPasswordDraft), '');
    console.log('PASS save error feedback, retry input retention, cancel clears password');

    await page.screenshot({ path: path.resolve(__dirname, '../target/backup-sync-ui.png') });
    await page.locator('#closeBackup').click();
    await page.evaluate(() => {
      const f = window.backupFixture;
      f.accounts = [{ steam_id64: '76561198000000001', account_name: 'saved_player',
        persona_name: 'Saved Player', timestamp: 1728000000 }];
      f.loginResult = { name: 'Saved Player', message: '已登录 Saved Player，配置覆盖完成', ok: true };
      f.emit('steam-accounts-changed', null);
    });
    const savedLogin = page.locator('#acctCards .acct').getByRole('button', { name: '登录', exact: true });
    async function assertLoginFeedback(message, ok) {
      await page.waitForFunction(text => document.querySelector('#tkTxt').textContent === text, message);
      assert.equal(await page.locator('#ticker').getAttribute('class'), 'ticker ' + (ok ? 'ok' : 'err'));
      assert.equal(await page.evaluate(() => progTimer), null, 'Completion must stop progress messages');
    }
    await savedLogin.click();
    await assertLoginFeedback('已登录 Saved Player，配置覆盖完成', true);
    assert.deepEqual(await page.evaluate(() => window.backupFixture.calls.find(c => c.command === 'switch_account').args),
      { sid64: '76561198000000001' });
    await page.evaluate(() => window.backupFixture.loginResult = {
      name: 'Saved Player', message: '已登录 Saved Player，但配置覆盖失败：模拟写入失败', ok: false
    });
    await savedLogin.click();
    await assertLoginFeedback('已登录 Saved Player，但配置覆盖失败：模拟写入失败', false);
    console.log('PASS account-list login displays configuration success/failure and stops progress');

    await page.evaluate(() => window.backupFixture.loginResult = {
      name: 'new_player', message: '已登录 new_player，配置覆盖完成', ok: true
    });
    await page.locator('#fUser').fill(' new_player ');
    await page.locator('#fPass').fill(' visible password ');
    await page.locator('#doLogin').click();
    await assertLoginFeedback('已登录 new_player，配置覆盖完成', true);
    assert.deepEqual(await page.evaluate(() => window.backupFixture.calls.find(c => c.command === 'login_new').args),
      { account: 'new_player', password: ' visible password ' });
    assert.equal(await page.locator('#fPass').inputValue(), '');
    assert.equal(await page.locator('#doLogin').isEnabled(), true);
    await page.evaluate(() => window.backupFixture.loginResult = {
      name: 'new_player', message: '已登录 new_player，但配置覆盖失败：模拟注册表写入失败', ok: false
    });
    await page.locator('#fUser').fill('new_player');
    await page.locator('#fPass').fill('visible password');
    await page.locator('#doLogin').click();
    await assertLoginFeedback('已登录 new_player，但配置覆盖失败：模拟注册表写入失败', false);
    assert.equal(await page.locator('#fPass').inputValue(), '', 'Confirmed login clears credentials even if cover fails');
    console.log('PASS new-account login preserves password input and displays configuration outcomes');

    await page.evaluate(() => window.backupFixture.loginError = '等待验证码超时，后台继续确认登录');
    await savedLogin.click();
    await assertLoginFeedback('等待验证码超时，后台继续确认登录', false);
    for(const ok of [true, false]) {
      const message = ok ? '后台已确认登录，配置覆盖完成' : '后台已确认登录，但配置覆盖失败：模拟文件占用';
      await page.evaluate(({ message, ok }) => {
        progStart(() => '旧登录进度');
        window.backupFixture.emit('steam-login-finished', { name: 'Saved Player', message, ok });
      }, { message, ok });
      await assertLoginFeedback(message, ok);
    }
    assert.equal(await page.evaluate(() => window.backupFixture.calls.some(c => c.command === 'steam_cover')), false,
      'Login configuration is applied once by the backend, never by a second frontend IPC');
    console.log('PASS late login success/failure events, timeout feedback, no duplicate cover IPC');
    for(const action of ['switch', 'new']) {
      const progress = await page.evaluate(action => {
        const f = window.backupFixture;
        f.loginError = null;
        f.loginResult = { name: 'current_player', message: '本次登录配置覆盖完成', ok: true };
        if(action === 'switch') doSwitch(f.accounts[0]);
        else {
          document.querySelector('#fUser').value = 'current_player';
          document.querySelector('#fPass').value = 'visible password';
          document.querySelector('#doLogin').click();
        }
        const message = document.querySelector('#tkTxt').textContent, timer = progTimer;
        f.emit('steam-login-finished', { name: 'old_player', message: '上次后台登录失败', ok: false });
        return { busy: switchBusy || loginBusy, messageUnchanged: document.querySelector('#tkTxt').textContent === message,
          timerUnchanged: timer !== null && progTimer === timer };
      }, action);
      assert.deepEqual(progress, { busy: true, messageUnchanged: true, timerUnchanged: true },
        'A previous late event must not replace active ' + action + ' login progress');
      await assertLoginFeedback('本次登录配置覆盖完成', true);
    }
    console.log('PASS late login events preserve active switch/new login progress and IPC results');
    assert.deepEqual(errors, []);
    console.log('PASS no JavaScript runtime errors (480 x 660 viewport)');
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
