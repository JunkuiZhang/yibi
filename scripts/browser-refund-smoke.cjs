/* Uses only synthetic records; run against a fresh disposable preview database. */
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const config = JSON.parse(fs.readFileSync(process.env.TALLY_BROWSER_CONFIG, 'utf8'));
const screenshots = process.env.TALLY_SCREENSHOTS || path.resolve('.local/refund-screenshots');
fs.mkdirSync(screenshots, { recursive: true });

(async () => {
  const browser = await chromium.launch({ headless: true, channel: process.env.TALLY_BROWSER_CHANNEL || 'msedge' });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  const form = page.locator('#entry-form');
  const call = (url, body) => page.evaluate(async ({ url, body }) => {
    const response = await fetch(url, { method: body ? 'POST' : 'GET', headers: { 'Content-Type': 'application/json', 'X-Tally-Request': '1' }, ...(body ? { body: JSON.stringify(body) } : {}) });
    if (!response.ok) throw new Error(`API ${response.status}: ${await response.text()}`);
    return response.json();
  }, { url, body });
  async function save() {
    await form.locator('button[type=submit]').click();
    await page.locator('#entry-dialog').waitFor({ state: 'hidden' });
  }
  async function month(value) {
    const response = page.waitForResponse(r => r.url().includes('/api/entries?') && r.url().includes(`month=${value}`));
    await page.locator('#month').fill(value);
    await page.locator('#month').dispatchEvent('change');
    await response;
    await page.locator('.transaction-table').waitFor();
  }
  const row = name => page.locator('.transaction-table tbody tr').filter({ hasText: name });
  async function refund(amount, name) {
    await page.locator('#new-entry').click();
    await form.locator('[name=kind]').selectOption('refund');
    await form.locator('[name=amount]').fill(amount);
    await form.locator('[name=date]').fill('2026-08-15');
    await form.locator('[name=date]').dispatchEvent('change');
    await form.locator('[name=merchant]').fill(name);
    await form.locator('[name=refund_search]').fill('退款验收原付款');
    await page.waitForFunction(() => [...document.querySelectorAll('[name=refund_of] option')].some(o => o.textContent.includes('退款验收原付款')));
    const id = await form.locator('[name=refund_of] option').filter({ hasText: '退款验收原付款' }).getAttribute('value');
    await form.locator('[name=refund_of]').selectOption(id);
    await save();
  }
  async function upload(status) {
    await page.locator('[data-tab=import]').first().click();
    await page.locator('#file-input').waitFor({ state: 'attached' });
    const csv = `微信支付账单明细\n交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n2026-07-20 12:00:00,商户消费,退款导入验收,测试商品,支出,88.00,零钱,${status},refund-browser-payment\n`;
    await page.locator('#file-input').setInputFiles({ name: 'synthetic-refund.csv', mimeType: 'text/csv', buffer: Buffer.from(csv) });
    await page.getByRole('heading', { name: '入账之前，再看一眼。' }).waitFor();
  }
  try {
    await page.goto(config.base);
    await page.locator('#login-form [name=username]').fill(config.username);
    await page.locator('#login-form [name=password]').fill(config.password);
    await page.locator('#login-form button').click();
    await page.getByRole('heading', { name: '把日子，记得清楚。' }).waitFor();
    await page.locator('#new-entry').click();
    await form.locator('[name=amount]').fill('100');
    await form.locator('[name=date]').fill('2026-07-10');
    await form.locator('[name=merchant]').fill('退款验收原付款');
    await form.locator('[name=category]').fill('待分类');
    await form.locator('[name=refunded]').fill('25');
    await form.locator('[name=split]').check();
    await form.locator('[name=split_start]').fill('2026-07');
    await form.locator('[name=split_months]').fill('12');
    assert.match(await page.locator('#split-hint').innerText(), /净额总计 ¥75.00/);
    await form.locator('[name=refunded]').fill('-1');
    await form.locator('button[type=submit]').click();
    assert.match(await page.locator('#entry-error').innerText(), /金额/);
    await form.locator('[name=refunded]').fill('101');
    await form.locator('button[type=submit]').click();
    assert.match(await page.locator('#entry-error').innerText(), /不能超过/);
    await form.locator('[name=refunded]').fill('25');
    await save();
    await page.locator('[data-tab=entries]').first().click();
    await month('2026-07');
    assert.match(await row('退款验收原付款').locator('.amount').innerText(), /−¥75.00\s+已退 ¥25.00/);

    await refund('25', '八月第一笔退款');
    assert.equal((await call('/api/summary?month=2026-07&mode=cash')).expense, 7500);
    assert.equal((await call('/api/summary?month=2026-08&mode=cash')).income, 0);
    await row('退款验收原付款').locator('[data-edit]').click();
    assert.equal(await form.locator('[name=amount]').inputValue(), '100.00');
    assert.match(await page.locator('#expense-refund-hint').innerText(), /已关联退款 ¥25.00/);
    await form.locator('[name=refunded]').fill('0');
    assert.match(await page.locator('#expense-refund-hint').innerText(), /计入支出 ¥75.00/);
    await save();
    await refund('75', '八月余款退款');
    assert.equal((await call('/api/summary?month=2026-07&mode=cash')).expense, 0);
    assert.equal((await call('/api/summary?month=2026-07&mode=cash')).unclassified, 0);
    await page.waitForFunction(() => document.querySelector('.transaction-table .amount')?.textContent.includes('已全额退款'));
    assert.equal((await row('退款验收原付款').locator('.amount').innerText()).trim(), '—\n已全额退款');
    assert.equal(await row('退款验收原付款').locator('.tag.warn').count(), 0);
    await page.screenshot({ path: path.join(screenshots, 'fully-refunded.png'), fullPage: true });
    await month('2026-08');
    assert.equal(await page.locator('.transaction-table .amount.positive').count(), 0);
    assert.equal(await page.locator('.transaction-table .amount').filter({ hasText: '退款留档' }).count(), 2);
    await row('八月第一笔退款').locator('[data-edit]').click();
    assert.equal(await form.locator('[name=amount]').inputValue(), '25.00');
    await page.waitForFunction(() => document.querySelector('#refund-target-hint').textContent.includes('找到'));
    await page.setViewportSize({ width: 390, height: 844 });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    assert.equal(await page.locator('#entry-dialog').evaluate(e => e.scrollWidth > e.clientWidth), false);
    await page.screenshot({ path: path.join(screenshots, 'mobile-refund-editor.png'), fullPage: true });
    await page.locator('#entry-dialog [data-action=close-dialog]').first().click();
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.locator('[data-mode=allocated]').click();
    await page.waitForFunction(() => [...document.querySelectorAll('.transaction-table tr')].some(e => e.textContent.includes('退款验收原付款')));
    assert.match(await row('退款验收原付款').locator('.amount').innerText(), /已全额退款/);

    await call('/api/entries', { date: '2026-07-01', kind: 'expense', amount_cents: 14800, refunded_cents: 2800, merchant: '部分退款的会员', description: '', category: '会员订阅', account: '微信', note: '', split_start: '2027-02', split_months: 12 });
    await page.locator('[data-tab=subscriptions]').first().click();
    const member = page.locator('.subscription').filter({ hasText: '部分退款的会员' });
    await member.waitFor();
    assert.match(await member.locator('.sub-cost').innerText(), /¥10.00/);
    assert.match(await member.locator('.sub-period').innerText(), /净额 ¥120.00 · 已退 ¥28.00/);
    await page.locator('[data-sub-filter=refunded]').click();
    await page.waitForFunction(() => document.querySelectorAll('.subscription').length === 1);
    assert.match(await page.locator('.subscription').innerText(), /已全额退款/);

    await upload('支付成功');
    await page.locator('#commit-import').click();
    await page.getByRole('heading', { name: '每一笔，都有来处。' }).waitFor();
    await upload('已全额退款');
    assert.equal(await page.locator('[data-import-index]:checked').count(), 1);
    assert.match(await page.locator('.preview-warning').innerText(), /更新累计退款/);
    assert.match(await page.locator('.preview-table .amount').innerText(), /已全额退款/);
    await page.locator('#commit-import').click();
    await page.getByRole('heading', { name: '每一笔，都有来处。' }).waitFor();
    assert.match(await page.locator('#toast').innerText(), /更新 1 笔退款状态/);
    await month('2026-07');
    await row('退款导入验收').locator('[data-edit]').click();
    assert.match(await page.locator('#entry-import-status').innerText(), /已全额退款/);
    assert.equal(await form.locator('[name=amount]').inputValue(), '88.00');
    assert.equal(await form.locator('[name=refunded]').inputValue(), '88.00');
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ status: 'passed', flows: ['partial-refund-entry', 'negative-and-excess-validation', 'cross-month-link', 'max-confirmed-linked', 'linked-refund-preview', 'full-refund-no-amount', 'refund-not-income', 'mobile-editor', 'allocated-full-refund-retained', 'net-membership-allocation', 'import-refund-update', 'original-status-retained'], screenshots }));
  } finally { await browser.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
