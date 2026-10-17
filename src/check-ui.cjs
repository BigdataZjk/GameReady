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
      const publish = () => {
        snapshot.state_version++;
        const event=clone(snapshot); event.config.password='';
        emit('backup-changed',event);
      };
      window.backupFixture = { snapshot, calls: [], accounts: [], loginResult: null, loginError: null,
        generation: 0, operation: null, status: { paths_valid: {}, steam_running: false, lol_guard_on: false },
        holdLogin: false, earlyFinal: null, holdStatus: false, statusResolvers: [],
        failSync: false, failSave: false, emit, publish };
      window.__TAURI__ = {
        event: { listen: async (name, fn) => { (listeners[name] ||= []).push(fn); return () => {}; } },
        dialog: { open: async () => null },
        window: { getCurrentWindow: () => ({ onCloseRequested: async () => {}, minimize() {}, hide() {}, close() {} }) },
        core: { invoke: async (command, args = {}) => {
          window.backupFixture.calls.push({ command, args: clone(args) });
          if(command === 'backup_get') return clone(snapshot);
          if(command === 'get_status') {
            const f = window.backupFixture;
            if(f.holdStatus) return new Promise(resolve => f.statusResolvers.push(resolve));
            return clone({ ...f.status, steam_operation: f.operation });
          }
          if(command === 'get_settings') return { accent: '#00e5ff', close_behavior: 'tray' };
          if(command === 'accounts_list') return clone(window.backupFixture.accounts);
          if(command === 'list_logs') return [];
          if(command === 'switch_account' || command === 'login_new') {
            const f = window.backupFixture, generation = ++f.generation;
            const operation = (phase,message) => {
              f.operation = { generation, phase, message, sid64: args.sid64 || null };
              emit('steam-operation',f.operation);
            };
            operation('starting','正在启动 Steam');
            if(f.holdLogin) await new Promise(resolve => f.releaseLogin = resolve);
            else await new Promise(resolve => setTimeout(resolve, 40));
            if(f.loginError){ operation('idle',''); throw { message: f.loginError }; }
            const result = { pending:false, ...clone(f.loginResult), generation };
            operation(result.pending ? 'waiting' : 'idle',result.pending ? result.message : '');
            if(f.earlyFinal) emit('steam-login-finished',{ name:'Saved Player', pending:false, generation, ...f.earlyFinal });
            return result;
          }
          if(command === 'steam_cover') {
            const f = window.backupFixture, generation = ++f.generation;
            f.operation = {generation,phase:'applying',message:'正在写入配置',sid64:f.status.current_steam_id64};
            emit('steam-operation',f.operation);
            await new Promise(resolve => f.releaseCover = resolve);
            f.operation = {...f.operation,phase:'idle',message:''}; emit('steam-operation',f.operation);
            return {ok:3,total:3,failed:[],auto_restarted:true};
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
          if(command === 'backup_save') {
            snapshot.config=clone(args.config); snapshot.message='fixture saved'; publish();
            const saved=clone(snapshot);
            // Background work publishes a newer snapshot before the save IPC returns.
            snapshot.config.fingerprint='fixture-pinned-key'; snapshot.status='syncing';
            snapshot.message='fixture background work'; publish();
            return saved;
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

    await page.evaluate(() => {
      const f=window.backupFixture;
      backupSnapshot=null; backupResult=null;
      f.snapshot.status='idle'; f.snapshot.last_success_ms=Date.now(); f.snapshot.message='fixture previous upload';
      toast('fixture baseline'); f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'fixture baseline','Initial snapshots must not announce historical success');
    await page.evaluate(() => {
      const f=window.backupFixture;
      setBackupFeedback('旧手动成功结果','success',true);
      f.snapshot.status='syncing'; f.publish();
      f.snapshot.status='idle'; f.snapshot.last_success_ms+=1; f.snapshot.message='fixture completed'; f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'NAS 自动备份完成：fixture completed');
    assert.equal(await page.locator('#backupFeedback').textContent(),'NAS 自动备份完成：fixture completed');
    await assertFeedbackVisible('success');
    await page.evaluate(() => {
      const f=window.backupFixture; toast('fixture keep newer activity');
      f.emit('backup-changed',f.snapshot);
      const stale=structuredClone(f.snapshot);stale.state_version--;stale.last_success_ms+=100;stale.message='stale success';
      f.emit('backup-changed',stale);
      f.snapshot.message='fixture inventory refresh';f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'fixture keep newer activity','Duplicate, stale, and unrelated snapshots must stay silent');
    assert.equal(await page.locator('#backupFeedback').textContent(),'NAS 自动备份完成：fixture completed');
    await page.evaluate(() => {
      const f=window.backupFixture;
      f.snapshot.status='syncing';f.publish();
      f.snapshot.status='network_error';f.snapshot.message='fixture connection failed';
      f.snapshot.next_retry_ms=Date.now()+60000;f.publish();
    });
    assert.match(await page.locator('#tkTxt').textContent(),/^NAS 自动同步失败：fixture connection failed/);
    assert.match(await page.locator('#backupFeedback').textContent(),/^NAS 自动同步失败：fixture connection failed/);
    assert.equal(await page.locator('#backupIndicator').getAttribute('data-state'),'warning');
    await assertFeedbackVisible('error');
    await page.evaluate(() => {
      const f=window.backupFixture;toast('fixture duplicate error must stay silent');
      f.emit('backup-changed',f.snapshot);f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'fixture duplicate error must stay silent');
    await page.evaluate(() => {
      const f=window.backupFixture;
      f.snapshot.status='auth_error'; f.snapshot.next_retry_ms=0; f.snapshot.message='fixture authentication failed'; f.publish();
      toast('fixture password-only operation');
      f.snapshot.message='fixture password saved'; f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'fixture password-only operation','Changing a password during an existing error is not a new sync failure');
    await page.evaluate(() => {
      const f=window.backupFixture;progStart(()=>'fixture login still running');
      f.snapshot.status='idle';f.snapshot.last_success_ms+=1;f.snapshot.next_retry_ms=0;
      f.snapshot.message='fixture recovered';f.publish();
    });
    assert.equal(await page.locator('#tkTxt').textContent(),'NAS 自动备份完成：fixture recovered');
    assert.equal(await page.evaluate(() => progTimer !== null),true,'NAS notifications cannot stop an active Steam operation');
    await page.evaluate(() => progStop());
    console.log('PASS automatic NAS outcomes are visible, replace stale pinned results, and ignore duplicate/startup snapshots');

    await page.locator('#backupConfigTab').click();
    await page.locator('#backupHost').fill('updated.example.test');
    assert.equal(await page.evaluate(() => backupDirty),true);
    await page.locator('#backupSave').click();
    await page.waitForFunction(() => !backupBusy);
    assert.equal(await page.evaluate(() => backupDirty),false,'A successful stale save response must still clear saved draft state');
    assert.equal(await page.locator('#backupHost').inputValue(),'updated.example.test');
    assert.equal(await page.locator('#backupPassword').inputValue(),'fixture-nas-password','Sanitized events must never clear the saved NAS password form');
    assert.equal(await page.evaluate(() => backupFingerprint),'fixture-pinned-key','The newer saved configuration wins over the stale save response');
    assert.equal(await page.evaluate(() => backupSnapshot.status),'syncing');
    await page.evaluate(() => {
      const stale=structuredClone(window.backupFixture.snapshot); stale.state_version--;
      stale.config.host='stale.example.test';stale.config.password='stale-fixture-password';
      applyBackup(stale,true);
    });
    assert.equal(await page.locator('#backupHost').inputValue(),'updated.example.test','A delayed full query cannot overwrite the current form');
    assert.equal(await page.locator('#backupPassword').inputValue(),'fixture-nas-password');
    await page.evaluate(() => {const f=window.backupFixture;f.snapshot.status='idle';f.publish();});
    await page.locator('#backupQueueTab').click();
    console.log('PASS configuration save race keeps the latest snapshot and clears dirty state');

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
      assert.equal(await page.locator('#ticker').getAttribute('class'), 'ticker' + (ok == null ? '' : ok ? ' ok' : ' err'));
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
    assert.equal(await page.locator('#fPass').inputValue(), 'visible password', 'An unsuccessful result retains credentials for retry');
    console.log('PASS new-account login preserves password input and displays configuration outcomes');

    await page.evaluate(() => window.backupFixture.loginError = 'Steam 启动失败');
    await savedLogin.click();
    await assertLoginFeedback('Steam 启动失败', false);
    await page.evaluate(() => {
      const f=window.backupFixture; f.loginError=null;
      f.loginResult={name:'Saved Player',message:'请在 Steam 完成验证，随后自动应用配置',ok:true,pending:true};
    });
    await savedLogin.click();
    await assertLoginFeedback('请在 Steam 完成验证，随后自动应用配置', null);
    assert.equal(await page.evaluate(() => toastTimer),null,'Waiting feedback has no fade-out timer');
    await page.evaluate(async () => {
      const f=window.backupFixture;
      f.status={...f.status,steam_running:true,current_steam_id64:f.accounts[0].steam_id64};
      await refreshStatus();
    });
    assert.equal(await page.locator('#acctCards .acct.cur').count(),0,'Waiting target is not a confirmed current account');
    assert.equal(await savedLogin.isEnabled(),true,'A pending verification can be replaced by a new login');
    for(const ok of [true, false]) {
      const message = ok ? '后台已确认登录，配置覆盖完成' : '后台已确认登录，但配置覆盖失败：模拟文件占用';
      await page.evaluate(({ message, ok }) => {
        const f=window.backupFixture;
        f.status={...f.status,steam_running:false,current_steam_id64:null};
        f.operation={generation:++f.generation,phase:'applying',message:'正在应用配置',sid64:f.accounts[0].steam_id64};
        f.emit('steam-operation',f.operation);
        f.emit('steam-login-finished', { name: 'Saved Player', message, ok, generation:f.generation,pending:false });
      }, { message, ok });
      await assertLoginFeedback(message, ok);
    }
    assert.equal(await page.evaluate(() => window.backupFixture.calls.some(c => c.command === 'steam_cover')), false,
      'Login configuration is applied once by the backend, never by a second frontend IPC');
    console.log('PASS pending verification feedback, truthful current-account badge, late outcomes, no duplicate cover IPC');
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
        f.emit('steam-login-finished', { name: 'old_player', message: '上次后台登录失败', ok: false,generation:f.generation-1,pending:false });
        f.emit('steam-operation',{generation:f.generation-1,phase:'idle',message:'旧操作',sid64:null});
        return { busy: switchBusy || loginBusy, messageUnchanged: document.querySelector('#tkTxt').textContent === message,
          timerUnchanged: timer !== null && progTimer === timer };
      }, action);
      assert.deepEqual(progress, { busy: true, messageUnchanged: true, timerUnchanged: true },
        'A previous late event must not replace active ' + action + ' login progress');
      await assertLoginFeedback('本次登录配置覆盖完成', true);
    }
    console.log('PASS late login events preserve active switch/new login progress and IPC results');
    await page.evaluate(() => {
      const f=window.backupFixture;
      f.loginResult={name:'Saved Player',message:'迟到的等待结果',ok:true,pending:true};
      f.earlyFinal={message:'最终确认已先到达',ok:true};
    });
    await savedLogin.click();
    await assertLoginFeedback('最终确认已先到达',true);
    await page.evaluate(() => {
      const f=window.backupFixture; f.earlyFinal=null;
      f.emit('steam-operation',{generation:f.generation,phase:'waiting',message:'迟到的等待阶段',sid64:f.accounts[0].steam_id64});
    });
    await assertLoginFeedback('最终确认已先到达',true);
    console.log('PASS final-before-pending result and late same-generation phase rejection');

    await page.evaluate(() => {
      const f=window.backupFixture; f.holdLogin=true;
      f.loginResult={name:'submitted_player',message:'提交的账号已完成',ok:true};
    });
    await page.locator('#fUser').fill('submitted_player');
    await page.locator('#fPass').fill('submitted password');
    await page.locator('#doLogin').click();
    await page.waitForFunction(() => typeof window.backupFixture.releaseLogin === 'function');
    await page.locator('#fUser').fill('next_player');
    await page.locator('#fPass').fill('next password');
    await page.evaluate(() => {const f=window.backupFixture;f.holdLogin=false;f.releaseLogin();});
    await assertLoginFeedback('提交的账号已完成',true);
    assert.equal(await page.locator('#fUser').inputValue(),'next_player');
    assert.equal(await page.locator('#fPass').inputValue(),'next password');
    console.log('PASS edits to next-login draft survive the previous result');

    for(const phase of ['stopping','applying','starting','restoring']) {
      const count = await page.evaluate(phase => {
        const f=window.backupFixture;
        f.operation={generation:++f.generation,phase,message:'阶段 '+phase,sid64:f.accounts[0].steam_id64};
        f.emit('steam-operation',f.operation);
        return f.calls.length;
      },phase);
      assert.equal(await page.locator('#doLogin').isDisabled(),true);
      assert.equal(await page.locator('#coverBtn').isDisabled(),true);
      assert.equal(await page.locator('#acctCards .acct button:enabled').count(),0);
      await page.evaluate(() => {doSwitch(window.backupFixture.accounts[0]);document.querySelector('#doLogin').click();document.querySelector('#coverBtn').click();});
      assert.equal(await page.evaluate(() => window.backupFixture.calls.length),count);
    }
    await page.evaluate(() => {
      const f=window.backupFixture;
      f.operation={...f.operation,phase:'idle',message:''};f.emit('steam-operation',f.operation);
    });
    assert.equal(await savedLogin.isEnabled(),true);
    assert.equal(await page.locator('#doLogin').isEnabled(),true);
    console.log('PASS every busy backend phase locks controls and idle unlocks them');

    const current = await page.evaluate(async () => {
      const f=window.backupFixture; f.holdStatus=true;
      const first=refreshStatus(), second=refreshStatus();
      f.statusResolvers[1]({...f.status,current_steam_id64:'newer'});await second;
      f.statusResolvers[0]({...f.status,current_steam_id64:'older'});await first;
      f.holdStatus=false;
      return state.currentSid64;
    });
    assert.equal(current,'newer');
    const phaseAfterStaleStatus = await page.evaluate(async () => {
      const f=window.backupFixture; f.holdStatus=true; f.statusResolvers=[];
      const oldOperation=structuredClone(f.operation), request=refreshStatus();
      f.operation={generation:++f.generation,phase:'applying',message:'最新覆盖阶段',sid64:f.accounts[0].steam_id64};
      f.emit('steam-operation',f.operation);
      f.statusResolvers[0]({...f.status,steam_operation:oldOperation});await request;
      f.holdStatus=false;
      const phase=steamOperation.phase;
      f.operation={...f.operation,phase:'idle',message:''};f.emit('steam-operation',f.operation);
      return phase;
    });
    assert.equal(phaseAfterStaleStatus,'applying');
    console.log('PASS stale status responses cannot roll back current-account state');

    await page.evaluate(() => {
      const f=window.backupFixture;
      f.operation={generation:++f.generation,phase:'waiting',message:'旧路径等待登录',sid64:f.accounts[0].steam_id64};
      f.emit('steam-operation',f.operation);
      f.operation={generation:++f.generation,phase:'idle',message:'Steam 路径已更新，上次登录确认已取消',sid64:null};
      applyStatus({...f.status,steam_operation:f.operation});
    });
    await assertLoginFeedback('Steam 路径已更新，上次登录确认已取消',null);
    assert.equal(await page.locator('#doLogin').isEnabled(),true);
    await page.evaluate(() => {
      const f=window.backupFixture;
      f.emit('steam-login-finished',{generation:f.generation-1,pending:false,ok:true,name:'old_player',message:'旧路径错误成功'});
      f.operation={...f.operation,message:''};f.emit('steam-operation',f.operation);
    });
    await assertLoginFeedback('Steam 路径已更新，上次登录确认已取消',null);
    console.log('PASS path-change cancellation replaces waiting feedback and empty idle stays silent');

    await page.evaluate(() => {
      const f=window.backupFixture;
      f.accounts.push({ steam_id64:'76561198000000002',account_name:'fresh_player',persona_name:'Fresh Player',timestamp:1728000001 });
      f.status={...f.status,steam_running:true,current_steam_id64:'76561198000000002'};
      applyStatus({...f.status,steam_operation:f.operation});
    });
    await page.waitForFunction(() => [...document.querySelectorAll('#acctCards .acct')].some(card=>card.textContent.includes('fresh_player')));
    assert.match(await page.locator('#acctCards .acct.cur').textContent(),/fresh_player/);
    console.log('PASS active SteamID change refreshes new account without waiting for a history event');
    await page.evaluate(async () => {
      const f=window.backupFixture;f.accounts=f.accounts.filter(a=>a.account_name!=='fresh_player');
      f.status={...f.status,steam_running:true,current_steam_id64:f.accounts[0].steam_id64};
      await refreshStatus();
    });
    await page.locator('#coverBtn').click();
    await page.waitForFunction(() => typeof window.backupFixture.releaseCover === 'function');
    await page.waitForTimeout(1300);
    assert.equal(await page.locator('#coverBadge').textContent(),'进行中');
    assert.notEqual(await page.locator('#coverBtn').evaluate(el=>el.style.getPropertyValue('--p')),'100');
    await page.evaluate(() => window.backupFixture.releaseCover());
    await page.waitForFunction(() => document.querySelector('#coverBadge').textContent === '完成');
    assert.equal(await page.locator('#coverBtn').evaluate(el=>el.style.getPropertyValue('--p')),'100');
    console.log('PASS cover shows real completion rather than reaching 100% on a timer');
    assert.deepEqual(errors, []);
    console.log('PASS no JavaScript runtime errors (480 x 660 viewport)');
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
