/* Synthetic data only; run against a fresh disposable server. */
const fs=require('node:fs');
const assert=require('node:assert/strict');
const {chromium}=require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const config=JSON.parse(fs.readFileSync(process.env.TALLY_BROWSER_CONFIG,'utf8'));

(async()=>{
  const browser=await chromium.launch({headless:true,channel:process.env.TALLY_BROWSER_CHANNEL || 'msedge'});
  const page=await browser.newPage({viewport:{width:1440,height:1000},locale:'zh-CN',timezoneId:'Asia/Shanghai'});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  const form=page.locator('#entry-form');
  const call=(url,body,method)=>page.evaluate(async({url,body,method})=>{
    const response=await fetch(url,{method:method || (body?'POST':'GET'),headers:{'Content-Type':'application/json','X-Tally-Request':'1'},...(body?{body:JSON.stringify(body)}:{})});
    if(!response.ok)throw new Error(`API ${response.status}: ${await response.text()}`);
    return response.json();
  },{url,body,method});
  const exported=async()=> (await call('/api/export')).entries;
  const category=async(id)=>(await exported()).find(e=>e.id===id).category;
  async function saveRule(pattern,value){
    const rule=page.locator('#rule-form');await rule.locator('[name=pattern]').fill(pattern);
    await rule.locator('[name=category]').fill(value);
    await rule.locator('button').click();
    await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('规则已保存'));
    await page.waitForFunction(()=>document.querySelector('#rule-form [name=pattern]')?.value==='');
  }
  async function deleteRule(pattern){
    page.once('dialog',async dialog=>{
      assert.match(dialog.message(),/剩余自定义规则.*待分类/);await dialog.accept();
    });
    await page.locator('.rule-row').filter({hasText:`${pattern} →`}).locator('[data-delete-rule]').click();
    await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('规则已删除'));
    await page.locator('.rule-row').filter({hasText:`${pattern} →`}).waitFor({state:'hidden'});
  }
  async function preview(id){
    return page.evaluate(async({id})=>{
      const csv=`支付宝交易记录明细\n交易时间,交易分类,交易对方,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号\n2026-09-30 12:00:00,其他,同步验收商户,合成商品,支出,2.00,余额,交易成功,${id}\n`;
      const data=new FormData();data.append('file',new Blob([csv]),'synthetic-rules.csv');
      const response=await fetch('/api/import/preview',{method:'POST',headers:{'X-Tally-Request':'1'},body:data});
      if(!response.ok)throw new Error(`preview ${response.status}`);return response.json();
    },{id});
  }
  try{
    await page.goto(config.base);
    assert.equal(await page.evaluate(async()=> (await fetch('/api/rules/apply',{method:'POST',headers:{'X-Tally-Request':'1'}})).status),401);
    await page.locator('#login-form [name=username]').fill(config.username);
    await page.locator('#login-form [name=password]').fill(config.password);
    await page.locator('#login-form button').click();
    await page.getByRole('heading',{name:'把日子，记得清楚。'}).waitFor();
    const month=await page.locator('#month').inputValue();
    const base={date:month+'-02',kind:'expense',amount_cents:10000,merchant:'同步验收商户',description:'合成商品',category:'旧手动分类',account:'微信',note:'保留备注',split_start:null,split_months:null,refunded_cents:0,refund_of:null};
    const add=async fields=>(await call('/api/entries',{...base,...fields})).id;
    const current=await add({});
    const old=await add({date:'2025-02-01',amount_cents:14800,split_start:'2027-02',split_months:12});
    const july=await add({date:'2026-07-01',refunded_cents:2500});
    const zero=await add({date:'2026-09-01',amount_cents:0});
    const refund=await add({date:'2026-08-01',kind:'refund',amount_cents:2500,refund_of:july});
    const income=await add({kind:'income'});
    const unrelated=await add({merchant:'另一商户',description:'同步验收商户'});
    const before=await exported();
    const summary=await call('/api/summary?month=2026-07&mode=cash');
    await page.locator('[data-tab=entries]').first().click();
    await page.locator(`[data-edit="${current}"]`).click();
    await form.locator('[name=category]').fill('居家生活');
    await form.locator('[name=remember_rule]').check();
    await form.locator('button[type=submit]').click();
    await page.locator('#entry-dialog').waitFor({state:'hidden'});
    assert.match(await page.locator('#toast').innerText(),/匹配 4 笔，更新 3 笔分类/);
    for(const id of [current,old,july,zero])assert.equal(await category(id),'居家生活');
    for(const id of [refund,income,unrelated])assert.equal(await category(id),'旧手动分类');
    const after=await exported();
    for(const entry of before){
      const actual={...after.find(e=>e.id===entry.id)},expected={...entry};
      delete actual.category;delete expected.category;assert.deepEqual(actual,expected);
    }
    const afterSummary=await call('/api/summary?month=2026-07&mode=cash');
    for(const key of ['income','expense','balance','refund','pending'])assert.equal(afterSummary[key],summary[key]);

    await page.locator('[data-tab=settings]').first().click();
    await page.locator('#rule-form').waitFor();
    await saveRule('同步','宽泛分类');
    assert.equal(await category(old),'居家生活');
    await saveRule('同步验收商户','新的住房分类');
    for(const id of [current,old,july,zero])assert.equal(await category(id),'新的住房分类');
    await deleteRule('同步验收商户');
    for(const id of [current,old,july,zero])assert.equal(await category(id),'宽泛分类');
    await deleteRule('同步');
    for(const id of [current,old,july,zero])assert.equal(await category(id),'待分类');
    assert.equal(await page.locator('[data-action=apply-rules]').isDisabled(),true);

    await saveRule('同步','重算分类');
    const manual=(await exported()).find(e=>e.id===july);
    await call(`/api/entries/${july}`,{...manual,category:'临时手动分类'},'PUT');
    await page.locator('[data-action=apply-rules]').click();
    await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('全部规则已应用'));
    assert.match(await page.locator('#toast').innerText(),/更新 1 笔分类/);
    assert.equal(await category(july),'重算分类');

    const pending=await preview('rules-pending-add');
    assert.equal(pending.preview.rows[0].entry.category,'重算分类');
    await call('/api/rules',{pattern:'同步',category:'最新分类',kind:'expense'});
    await call('/api/import/commit',{token:pending.token,indices:[0]});
    let imported=(await exported()).find(e=>e.external_id==='rules-pending-add:expense');
    assert.equal(imported.category,'最新分类');
    const pendingDelete=await preview('rules-pending-delete');
    const rule=(await call('/api/state')).rules.find(r=>r.pattern==='同步');
    await call(`/api/rules/${rule.id}`,null,'DELETE');
    await call('/api/import/commit',{token:pendingDelete.token,indices:[0]});
    imported=(await exported()).find(e=>e.external_id==='rules-pending-delete:expense');
    assert.equal(imported.category,'待分类');
    assert.equal((await exported()).length,before.length+2);
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({status:'passed',flows:['remember-updates-all-months','kind-and-merchant-scope','fields-and-refunds-preserved','settings-rule-save','longer-rule-priority','delete-falls-back','delete-unmatched-pending','apply-existing-rules','import-uses-latest-rules','deleted-rule-not-used-by-pending-import','authenticated-batch-route']}));
  }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
