/* Optional UI acceptance: requires Playwright and a disposable running preview. */
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const config = JSON.parse(fs.readFileSync(process.env.TALLY_BROWSER_CONFIG, 'utf8'));
const screenshots = process.env.TALLY_SCREENSHOTS || path.resolve('.local/screenshots');
fs.mkdirSync(screenshots, { recursive: true });

(async () => {
  const browser = await chromium.launch({ headless: true, ...(process.env.TALLY_BROWSER_CHANNEL ? { channel: process.env.TALLY_BROWSER_CHANNEL } : {}) });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: 'zh-CN', timezoneId: 'Asia/Shanghai' });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  try {
    await page.goto(config.base);
    await page.locator('#login-screen').waitFor({ state: 'visible' });
    await page.screenshot({ path: path.join(screenshots, 'login.png'), fullPage: true });
    await page.locator('#login-form [name=username]').fill(config.username);
    await page.locator('#login-form [name=password]').fill(config.password);
    await page.locator('#login-form button').click();
    await page.locator('#app').waitFor({ state: 'visible' });
    await page.getByRole('heading', { name: '把日子，记得清楚。' }).waitFor();
    await page.locator('#month').fill('2026-10');
    await page.locator('#month').dispatchEvent('change');
    await page.locator('#new-entry').click();
    const form = page.locator('#entry-form');
    await form.locator('[name=amount]').fill('148');
    await form.locator('[name=date]').fill('2026-10-04');
    await form.locator('[name=merchant]').fill('年度视频会员');
    await form.locator('[name=category]').fill('会员订阅');
    await form.locator('[name=split]').check();
    await form.locator('[name=split_start]').fill('2027-02');
    await form.locator('[name=split_months]').fill('12');
    assert.match(await page.locator('#split-hint').innerText(), /2028 年 1 月/);
    await form.locator('button[type=submit]').click();
    await page.locator('#entry-dialog').waitFor({ state: 'hidden' });
    await page.getByRole('heading', { name: '把日子，记得清楚。' }).waitFor();

    const call = (url, body) => page.evaluate(async ({ url, body }) => {
      const r = await fetch(url, { method: body ? 'POST' : 'GET', headers: { 'Content-Type': 'application/json', 'X-Tally-Request': '1' }, ...(body ? { body: JSON.stringify(body) } : {}) });
      if (!r.ok) throw new Error(`API ${r.status}`);
      return r.json();
    }, { url, body });
    const base = {date:'2026-10-04',kind:'expense',amount_cents:10000,merchant:'',description:'',category:'餐饮',account:'微信',note:'',split_start:null,split_months:null};
    for (const e of [
      {date:'2026-07-05',kind:'income',merchant:'公司工资',category:'工资',amount_cents:2500000,account:'工资卡'},
      {date:'2026-08-05',kind:'income',merchant:'公司工资',category:'工资',amount_cents:2400000,account:'工资卡'},
      {date:'2026-09-05',kind:'income',merchant:'公司工资',category:'工资',amount_cents:2300000,account:'工资卡'},
      {date:'2026-10-04',kind:'income',merchant:'公司工资',category:'工资',amount_cents:2200000,account:'工资卡'},
      {date:'2026-10-02',merchant:'周末的咖啡',amount_cents:2800},
      {date:'2026-10-03',merchant:'日常食堂',amount_cents:3500},
      {date:'2026-10-01',merchant:'每月房租',category:'居家生活',amount_cents:260000,account:'工资卡'},
      {date:'2026-10-02',merchant:'通勤地铁',category:'交通出行',amount_cents:800},
      {date:'2026-10-03',merchant:'生活补给',category:'日用购物',amount_cents:18750},
      {date:'2026-02-01',merchant:'上一年的会员',category:'会员订阅',amount_cents:12000,split_start:'2026-02',split_months:12},
      {date:'2026-07-02',merchant:'七月生活',category:'日用购物',amount_cents:586000},
      {date:'2026-08-02',merchant:'八月生活',category:'日用购物',amount_cents:723000},
      {date:'2026-09-02',merchant:'九月生活',category:'日用购物',amount_cents:642000},
    ]) await call('/api/entries', { ...base, ...e });
    const feb = await call('/api/summary?month=2027-02&mode=allocated');
    assert.equal(feb.expense, 1234);

    await page.locator('[data-tab=entries]').first().click();
    await page.getByRole('button', {name:'记工资',exact:false}).click();
    assert.equal(await form.locator('[name=kind]').inputValue(), 'income');
    assert.equal(await form.locator('[name=category]').inputValue(), '工资');
    await form.locator('[name=amount]').fill('50000');
    await form.locator('[name=category]').fill('年终奖');
    await form.locator('[name=merchant]').fill('年度奖金');
    await form.locator('[name=date]').fill('2026-10-04');
    await form.locator('button[type=submit]').click();
    await page.locator('#entry-dialog').waitFor({state:'hidden'});

    await page.locator('[data-tab=import]').first().click();
    const bill = '微信支付账单明细\n交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n2026-10-02 12:00:00,商户消费,测试咖啡店,拿铁,支出,18.50,零钱,支付成功,browser-wx-001\n';
    await page.locator('#file-input').setInputFiles({name:'wechat.csv',mimeType:'text/csv',buffer:Buffer.from(bill)});
    await page.getByRole('heading',{name:'入账之前，再看一眼。'}).waitFor();
    assert.equal(await page.locator('[data-import-index]:checked').count(),1);
    await page.locator('#commit-import').click();
    await page.getByRole('heading',{name:'每一笔，都有来处。'}).waitFor();
    await page.locator('[data-tab=import]').first().click();
    await page.locator('#file-input').setInputFiles({name:'wechat.csv',mimeType:'text/csv',buffer:Buffer.from(bill)});
    await page.getByRole('heading',{name:'入账之前，再看一眼。'}).waitFor();
    assert.equal(await page.locator('#commit-import').isDisabled(),true);
    await page.screenshot({path:path.join(screenshots,'import.png'),fullPage:true});

    await page.locator('[data-tab=subscriptions]').first().click();
    await page.getByRole('heading',{name:'把长期的喜欢，慢慢记。'}).waitFor();
    assert.equal(await page.locator('.subscription').count(),2);
    await page.screenshot({path:path.join(screenshots,'memberships.png'),fullPage:true});
    await page.locator('[data-tab=overview]').first().click();
    await page.getByRole('heading',{name:'把日子，记得清楚。'}).waitFor();
    await page.screenshot({path:path.join(screenshots,'overview.png'),fullPage:true});
    await page.locator('[data-mode=allocated]').click();
    await page.getByRole('heading',{name:'把日子，记得清楚。'}).waitFor();
    await page.locator('#month').fill('2027-02');
    await page.locator('#month').dispatchEvent('change');
    await page.waitForFunction(()=>document.querySelector('.stat-card.featured .stat-value')?.textContent.includes('12.34'));

    const malicious = await call('/api/entries',{...base,merchant:'<img src=x onerror="window.__injected=1">'});
    await page.locator('#month').fill('2026-10');
    await page.locator('#month').dispatchEvent('change');
    await page.getByRole('heading',{name:'把日子，记得清楚。'}).waitFor();
    assert.equal(await page.evaluate(()=>window.__injected),undefined);
    assert.equal(await page.locator('#content img').count(),0);
    await page.evaluate(async id=>fetch(`/api/entries/${id}`,{method:'DELETE',headers:{'X-Tally-Request':'1'}}),malicious.id);

    await page.setViewportSize({width:390,height:844});
    await page.locator('[data-mode=cash]').click();
    await page.getByRole('heading',{name:'把日子，记得清楚。'}).waitFor();
    const overflow = await page.evaluate(()=>document.documentElement.scrollWidth > innerWidth);
    assert.equal(overflow,false,'Mobile page must not overflow horizontally');
    await page.screenshot({path:path.join(screenshots,'mobile.png'),fullPage:true});
    await page.locator('#new-entry').click();
    await form.locator('[name=amount]').fill('-1');
    await form.locator('button[type=submit]').click();
    assert.match(await page.locator('#entry-error').innerText(),/金额/);
    await page.screenshot({path:path.join(screenshots,'mobile-entry.png'),fullPage:true});
    await page.locator('#entry-dialog [data-action=close-dialog]').first().click();
    await page.locator('[data-tab=settings]').first().click();
    await page.getByRole('heading',{name:'一本账本，按你的方式。'}).waitFor();
    await page.locator('[data-action=logout]').click();
    await page.locator('#login-screen').waitFor({state:'visible'});
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({status:'passed',flows:['login','manual-entry','deferred-allocation','income','bonus','CSV-preview','deduplication','subscriptions','XSS-escaping','mobile-layout','validation','logout'],screenshots}));
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exitCode=1;});
