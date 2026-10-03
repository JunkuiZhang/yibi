/* Run only against a fresh disposable database with synthetic records. */
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const config = JSON.parse(fs.readFileSync(process.env.TALLY_BROWSER_CONFIG, 'utf8'));
const screenshots = process.env.TALLY_SCREENSHOTS || path.resolve('.local/category-screenshots');
fs.mkdirSync(screenshots, { recursive: true });

(async () => {
  const browser = await chromium.launch({ headless: true, channel: process.env.TALLY_BROWSER_CHANNEL || 'msedge' });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  const form = page.locator('#entry-form');
  const category = form.locator('[name=category]');
  const options = page.locator('#category-options');
  const call = (url, body) => page.evaluate(async ({url, body}) => {
    const response = await fetch(url, { method: body ? 'POST' : 'GET', headers: {'Content-Type': 'application/json', 'X-Tally-Request': '1'}, ...(body ? {body: JSON.stringify(body)} : {}) });
    if (!response.ok) throw new Error(`API ${response.status}`);
    return response.json();
  }, {url, body});
  async function save() {
    await form.locator('button[type=submit]').click();
    await page.locator('#entry-dialog').waitFor({state: 'hidden'});
  }
  async function edit(name) {
    const button = page.locator('.transaction-table tr').filter({hasText: name}).locator('[data-edit]');
    await button.waitFor();
    await button.click();
  }
  try {
    await page.goto(config.base);
    await page.locator('#login-form [name=username]').fill(config.username);
    await page.locator('#login-form [name=password]').fill(config.password);
    await page.locator('#login-form button').click();
    await page.getByRole('heading', {name: '把日子，记得清楚。'}).waitFor();
    const date = await page.locator('#month').inputValue() + '-10';
    await call('/api/entries', {date, kind:'expense', amount_cents:10000, merchant:'分类验收房租', description:'合成租房记录', category:'待分类', account:'微信', note:'', split_start:null, split_months:null});
    await page.locator('[data-tab=entries]').first().click();
    await edit('分类验收房租');
    assert.equal(await category.inputValue(), '待分类');
    await page.locator('#category-toggle').click();
    await options.waitFor({state:'visible'});
    assert.equal(await options.getByRole('option').count(), 13);
    await options.getByRole('option', {name:'居家生活', exact:true}).click();
    assert.equal(await category.inputValue(), '居家生活');
    assert.equal(await options.isVisible(), false);
    await save();
    assert.equal((await call('/api/export')).entries[0].category, '居家生活');

    await edit('分类验收房租');
    await category.click();
    assert.equal(await options.getByRole('option').count(), 13);
    await category.fill('测试租房分类');
    assert.equal(await options.getByRole('option').count(), 0);
    await save();
    assert.equal((await call('/api/export')).entries[0].category, '测试租房分类');
    await edit('分类验收房租');
    await page.locator('#category-toggle').click();
    assert.equal(await options.getByRole('option', {name:'测试租房分类', exact:true}).count(), 1);
    await category.press('Escape');
    assert.equal(await options.isVisible(), false);
    assert.equal(await page.locator('#entry-dialog').isVisible(), true);
    await category.press('ArrowDown');
    await category.press('Enter');
    assert.equal(await category.inputValue(), '餐饮');
    await page.locator('#category-toggle').click();
    await page.screenshot({path:path.join(screenshots,'category-desktop.png')});
    await category.fill('交通');
    assert.equal(await options.getByRole('option').count(), 1);
    await category.press('ArrowDown');
    await category.press('Enter');
    assert.equal(await category.inputValue(), '交通出行');
    await save();

    await edit('分类验收房租');
    await form.locator('[name=kind]').selectOption('income');
    await category.click();
    assert.equal(await options.getByRole('option', {name:'工资',exact:true}).count(),1);
    assert.equal(await options.getByRole('option', {name:'餐饮',exact:true}).count(),0);
    await options.getByRole('option', {name:'工资',exact:true}).click();
    await form.locator('[name=kind]').selectOption('expense');
    await page.setViewportSize({width:390,height:844});
    await page.locator('#category-toggle').click();
    await options.scrollIntoViewIfNeeded();
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
    assert.equal(await page.locator('#entry-dialog').evaluate(e=>e.scrollWidth>e.clientWidth),false);
    await page.screenshot({path:path.join(screenshots,'category-mobile.png')});
    await options.getByRole('option', {name:'居家生活',exact:true}).click();
    await save();
    assert.equal((await call('/api/export')).entries[0].category, '居家生活');
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({status:'passed',flows:['full-list-with-current-value','click-and-persist','custom-category','custom-suggestion','keyboard-selection','escape-keeps-dialog','typing-filter','kind-options','mobile-no-overflow'],screenshots}));
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exitCode=1;});
