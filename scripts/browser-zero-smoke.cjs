/* Synthetic zero-value entries and bike rides; requires a fresh preview database. */
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const config = JSON.parse(fs.readFileSync(process.env.TALLY_BROWSER_CONFIG, 'utf8'));
const screenshots = process.env.TALLY_SCREENSHOTS || path.resolve('.local/zero-screenshots');
fs.mkdirSync(screenshots, { recursive: true });

(async () => {
  const browser = await chromium.launch({ headless: true, channel: process.env.TALLY_BROWSER_CHANNEL || 'msedge' });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  const form = page.locator('#entry-form');
  const group = page.locator('details[data-bike-month="2026-09"]');
  const topRows = () => page.locator('.transaction-table').first().locator(':scope > tbody > tr');
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
    const loaded = page.waitForResponse(r => r.url().includes('/api/entries?') && r.url().includes(`month=${value}`));
    await page.locator('#month').fill(value);
    await page.locator('#month').dispatchEvent('change');
    await loaded;
    await page.locator('.transaction-table').first().waitFor();
  }
  async function search(text) {
    const loaded = page.waitForResponse(r => r.url().includes('/api/entries?') && r.url().includes('group_bikes=true'));
    await page.locator('#search-form [name=q]').fill(text);
    await page.locator('#search-form button[type=submit]').click();
    await loaded;
    await group.waitFor();
  }
  async function waitCount(count) {
    await page.waitForFunction(count => document.querySelector('details[data-bike-month="2026-09"]>summary strong')?.textContent === `9月共享单车 · ${count}次`, count);
  }
  try {
    await page.goto(config.base);
    await page.locator('#login-form [name=username]').fill(config.username);
    await page.locator('#login-form [name=password]').fill(config.password);
    await page.locator('#login-form button').click();
    await page.getByRole('heading', { name: '把日子，记得清楚。' }).waitFor();
    for (const [kind, merchant] of [['expense', '零元试用'], ['income', '零元收入'], ['transfer', '零元转账']]) {
      await page.locator('#new-entry').click();
      await form.locator('[name=kind]').selectOption(kind);
      await form.locator('[name=date]').fill('2026-09-30');
      await form.locator('[name=merchant]').fill(merchant);
      if (kind === 'expense') {
        await form.locator('[name=amount]').fill('-1');
        await form.locator('button[type=submit]').click();
        assert.match(await page.locator('#entry-error').innerText(), /金额/);
        await form.locator('[name=kind]').selectOption('refund');
        await form.locator('[name=amount]').fill('0');
        await form.locator('button[type=submit]').click();
        assert.match(await page.locator('#entry-error').innerText(), /大于零/);
        await form.locator('[name=kind]').selectOption('expense');
      }
      await form.locator('[name=amount]').fill('0');
      if (kind === 'expense') {
        assert.equal(await form.locator('[name=split]').isDisabled(), true);
        assert.match(await page.locator('#expense-refund-hint').innerText(), /零元记录/);
        assert.doesNotMatch(await page.locator('#expense-refund-hint').innerText(), /全额退款/);
      }
      await save();
    }

    const header = '微信支付账单明细\n交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n';
    const zeroRides = Array.from({ length: 42 }, (_, i) => `2026-09-${String(2 + i % 28).padStart(2, '0')} 12:00:00,商户消费,哈啰出行,单车骑行通勤样本${String(i + 1).padStart(2, '0')},支出,0.00,零钱,支付成功,browser-zero-bike-${i}`);
    const paidRides = Array.from({ length: 12 }, (_, i) => `2026-09-01 12:00:00,商户消费,美团单车,付费骑行${i + 1},支出,1.50,零钱,支付成功,browser-paid-bike-${i}`);
    const bill = header + [...zeroRides, ...paidRides, '2026-08-01 12:00:00,商户消费,哈啰出行,单车骑行八月样本,支出,0.00,零钱,支付成功,browser-august-bike'].join('\n');
    await page.locator('[data-tab=import]').first().click();
    await page.locator('#file-input').setInputFiles({ name: 'synthetic-zero-bikes.csv', mimeType: 'text/csv', buffer: Buffer.from(bill) });
    await page.getByRole('heading', { name: '入账之前，再看一眼。' }).waitFor();
    assert.equal(await page.locator('.preview-table tbody tr').count(), 55);
    assert.equal(await page.locator('[data-import-index]:checked').count(), 55);
    assert.equal(await page.locator('.preview-table .amount').filter({ hasText: '零元记录' }).count(), 43);
    assert.match(await page.locator('#content').innerText(), /入账后按月收起/);
    await page.locator('#commit-import').click();
    await page.getByRole('heading', { name: '每一笔，都有来处。' }).waitFor();
    await month('2026-09');
    await waitCount(42);
    assert.equal(await group.evaluate(e => e.open), false);
    assert.equal(await topRows().count(), 16);
    assert.equal(await page.locator('.pagination span').innerText(), '1 / 1');
    assert.match(await page.locator('.table-footer').innerText(), /57 笔.*16 行/);
    assert.equal(await topRows().filter({ hasText: '美团单车' }).count(), 12);
    for (const name of ['零元试用', '零元收入', '零元转账']) {
      const row = topRows().filter({ hasText: name });
      assert.equal(await row.locator('.amount').innerText(), '¥0.00\n零元记录');
      assert.equal(await row.locator('.amount.positive,.tag.warn,.transaction-icon.income').count(), 0);
    }
    assert.equal((await call('/api/summary?month=2026-09&mode=cash')).income_months, 0);

    await search('通勤样本01');
    await waitCount(1);
    assert.match(await group.locator('summary').innerText(), /仅含筛选匹配/);
    await search('');
    await waitCount(42);
    await group.locator('summary').click();
    assert.equal(await group.locator('.transaction-table tbody tr').count(), 42);
    await group.locator('[data-edit]').first().click();
    assert.equal(await form.locator('[name=amount]').inputValue(), '0.00');
    await form.locator('[name=note]').fill('零元骑行编辑验收');
    await save();
    await waitCount(42);
    assert.equal(await group.evaluate(e => e.open), true);
    await group.locator('[data-edit]').first().click();
    assert.equal(await form.locator('[name=note]').inputValue(), '零元骑行编辑验收');
    page.once('dialog', dialog => dialog.accept());
    await page.locator('#delete-entry').click();
    await page.locator('#entry-dialog').waitFor({ state: 'hidden' });
    await waitCount(41);
    await group.locator('[data-edit]').first().click();
    await form.locator('[name=amount]').fill('1.50');
    await save();
    await waitCount(40);
    assert.equal(await topRows().filter({ hasText: '哈啰出行' }).filter({ has: page.locator('.amount', { hasText: '−¥1.50' }) }).count(), 1);
    assert.equal((await call('/api/summary?month=2026-09&mode=cash')).expense, 1950);
    await page.screenshot({ path: path.join(screenshots, 'bike-group-desktop.png') });
    await page.setViewportSize({ width: 390, height: 844 });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    assert.equal(await group.evaluate(e => e.scrollWidth > e.clientWidth), false);
    assert.equal(await group.locator('.table-scroll').evaluate(e => e.scrollWidth > e.clientWidth), false);
    await group.locator('summary').scrollIntoViewIfNeeded();
    await page.screenshot({ path: path.join(screenshots, 'bike-group-mobile.png') });
    await page.setViewportSize({ width: 1440, height: 1000 });

    await month('2026-08');
    assert.match(await page.locator('.bike-group>summary strong').innerText(), /8月共享单车 · 1次/);
    await month('2026-09');
    await page.locator('[data-mode=allocated]').click();
    await waitCount(40);
    await page.locator('[data-tab=overview]').first().click();
    await page.getByRole('heading', { name: '把日子，记得清楚。' }).waitFor();
    await waitCount(40);

    const entry = { date: '2026-09-01', kind: 'expense', amount_cents: 100, merchant: '分页普通交易', description: '', category: '餐饮', account: '微信', note: '', split_start: null, split_months: null };
    for (let i = 0; i < 34; i++) await call('/api/entries', { ...entry, description: `分页${i}` });
    await page.locator('[data-tab=entries]').first().click();
    await page.waitForFunction(() => document.querySelector('.pagination span')?.textContent === '1 / 2');
    assert.equal(await topRows().count(), 50);
    await page.locator('[data-action=page][data-page="2"]').click();
    await page.waitForFunction(() => document.querySelector('.pagination span')?.textContent === '2 / 2');
    assert.equal(await topRows().count(), 1);
    assert.equal(await page.locator('[data-action=page][data-page="3"]').isDisabled(), true);
    await topRows().locator('[data-edit]').click();
    await form.locator('[name=amount]').fill('0');
    await form.locator('[name=merchant]').fill('美团单车');
    await form.locator('[name=description]').fill('单车骑行归组测试');
    await save();
    await page.waitForFunction(() => document.querySelector('.pagination span')?.textContent === '1 / 1');
    assert.equal(await topRows().count(), 50);
    await waitCount(41);
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ status: 'passed', flows: ['zero-manual-entry', 'zero-income-neutral', 'zero-refund-rejected', 'zero-split-disabled', 'zero-import-preserved', 'monthly-bike-group', 'paid-rides-separate', 'filtered-group-count', 'child-edit-and-delete', 'group-persisted-open', 'mobile-no-overflow', 'monthly-separation', 'allocated-zero-visible', 'overview-group', 'group-before-pagination', 'page-clamped-after-grouping'], screenshots }));
  } finally { await browser.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
