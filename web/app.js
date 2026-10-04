"use strict";

const $ = (s, root = document) => root.querySelector(s);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const icon = name => `<svg aria-hidden="true"><use href="#i-${name}"/></svg>`;
const today = new Intl.DateTimeFormat("sv-SE", {timeZone:"Asia/Shanghai",year:"numeric",month:"2-digit",day:"2-digit"}).format(new Date());
const money = cents => (cents / 100).toLocaleString("zh-CN", {minimumFractionDigits:2,maximumFractionDigits:2});
const yuan = cents => `¥${money(cents)}`;
const monthLabel = month => `${month.slice(0,4)} 年 ${Number(month.slice(5))} 月`;
const monthIndex = month => Number(month.slice(0,4))*12 + Number(month.slice(5))-1;
const addMonths = (month, count) => { const i=monthIndex(month)+count; return `${Math.floor(i/12)}-${String(i%12+1).padStart(2,"0")}`; };
const kindName = {expense:"支出",income:"收入",transfer:"转账",refund:"退款"};
const categories = ["餐饮","日用购物","交通出行","居家生活","会员订阅","医疗健康","通讯网络","学习成长","休闲娱乐","兴趣爱好","人情往来","其他支出","待分类"];
const colors = ["#6e8860","#9bb589","#c2cbb1","#e3d5b5","#b6c9c3","#adb4a2"];
const state = {tab:"overview",month:today.slice(0,7),mode:"cash",data:null,pool:new Map(),page:1,search:"",kind:"",preview:null,selected:new Set(),previewPage:1,withBonus:true,subscriptionFilter:"all",expandedBikeGroups:new Set()};
const classificationState = {page:1,search:"",kind:"",merchant:null,pending:true,selected:new Set(),visible:[],merchants:[],category:"",locked:true};
let renderVersion=0,toastTimer,refundSearchTimer,refundSearchVersion=0;
let editingLinkedRefunds=0;
try { state.mode=localStorage.getItem("tally-mode")==="allocated" ? "allocated":"cash"; } catch {}

function toast(message, error=false) {
  const box=$("#toast"); box.textContent=message;box.className=`toast${error?" error":""}`;box.hidden=false;
  clearTimeout(toastTimer);toastTimer=setTimeout(()=>box.hidden=true,5000);
}
function showLogin() {
  renderVersion++;refundSearchVersion++;clearTimeout(refundSearchTimer);$("#app").hidden=true;$("#login-screen").hidden=false;$("#initial-loading").hidden=true;
  $("#content").replaceChildren();$("#entry-dialog").close();state.pool.clear();state.expandedBikeGroups.clear();state.preview=null;state.selected.clear();state.data=null;
  classificationState.selected.clear();classificationState.merchant=null;classificationState.search="";classificationState.category="";
  $("#login-form").elements.password.value="";
}
async function api(path, options={}) {
  const method=options.method || "GET";
  const headers={...(method!=="GET"?{"X-Tally-Request":"1"}:{}),...options.headers};
  let body=options.body;
  if (body && !(body instanceof FormData)) {body=JSON.stringify(body);headers["Content-Type"]="application/json";}
  const response=await fetch(path,{...options,body,headers,credentials:"same-origin",cache:"no-store"});
  const text=await response.text();let data;
  try { data=JSON.parse(text); } catch { data={error:response.status===413?"文件过大，请每次上传小于 8MB 的账单":"请求未能完成，请稍后重试"}; }
  if (!response.ok) {if(response.status===401)showLogin();throw new Error(data.error || "操作失败");}
  return data;
}
async function bootstrap() {
  state.data=await api("/api/state");
  if (state.month<state.data.ledger_start.slice(0,7))state.month=state.data.ledger_start.slice(0,7);
  $("#username").textContent=state.data.username;
  $("#ledger-since").textContent=`从 ${monthLabel(state.data.ledger_start.slice(0,7))} 开始记录`;
  $("#month").value=state.month;$("#month").min=state.data.ledger_start.slice(0,7);$("#month").max="2199-12";
  $("#accounts").innerHTML=state.data.accounts.map(a=>`<option value="${esc(a)}">`).join("");
  $("#login-screen").hidden=true;$("#initial-loading").hidden=true;$("#app").hidden=false;
}
const modeSwitch = () => `<div class="mode-switch" aria-label="统计口径"><button data-mode="cash" class="${state.mode==="cash"?"active":""}" aria-pressed="${state.mode==="cash"}">净收支</button><button data-mode="allocated" class="${state.mode==="allocated"?"active":""}" aria-pressed="${state.mode==="allocated"}">按月分摊</button></div>`;
const heading = (title, subtitle, extra="", cls="") => `<div class="page-heading ${cls}"><div>${cls==="greeting"?'<span class="eyebrow">YOUR EVERYDAY, AT A GLANCE</span>':""}<h1>${title}</h1><p>${subtitle}</p></div>${extra}</div>`;
const empty = (title, text, action="") => `<div class="empty"><span class="empty-icon">${icon("book")}</span><h3>${title}</h3><p>${text}</p>${action?`<div class="empty-actions">${action}</div>`:""}</div>`;
const addButton = () => `<button class="button primary" data-action="add">${icon("plus")}记一笔</button>`;
function cacheEntries(rows) {for(const e of rows){if(e.row_type==="bike_group")cacheEntries(e.entries);else state.pool.set(e.id,e);}return rows;}
const refundedAmount = e => e.kind==="expense" ? (e.refunded_total_cents ?? e.refunded_cents ?? 0) : 0;
const netAmount = e => e.net_amount_cents ?? (e.kind==="refund" ? 0 : Math.max(0,e.amount_cents-refundedAmount(e)));
function allocatedExpense(e, month) {
  const amount=netAmount(e);
  if(e.kind!=="expense")return amount;
  if(!e.split_start)return e.date.startsWith(month)?amount:0;
  const offset=monthIndex(month)-monthIndex(e.split_start), n=e.split_months;
  return offset<0 || offset>=n ? 0 : Math.floor(amount/n)+(offset<amount%n?1:0);
}
function amountDisplay(e, allocated=false) {
  const refunded=refundedAmount(e),amount=allocated?allocatedExpense(e,state.month):netAmount(e);
  if(e.kind==="refund")return '<span>—</span><small>退款留档</small>';
  if(e.amount_cents===0)return '<span>¥0.00</span><small>零元记录</small>';
  if(e.kind==="expense" && netAmount(e)===0 && refunded>0)return '<span>—</span><small>已全额退款</small>';
  const sign=e.kind==="transfer"?"":e.kind==="income"?"+":"−";
  return `<span>${sign}${yuan(amount)}</span>${refunded?`<small>已退 ${yuan(refunded)}</small>`:""}`;
}
function bikeGroupRow(group) {
  const filtered=state.tab==="entries" && (state.search || state.kind);
  return `<tr class="bike-group-row"><td colspan="5"><details class="bike-group" data-bike-month="${esc(group.month)}" ${state.expandedBikeGroups.has(group.month)?"open":""}><summary><span class="transaction-icon bike-icon">${icon("bike")}</span><span class="bike-group-label"><strong>${Number(group.month.slice(5))}月共享单车 · ${group.count}次</strong><small>${filtered?"仅含筛选匹配的零元骑行":"零元骑行按月收起，单笔记录完整保留"}</small></span><span class="amount zero-amount"><span>¥0.00</span><small>零元记录</small></span><span class="bike-group-toggle" aria-hidden="true">${icon("arrow")}</span></summary><div class="bike-group-entries"><p>${monthLabel(group.month)} · ${filtered?"符合筛选的":"本月"} ${group.count} 次零元骑行，按日期从近到远排列。可逐笔编辑或删除。</p>${transactionTable(group.entries)}</div></details></td></tr>`;
}
function transactionTable(rows) {
  if(!rows.length)return empty("这个月，故事还没开始", "导入微信、支付宝账单，或手动记下第一笔收支。", `${addButton()}<button class="button secondary" data-tab="import">导入账单</button>`);
  return `<div class="table-scroll"><table class="transaction-table"><thead><tr><th>交易 / 用途</th><th class="desktop-col">分类</th><th class="desktop-col">账户</th><th class="align-right">${state.mode==="allocated"?"本月金额":"计入金额"}</th><th></th></tr></thead><tbody>${rows.map(e=>{
    if(e.row_type==="bike_group")return bikeGroupRow(e);
    const zero=e.amount_cents===0,positive=e.kind==="income" && !zero,fullyRefunded=e.kind==="expense" && netAmount(e)===0 && refundedAmount(e)>0;
    const symbol={expense:"↓",income:"↑",transfer:"⇄",refund:"↩"}[e.kind];
    const refundNote=e.kind==="refund" ? (e.refund_target?` · 回冲 ${esc(e.refund_target.date.slice(0,7))}`:" · 不单独计收支") : "";
    return `<tr><td><div class="transaction-name"><span class="transaction-icon ${zero?"zero-icon":e.kind}">${symbol}</span><div><strong title="${esc(e.merchant || e.description)}">${esc(e.merchant || e.description || kindName[e.kind])}</strong><small>${esc(e.date)}${e.split_start?` · ${e.split_months} 期分摊`:""}${e.kind==="transfer"?" · 不计收支":""}${refundNote}</small></div></div></td><td class="desktop-col"><span class="tag ${e.category==="待分类" && !zero && !fullyRefunded && e.kind!=="refund"?"warn":""}">${esc(e.category)}</span></td><td class="desktop-col">${esc(e.account)}</td><td class="amount ${positive?"positive":""} ${zero?"zero-amount":e.kind==="refund" || fullyRefunded?"muted-amount":""}">${amountDisplay(e,state.mode==="allocated")}</td><td><button class="link-button" data-edit="${e.id}" aria-label="编辑 ${esc(e.merchant || e.description)}">编辑</button></td></tr>`;
  }).join("")}</tbody></table></div>`;
}
function statCard(title,value,note,symbol,featured=false) {
  return `<section class="stat-card${featured?" featured":""}"><div class="stat-top"><span>${title}</span><span class="stat-icon">${icon(symbol)}</span></div><div class="stat-value"><span class="currency">¥</span>${money(value)}</div><div class="stat-note"><i class="dot"></i>${note}</div></section>`;
}
function chart(months) {
  if(!months.some(m=>m.income || m.expense))return empty("每个月，都有自己的节奏", "记录之后，这里会呈现收入与支出的变化。");
  const max=Math.max(10000,...months.flatMap(m=>[m.income,Math.abs(m.expense)]));
  const short=n=>n>=1e6?`${(n/1e6).toFixed(1)}万`:Math.round(n/100).toLocaleString("zh-CN");
  return `<div class="chart" role="img" aria-label="各月收入支出柱状图"><div class="chart-axis">${[1,.75,.5,.25,0].map(v=>`<span>${short(max*v)}</span>`).join("")}</div>${months.map(m=>`<div class="chart-group" title="${monthLabel(m.month)}：收入 ${yuan(m.income)}，支出 ${yuan(m.expense)}"><div class="bar income" style="height:${m.income/max*100}%"></div><div class="bar" style="height:${Math.max(0,m.expense)/max*100}%"></div><span>${Number(m.month.slice(5))} 月</span></div>`).join("")}</div><p class="chart-hint">只展示正式记账起点至所选月份；退款抵扣原支出，未计为当月收入。</p>`;
}
function categoryChart(items) {
  const positive=items.filter(i=>i.amount>0), total=positive.reduce((s,i)=>s+i.amount,0);
  if(!total)return empty("钱花在哪里，一目了然", "有支出后，这里会展示你的消费分布。");
  const slices=positive.slice(0,5);
  if(positive.length>5)slices.push({name:"其他分类",amount:positive.slice(5).reduce((s,i)=>s+i.amount,0)});
  let sum=0;
  const gradient=slices.map((item,i)=>{const start=sum;sum+=item.amount/total*360;return `${colors[i%colors.length]} ${start}deg ${sum}deg`;}).join(",");
  return `<div class="category-area"><div class="donut" style="background:conic-gradient(${gradient})"><div class="donut-center"><strong>${positive.length}</strong><small>个消费分类</small></div></div><div class="category-list">${slices.map((item,i)=>`<div class="category-row"><i style="background:${colors[i%colors.length]}"></i><span title="${esc(item.name)}">${esc(item.name)}</span><strong>${yuan(item.amount)}</strong></div>`).join("")}</div></div>`;
}
async function overview() {
  const query=new URLSearchParams({month:state.month,mode:state.mode});
  const [s,list]=await Promise.all([api(`/api/summary?${query}`),api(`/api/entries?${query}&group_bikes=true`)]);cacheEntries(list.entries);
  const description=state.mode==="cash"?"按交易月份记净收支，退款抵扣原支出月份。":"将退款后的净支出按月分摊，让每个月更清楚。";
  return heading("把日子，记得清楚。",description,modeSwitch(),"greeting")+
    `<div class="stat-grid">${statCard(state.mode==="cash"?"本月支出":"本月分摊后费用",s.expense,`已抵扣退款 ${yuan(s.refund)}`,"up",true)}${statCard("本月收入",s.income,"工资、奖金与其他实际收入","down")}${statCard(state.mode==="cash"?"本月收支结余":"本月费用口径结余",s.balance,"本月收入 − 本月支出","grid")}${statCard("未来待分摊",s.pending,"已付款，计入后续月份的费用","layers")}</div>`+
    `<div class="dashboard-grid"><section class="card card-pad"><div class="card-header"><div><h2>收支之间</h2><p>${state.month.slice(0,4)} 年 · 截至 ${Number(state.month.slice(5))} 月</p></div><div class="legend"><span><i class="income"></i>收入</span><span><i></i>支出</span></div></div>${chart(s.months)}</section><section class="card card-pad"><div class="card-header"><div><h2>消费的形状</h2><p>本月分类分布${s.unclassified?` · ${s.unclassified} 笔待分类`:""}</p></div><span class="tag">${Number(state.month.slice(5))} 月</span></div>${categoryChart(s.categories)}</section></div>`+
    `<section class="card income-strip"><div><span>本年累计实发工资</span><strong>${yuan(s.salary)}</strong><small>正式记账起点至所选月份</small></div><div><span>本年累计年终奖</span><strong>${yuan(s.bonus)}</strong><small>按实际到账月份单独记录</small></div><div><span>本年累计收入</span><strong>${yuan(s.year_income)}</strong><small>包括其他收入</small></div><div><span>有记录月份的平均收入</span><strong>${s.income_months?yuan(state.withBonus?s.income_average:s.income_average_without_bonus):"—"}</strong><small>${s.income_months} 个有收入记录的月份 · <button class="link-button" data-action="toggle-bonus">${state.withBonus?"含年终奖":"不含年终奖"} ⇄</button></small></div></section>`+
    `<section class="card card-pad"><div class="card-header"><div><h2>生活的最近几笔</h2><p>${list.total} 笔记录 · ${state.mode==="cash"?"退款回冲原支出月份":"包含净额分摊"}</p></div><button class="button ghost" data-tab="entries">查看全部 ${icon("arrow")}</button></div>${transactionTable(list.entries.slice(0,5))}</section>`;
}
async function entriesView() {
  const query=new URLSearchParams({month:state.month,mode:state.mode,q:state.search,kind:state.kind,page:state.page,group_bikes:"true"});
  const result=await api(`/api/entries?${query}`),lastPage=Math.max(1,Math.ceil((result.display_total ?? result.total)/50));
  if(state.page>lastPage){state.page=lastPage;return entriesView();}
  cacheEntries(result.entries);
  return heading("每一笔，都有来处。",`${monthLabel(state.month)} · 搜索、分类，也可以补上从前的记录。`,modeSwitch())+
    `<form id="search-form" class="toolbar"><label class="search-box">${icon("search")}<input name="q" value="${esc(state.search)}" aria-label="搜索流水" placeholder="搜索商户、分类、用途或备注"></label><select name="kind" aria-label="筛选收支类型"><option value="">全部类型</option>${Object.entries(kindName).map(([k,v])=>`<option value="${k}" ${state.kind===k?"selected":""}>${v}</option>`).join("")}</select><button class="button secondary" type="submit">筛选</button><button class="button secondary" type="button" data-tab="classification">整理分类</button><button class="button secondary" type="button" data-action="salary">${icon("down")}记工资</button></form>`+
    `<section class="card card-pad">${transactionTable(result.entries)}<div class="table-footer"><span>${state.search || state.kind?"筛选匹配":"共"} ${result.total} 笔${result.display_total<result.total?` · 合并展示为 ${result.display_total} 行`:""} · ${state.mode==="cash"?"转账不计入收支":"分摊记录保留原付款日期"}</span>${pagination(state.page,Math.ceil((result.display_total ?? result.total)/50),"page")}</div></section>`;
}
function pagination(page,pages,action) {
  return `<div class="pagination"><button data-action="${action}" data-page="${page-1}" ${page<=1?"disabled":""} aria-label="上一页">←</button><span>${page} / ${Math.max(1,pages)}</span><button data-action="${action}" data-page="${page+1}" ${page>=pages?"disabled":""} aria-label="下一页">→</button></div>`;
}
async function subscriptionsView() {
  const result=await api("/api/subscriptions");cacheEntries(result.entries);
  const status=e=>netAmount(e)===0 && refundedAmount(e)>0?"refunded":state.month<e.split_start?"future":state.month>addMonths(e.split_start,e.split_months-1)?"finished":"active";
  const rows=result.entries.filter(e=>state.subscriptionFilter==="all" || status(e)===state.subscriptionFilter).sort((a,b)=>a.split_start.localeCompare(b.split_start));
  const filters=`<div class="mode-switch">${Object.entries({all:"全部",active:"分摊中",future:"待生效",finished:"已结束",refunded:"已退款"}).map(([k,v])=>`<button data-sub-filter="${k}" class="${state.subscriptionFilter===k?"active":""}">${v}</button>`).join("")}</div>`;
  return heading("把长期的喜欢，慢慢记。",`按 ${monthLabel(state.month)} 查看会员与跨月费用。`,`<button class="button secondary" data-action="membership">${icon("plus")}补录会员</button>`)+`<div class="toolbar">${filters}<span class="muted small">共 ${result.entries.length} 项 · 退款后净额按原期限分摊</span></div>`+
    (rows.length?`<div class="subscriptions-grid">${rows.map(e=>{
      const n=e.split_months,elapsed=Math.max(0,Math.min(n,monthIndex(state.month)-monthIndex(e.split_start)+1));
      const amount=netAmount(e),refunded=refundedAmount(e),recognized=Math.floor(amount/n)*elapsed+Math.min(elapsed,amount%n);
      const label=amount===0 && refunded>0?"已全额退款":{future:"尚未生效",active:"分摊中",finished:"已结束"}[status(e)];
      return `<article class="card subscription"><div class="subscription-top"><span class="transaction-icon">${icon("layers")}</span><span class="tag ${status(e)==="active"?"green":""}">${label}</span></div><div><h3>${esc(e.merchant || e.description || "跨月费用")}</h3><p class="sub-category">${esc(e.category)} · 共 ${n} 个月</p></div><div class="sub-cost">${amount===0 && refunded>0?"—":`${yuan(Math.floor(amount/n))}<small>/ 月起</small>`}</div><div class="sub-period">${monthLabel(e.split_start)} — ${monthLabel(addMonths(e.split_start,n-1))}<br>${refunded?`净额 ${yuan(amount)} · 已退 ${yuan(refunded)}`:`总额 ${yuan(amount)} · 尾差精确分配`}</div><div><div class="progress-track"><div style="width:${elapsed/n*100}%"></div></div><div class="sub-details"><span>已计入 ${elapsed} / ${n} 期</span><span>余 ${yuan(amount-recognized)}</span></div></div><div class="sub-bottom"><span>${esc(e.date)} 付款</span><button class="link-button" data-edit="${e.id}">查看 / 编辑 ${icon("arrow")}</button></div></article>`;
    }).join("")}</div>`:`<section class="card">${empty("长期的支出，也可以从容安排", "补录旧会员，或给已导入的支出设置开始月份与分摊期限。",'<button class="button primary" data-action="membership">添加分摊记录</button>')}</section>`);
}
function importView() {
  if(state.preview)return previewView();
  return heading("让账单，自己走进账本。","导入微信、支付宝明细，确认之后再入账。")+
    `<div class="import-intro"><section class="card card-pad"><div id="dropzone" class="dropzone"><span class="upload-icon">${icon("upload")}</span><h3>把账单放在这里</h3><p>拖放文件，或点击下方按钮选择</p><button class="button primary" data-action="choose-file">选择账单文件 ${icon("arrow")}</button><input id="file-input" type="file" accept=".csv,.xlsx" hidden><p>CSV / XLSX · 最大 8MB · 每次最多 10,000 笔</p></div><div class="note-box">微信：账单 → 常见问题 → 下载账单。支付宝：账单 → 开具交易流水证明 / 导出。入口名称可能随版本变化。下载的 ZIP 包请先在本地解压。</div></section><section class="card card-pad"><div class="card-header"><div><h2>三步，整理好生活</h2><p>保留你的确认，也记住你的习惯。</p></div></div><div class="import-guide">${[["上传原始明细","支持 UTF-8、GBK CSV 和 XLSX，自动识别平台与表头。"],["检查导入预览","查看自动分类、重复交易和需要核对的行。重复记录默认跳过。"],["确认，成为你的账本","导入后可修改分类、设置分摊，或记住商户分类规则。"]].map(([h,p],i)=>`<div class="guide-step"><b>0${i+1}</b><div><h3>${h}</h3><p>${p}</p></div></div>`).join("")}</div><div class="note-box">工资若先到账银行卡，再转入微信或支付宝，后一步属于账户转账。原付款中明确的退款金额会直接抵扣；独立退款只留流水。需要时可在编辑退款时关联原支出，跨月也回冲原付款月份。</div></section></div>`;
}
function previewView() {
  const p=state.preview.preview, duplicates=p.rows.filter(r=>r.duplicate).length;
  const start=(state.previewPage-1)*100, visible=p.rows.slice(start,start+100);
  return heading("入账之前，再看一眼。",`已识别 ${p.source==="wechat"?"微信":"支付宝"}账单。预览保留 30 分钟；重新上传会替换当前预览。`,'<button class="button secondary" data-action="cancel-import">重新选择文件</button>')+
    `<div class="preview-summary"><span class="pill">${p.rows.length} 条明细</span><span class="pill">${duplicates} 条重复</span><span class="pill">${p.skipped} 条未完成交易已跳过</span><span class="pill">${p.errors.length} 条无法解析</span></div><section class="card card-pad"><div class="toolbar"><span class="muted small">早于记账起点的记录默认不选，可按需勾选。原付款按退款后净额入账，独立退款留档不计收支。零元记录会保留；零元共享单车入账后按月收起，预览仍逐笔核对。</span><div><button class="link-button" data-action="select-import">全选可导入项</button><button class="link-button" data-action="clear-import">清空选择</button></div></div><div class="table-scroll"><table class="preview-table"><thead><tr><th>选择</th><th>日期 / 交易对象</th><th>类型</th><th>计入金额</th><th>分类</th><th>核对信息</th></tr></thead><tbody>${visible.map((r,i)=>`<tr class="${r.duplicate?"row-duplicate":""}"><td><input type="checkbox" data-import-index="${start+i}" ${state.selected.has(start+i)?"checked":""} ${r.duplicate?"disabled":""} aria-label="导入第 ${r.line} 行"></td><td><div class="transaction-name"><div><strong>${esc(r.entry.merchant || r.entry.description || "交易")}</strong><small>${esc(r.entry.date)}</small></div></div></td><td>${kindName[r.entry.kind]}</td><td class="amount ${r.entry.amount_cents===0?"zero-amount":r.entry.kind==="income"?"positive":""}">${amountDisplay(r.entry)}</td><td><span class="tag">${esc(r.entry.category)}</span></td><td><div class="preview-warning">${r.duplicate?"重复记录，将跳过":esc(r.warnings.join("；")) || "✓ 可以导入"}</div></td></tr>`).join("")}</tbody></table></div><div class="table-footer"><span>预览第 ${Math.min(start+1,p.rows.length)}–${Math.min(start+100,p.rows.length)} 条</span>${pagination(state.previewPage,Math.ceil(p.rows.length/100),"preview-page")}</div>${p.errors.length?`<details class="errors-list"><summary>${p.errors.length} 条无法解析的行（不会入账）</summary><ul>${p.errors.map(e=>`<li>${esc(e)}</li>`).join("")}</ul></details>`:""}<div class="preview-actions"><span id="selected-count" class="muted">已选择 ${state.selected.size} 条</span><button id="commit-import" class="button primary" data-action="commit-import" ${state.selected.size?"":"disabled"}>确认入账 ${icon("check")}</button></div></section>`;
}
function ruleFeedback(result,label) {return result.matched?`${label}，匹配 ${result.matched} 笔，更新 ${result.updated} 笔分类${result.locked_skipped?`，保留 ${result.locked_skipped} 笔固定分类`:""}`:`${label}，暂无匹配记录`;}
async function classificationView() {
  const version=renderVersion;
  const c=classificationState,query=new URLSearchParams({q:c.search,pending:c.pending,page:c.page});
  if(c.kind)query.set("kind",c.kind);
  if(c.merchant!==null)query.set("merchant",c.merchant);
  const result=await api(`/api/classification?${query}`),pages=Math.max(1,Math.ceil(result.total/50));
  if(version!==renderVersion)return "";
  if(c.page>pages){c.page=pages;return classificationView();}
  cacheEntries(result.entries);c.visible=result.entries;c.merchants=result.merchants;
  const choices=[...new Set([...categories,"工资","年终奖","利息","其他收入",...state.data.rules.map(r=>r.category),...state.pool.values()].map(v=>typeof v==="string"?v:v.category))];
  return heading("把分类，整理清楚。","跨月份整理有金额的收支。信息不足的商户，可分别勾选购物和外卖。",'<button class="button secondary" data-tab="entries">返回流水</button>')+
    `<form id="classification-search" class="toolbar"><label class="search-box">${icon("search")}<input name="q" value="${esc(c.search)}" aria-label="搜索待整理记录" placeholder="搜索商户、商品或备注"></label><select name="kind" aria-label="整理收支类型"><option value="">收入和支出</option>${["expense","income"].map(k=>`<option value="${k}" ${c.kind===k?"selected":""}>${kindName[k]}</option>`).join("")}</select><label class="check-label"><input name="pending" type="checkbox" ${c.pending?"checked":""}>只看待分类</label><button class="button secondary">筛选</button></form>`+
    `<section class="card card-pad"><div class="card-header"><div><h2>按交易对象整理</h2><p>${result.merchants_total} 个交易对象${result.merchants_total>100?" · 先展示 100 个，可搜索缩小范围":""} · 点击查看，分类只影响勾选的记录</p></div></div><div class="merchant-groups"><button class="merchant-chip ${c.merchant===null?"active":""}" data-action="all-merchants">全部</button>${c.merchants.map((g,i)=>`<button class="merchant-chip ${c.merchant===g.merchant?"active":""}" data-merchant-index="${i}"><strong>${esc(g.merchant || "未填写交易对象")}</strong><small>${g.count} 笔 · ${yuan(g.amount_cents)}</small></button>`).join("")}</div></section>`+
    `<section class="card card-pad classification-records"><form id="classification-batch" class="classification-batch"><strong id="classification-selected">已选择 ${c.selected.size} 笔</strong><label>归入分类<input name="category" list="classification-categories" maxlength="60" value="${esc(c.category)}" required placeholder="选择或输入分类"></label><label class="check-label"><input name="locked" type="checkbox" ${c.locked?"checked":""}>固定分类，规则不覆盖</label><button id="classification-save" class="button primary" ${c.selected.size?"":"disabled"}>保存选中记录</button><button type="button" class="link-button" data-action="clear-classification">清空选择</button></form><datalist id="classification-categories">${choices.map(v=>`<option value="${esc(v)}">`).join("")}</datalist><p class="muted small">当前查看${c.merchant===null?"全部交易对象":esc(c.merchant || "未填写交易对象")}。批量保存不创建商户规则；取消固定后，记录可再次跟随规则更新。</p>${result.entries.length?`<div class="table-scroll"><table class="classification-table"><thead><tr><th><input type="checkbox" id="classification-select-page" aria-label="选择当前页" ${result.entries.every(e=>c.selected.has(e.id))?"checked":""}></th><th>日期 / 交易对象 / 商品说明</th><th>分类</th><th class="align-right">净额</th><th></th></tr></thead><tbody>${result.entries.map(e=>`<tr><td><input type="checkbox" data-classification-id="${e.id}" aria-label="选择 ${esc(e.date+' '+e.merchant)}" ${c.selected.has(e.id)?"checked":""}></td><td><strong>${esc(e.merchant || "未填写交易对象")}</strong><small>${esc(e.date)} · ${kindName[e.kind]}</small><p>${esc(e.description || "没有商品说明")}</p></td><td><span class="tag ${e.category==="待分类"?"warn":""}">${esc(e.category)}</span>${e.category_locked?'<small>已固定</small>':""}</td><td class="amount">${yuan(netAmount(e))}</td><td><button class="link-button" data-edit="${e.id}">编辑</button></td></tr>`).join("")}</tbody></table></div>`:empty("这组记录已经整理好了","切换交易对象，或取消“只看待分类”查看已分类的记录。")}<div class="table-footer"><span>共 ${result.total} 笔 · 选择可跨页保留</span>${pagination(c.page,pages,"classification-page")}</div></section>`;
}
function classificationSelection() {
  const c=classificationState;
  $("#classification-selected").textContent=`已选择 ${c.selected.size} 笔`;
  $("#classification-save").disabled=!c.selected.size;
  const all=$("#classification-select-page");
  if(all){const count=c.visible.filter(e=>c.selected.has(e.id)).length;all.checked=count===c.visible.length;all.indeterminate=count>0 && count<c.visible.length;}
}
function settingsView() {
  return heading("一本账本，按你的方式。","调整记账起点、整理分类习惯，或带走自己的数据。")+
    `<div class="settings-grid"><div class="settings-stack"><section class="card card-pad"><h2>记账起点</h2><p>从这个月份开始展示正式收支。此前补录的会员仍按原始期限分摊，保留付款记录。</p><form id="settings-form"><label>正式记账开始月份<input type="month" name="start" value="${state.data.ledger_start.slice(0,7)}" min="1900-01" max="2199-12" required></label><button class="button primary">保存设置</button></form></section><section class="card card-pad"><h2>我的数据</h2><p>导出包含全部流水、分摊信息和分类规则的 JSON 文件。完整恢复请使用服务器数据库备份。</p><button class="button secondary" data-action="export">${icon("down")}导出账本数据</button></section><section class="card card-pad"><h2>私人账户</h2><p>当前登录：${esc(state.data.username)}。修改或找回密码需通过服务器上的账户管理命令完成。</p><button class="button secondary" data-action="logout">${icon("exit")}退出登录</button></section></div><section class="card card-pad"><h2>我的分类习惯</h2><p>商户与商品关键词同时满足才匹配；商品条件优先，再按关键词总长度排序。保存、编辑时同步历史账单；删除后按剩余规则重算，未命中则待分类。固定分类始终保留。</p><button class="button secondary" data-tab="classification">整理待分类记录</button><form id="rule-form" class="inline-form"><input name="rule_id" type="hidden"><label>商户关键词（可选）<input name="pattern" maxlength="200" placeholder="例如：京东"></label><label>商品 / 用途关键词（可选）<input name="description_pattern" maxlength="200" placeholder="例如：外卖、游戏点卡"></label><label>归入分类<input name="category" list="categories" required maxlength="60" placeholder="例如：餐饮"></label><label>适用类型<select name="kind">${Object.entries(kindName).map(([k,v])=>`<option value="${k}">${v}</option>`).join("")}</select></label><p class="span-two small muted">至少填写一个关键词。京东只有订单编号的记录，需到“整理分类”中按实际用途勾选处理。</p><button id="rule-save" class="button secondary">保存并更新账单</button><button type="button" id="rule-cancel" class="link-button" data-action="cancel-rule-edit" hidden>取消编辑</button></form><div class="toolbar"><button class="button secondary" data-action="apply-rules" ${state.data.rules.length?"":"disabled"}>重新应用全部规则</button><span class="muted small">更新未固定的匹配记录</span></div><div class="rules-list">${state.data.rules.length?state.data.rules.map(r=>`<div class="rule-row"><div><strong>${esc(r.pattern || "任意商户")}${r.description_pattern?` + ${esc(r.description_pattern)}`:""} → ${esc(r.category)}</strong><small>${kindName[r.kind]} · ${r.description_pattern?"商品和商户同时匹配":"商户名称包含关键词"}</small></div><button class="link-button" data-edit-rule="${r.id}">编辑</button><button class="link-button danger" data-delete-rule="${r.id}">移除</button></div>`).join(""):empty("还没有自己的分类规则","编辑流水时，可以顺便记住商户的分类习惯。")}</div></section></div>`;
}
async function render() {
  const version=++renderVersion;
  $("#page-name").textContent={overview:"生活概览",entries:"账单流水",subscriptions:"会员与分摊",import:"导入账单",settings:"账本设置",classification:"整理分类"}[state.tab];
  document.querySelectorAll(".sidebar [data-tab]").forEach(b=>{b.classList.toggle("active",b.dataset.tab===(state.tab==="classification"?"entries":state.tab));b.setAttribute("aria-current",b.dataset.tab===(state.tab==="classification"?"entries":state.tab)?"page":"false");});
  $("#month").closest("label").hidden=["import","settings","classification"].includes(state.tab);
  $("#content").innerHTML='<div class="loading"><span>整理你的生活记录…</span></div>';
  try {
    const html=await ({overview,entries:entriesView,subscriptions:subscriptionsView,import:importView,settings:settingsView,classification:classificationView}[state.tab])();
    if(version!==renderVersion || !state.data)return;
    $("#content").innerHTML=html;
    if(state.tab==="import" && !state.preview)bindUpload();
    if(state.tab==="classification")classificationSelection();
  } catch(e) {
    if(version!==renderVersion)return;
    $("#content").innerHTML=empty("暂时没能打开账本",esc(e.message),'<button class="button secondary" data-action="retry">重试</button>');
  }
}
async function refresh(){await bootstrap();await render();}
function changeTab(tab){if(state.tab==="classification" || tab==="classification")classificationState.selected.clear();state.tab=tab;state.page=1;render();}
let categorySuggestions=[],activeCategory=-1;
function categoryChoices() {
  const kind=$("#entry-form").elements.kind.value;
  const defaults=kind==="income"?["工资","年终奖","利息","其他收入","待分类"]:kind==="transfer"?["账户转账"]:categories;
  const saved=[...state.pool.values()].filter(e=>e.kind===kind).map(e=>e.category);
  const rules=(state.data?.rules || []).filter(r=>r.kind===kind).map(r=>r.category);
  return [...new Set([...defaults,...saved,...rules].filter(Boolean))];
}
function closeCategories() {
  $("#category-options").hidden=true;$("#entry-category").setAttribute("aria-expanded","false");
  $("#category-toggle").setAttribute("aria-expanded","false");$("#entry-category").removeAttribute("aria-activedescendant");activeCategory=-1;
}
function openCategories(filter="") {
  const input=$("#entry-category"),list=$("#category-options");
  categorySuggestions=categoryChoices().filter(name=>name.toLowerCase().includes(filter.trim().toLowerCase()));activeCategory=-1;
  list.innerHTML=categorySuggestions.length?categorySuggestions.map((name,i)=>`<button type="button" role="option" id="category-option-${i}" data-category-index="${i}" tabindex="-1" aria-selected="${name===input.value}">${esc(name)}</button>`).join(""):'<p class="category-empty">没有匹配项，可直接输入新分类。</p>';
  list.hidden=false;list.scrollTop=0;input.setAttribute("aria-expanded","true");input.removeAttribute("aria-activedescendant");$("#category-toggle").setAttribute("aria-expanded","true");
}
function chooseCategory(index) {
  if(categorySuggestions[index]===undefined)return;
  const input=$("#entry-category");input.value=categorySuggestions[index];input.focus();closeCategories();input.dispatchEvent(new Event("change",{bubbles:true}));
}
function formKind() {
  const form=$("#entry-form"),kind=form.elements.kind.value;
  $("#split-control").hidden=kind!=="expense";
  $("#expense-refund-control").hidden=kind!=="expense";
  $("#refund-control").hidden=kind!=="refund";
  const choices=kind==="income"?["工资","年终奖","利息","其他收入","待分类"]:kind==="transfer"?["账户转账"]:categories;
  $("#categories").innerHTML=choices.map(c=>`<option value="${esc(c)}">`).join("");
  closeCategories();
  if(kind==="refund")loadRefundTargets();
  else {refundSearchVersion++;clearTimeout(refundSearchTimer);}
  splitHint();
}
function cents(value, allowZero=false) {
  const s=String(value).trim();if(!/^\d+(\.\d{1,2})?$/.test(s))throw new Error(allowZero?"金额须为零或正数，最多两位小数":"金额须为正数，最多两位小数");
  const [a,b=""]=s.split(".");const n=Number(a)*100+Number(b.padEnd(2,"0"));
  if(!Number.isSafeInteger(n) || n<(allowZero?0:1) || n>100000000000)throw new Error(allowZero?"金额须为零或正数，且不超过十亿元":"金额必须大于零且不超过十亿元");return n;
}
const targetLabel = e => `${e.date} · ${e.merchant || e.description || "支出"} · 原付款 ${yuan(e.amount_cents)}`;
async function loadRefundTargets() {
  const f=$("#entry-form").elements,version=++refundSearchVersion;
  clearTimeout(refundSearchTimer);
  if(f.kind.value!=="refund" || !$("#entry-dialog").open)return;
  if(!f.date.value || !f.date.validity.valid){$("#refund-target-hint").textContent="请先填写退款日期，再查找原支出。";return;}
  const selected=f.refund_of.value;
  const selectedLabel=f.refund_of.selectedOptions[0]?.textContent;
  $("#refund-target-hint").textContent="正在查找原支出…";
  try {
    const query=new URLSearchParams({q:f.refund_search.value.trim(),date:f.date.value});
    if(selected)query.set("selected",selected);
    const result=await api(`/api/refund-targets?${query}`);
    if(version!==refundSearchVersion || !$("#entry-dialog").open || f.kind.value!=="refund")return;
    const rows=result.entries;
    f.refund_of.innerHTML='<option value="">不关联，只保留退款流水</option>'+rows.map(e=>`<option value="${e.id}">${esc(targetLabel(e))}</option>`).join("");
    if(selected && !rows.some(e=>String(e.id)===selected)){
      const option=document.createElement("option");option.value=selected;option.textContent=selectedLabel || "已关联的原支出";f.refund_of.append(option);
    }
    f.refund_of.value=selected;
    $("#refund-target-hint").textContent=rows.length?`找到 ${result.total} 笔原支出${result.total>100?"，先显示最近 100 笔，可继续输入关键词缩小范围":""}，包含正式记账前的记录。`:"没有找到原支出。可先补录原付款，再关联这笔退款。";
  }catch(e){if(version===refundSearchVersion)$("#refund-target-hint").textContent=e.message;}
}
function splitHint() {
  const f=$("#entry-form").elements;
  let zero=false;try{zero=cents(f.amount.value,true)===0;}catch{}
  f.split.disabled=zero;if(zero)f.split.checked=false;$("#split-zero-note").hidden=!zero;
  const active=f.split.checked && f.kind.value==="expense";
  $("#split-fields").hidden=!active;f.split_start.required=active;f.split_months.required=active;
  try {
    const n=Number(f.split_months.value),original=cents(f.amount.value,f.kind.value!=="refund"),confirmed=f.kind.value==="expense"?cents(f.refunded.value || "0",true):0;
    const refunded=Math.max(confirmed,editingLinkedRefunds),amount=original-refunded,start=f.split_start.value;
    if(amount<0)throw new Error("累计退款不能超过原始支付金额。");
    $("#expense-refund-hint").textContent=`${editingLinkedRefunds?`已关联退款 ${yuan(editingLinkedRefunds)}；`:""}${original===0?"零元记录会保留在流水中，不影响收支统计。":amount===0?"已全额退款，只保留流水。":`计入支出 ${yuan(amount)}${refunded?"，退款回冲原付款月份":""}。`}`;
    $("#split-hint").textContent=active && start && n>=2 && n<=120 ? `${monthLabel(start)} 至 ${monthLabel(addMonths(start,n-1))}，共 ${n} 期，每月约 ${yuan(Math.floor(amount/n))}。尾差自动分配，净额总计 ${yuan(amount)}。` : "填写开始月份与期限；实际付款日期保持不变。";
  }catch(e){$("#split-hint").textContent="填写金额后会显示分摊预览。";$("#expense-refund-hint").textContent=f.amount.value?e.message:"原始支付金额保持不变，统计使用退款后的净额。";}
}
function openEntry(entry=null, preset="") {
  const form=$("#entry-form");form.reset();const f=form.elements;
  refundSearchVersion++;clearTimeout(refundSearchTimer);editingLinkedRefunds=entry?.linked_refunded_cents || 0;
  $("#entry-error").textContent="";$("#entry-title").textContent=entry?"这一笔，记得更清楚":"记下这一笔";
  const base=entry || (preset==="salary" && state.data.last_salary ? {...state.data.last_salary,id:0,date:today}:{});
  f.id.value=entry?.id || "";f.kind.value=base.kind || (preset==="salary"?"income":"expense");
  f.date.value=base.date || today;f.amount.value=base.amount_cents!==undefined?(base.amount_cents/100).toFixed(2):"";
  f.merchant.value=base.merchant || "";f.description.value=base.description || "";f.note.value=base.note || "";
  f.category.value=base.category || (preset==="salary"?"工资":preset==="membership"?"会员订阅":"待分类");
  f.category_locked.checked=!!base.category_locked;$("#remember-rule-options").hidden=true;
  f.account.value=base.account || (preset==="salary"?"工资卡":"微信");
  f.refunded.value=((base.refunded_cents || 0)/100).toFixed(2);
  f.refund_of.innerHTML='<option value="">不关联，只保留退款流水</option>'+(base.refund_of?`<option value="${Number(base.refund_of)}">${esc(base.refund_target?targetLabel(base.refund_target):"已关联的原支出")}</option>`:"");
  f.refund_of.value=base.refund_of?String(base.refund_of):"";
  $("#entry-import-status").hidden=!base.import_status;$("#entry-import-status").textContent=base.import_status?`账单原始状态：${base.import_status}`:"";
  f.split.checked=!!base.split_start || preset==="membership";f.split_start.value=base.split_start || state.month;f.split_months.value=base.split_months || 12;
  $("#delete-entry").hidden=!entry;$("#entry-dialog").showModal();formKind();f.amount.focus();
}
async function saveEntry(event) {
  event.preventDefault();const form=event.target,f=form.elements,button=$("button[type=submit]",form);button.disabled=true;$("#entry-error").textContent="";
  try {
    const split=f.kind.value==="expense" && f.split.checked;
    const entry={date:f.date.value,kind:f.kind.value,amount_cents:cents(f.amount.value,f.kind.value!=="refund"),merchant:f.merchant.value.trim(),description:f.description.value.trim(),category:f.category.value.trim(),category_locked:f.category_locked.checked,account:f.account.value.trim(),note:f.note.value.trim(),split_start:split?f.split_start.value:null,split_months:split?Number(f.split_months.value):null,refunded_cents:f.kind.value==="expense"?cents(f.refunded.value || "0",true):0,refund_of:f.kind.value==="refund" && f.refund_of.value?Number(f.refund_of.value):null};
    if(split && entry.amount_cents===0)throw new Error("零元记录无需分摊，请取消分摊设置。");
    if(entry.kind==="expense" && Math.max(entry.refunded_cents,editingLinkedRefunds)>entry.amount_cents)throw new Error("累计退款不能超过原始支付金额。");
    if(f.remember_rule.checked && !entry.merchant)throw new Error("记住分类规则时，请填写交易对象。");
    await api(f.id.value?`/api/entries/${f.id.value}`:"/api/entries",{method:f.id.value?"PUT":"POST",body:entry});
    let ruleFailed=false,ruleResult=null;
    if(f.remember_rule.checked)try{ruleResult=await api("/api/rules",{method:"POST",body:{pattern:entry.merchant,description_pattern:f.rule_description.value.trim(),category:entry.category,kind:entry.kind}});}catch{ruleFailed=true;}
    $("#entry-dialog").close();toast(ruleFailed?"记录已保存，分类规则未保存，请在设置中重试":ruleResult?ruleFeedback(ruleResult,"记录与规则已保存"):"已经记下这一笔",ruleFailed);
    await refresh();
  }catch(e){$("#entry-error").textContent=e.message;}finally{button.disabled=false;}
}
function bindUpload() {
  const zone=$("#dropzone"),input=$("#file-input");
  input.addEventListener("change",()=>{if(input.files[0])uploadFile(input.files[0]);});
  zone.addEventListener("dragover",e=>{e.preventDefault();zone.classList.add("dragging");});
  zone.addEventListener("dragleave",()=>zone.classList.remove("dragging"));
  zone.addEventListener("drop",e=>{e.preventDefault();zone.classList.remove("dragging");if(e.dataTransfer.files[0])uploadFile(e.dataTransfer.files[0]);});
}
let uploading=false;
async function uploadFile(file) {
  if(uploading)return;
  if(!/\.(csv|xlsx)$/i.test(file.name) || file.size>8*1024*1024){toast("请选择小于 8MB 的 CSV 或 XLSX 文件",true);return;}
  uploading=true;$("#content").innerHTML='<div class="loading"><span>正在解析账单并检查重复记录…</span></div>';
  try {
    const body=new FormData();body.append("file",file);
    state.preview=await api("/api/import/preview",{method:"POST",body});state.previewPage=1;state.selected.clear();
    state.preview.preview.rows.forEach((r,i)=>{if(!r.duplicate && r.entry.date>=state.data.ledger_start)state.selected.add(i);});
  }catch(e){toast(e.message,true);}finally{uploading=false;if(state.data)render();}
}
async function logout(){await api("/api/logout",{method:"POST"});showLogin();}

document.addEventListener("click",async event=>{
  const b=event.target.closest("button");if(!b || b.disabled)return;
  try {
    if(b.dataset.tab){changeTab(b.dataset.tab);return;}
    if(b.dataset.mode){state.mode=b.dataset.mode;state.page=1;try{localStorage.setItem("tally-mode",state.mode);}catch{}await render();return;}
    if(b.dataset.edit){openEntry(state.pool.get(Number(b.dataset.edit)));return;}
    if(b.dataset.subFilter){state.subscriptionFilter=b.dataset.subFilter;await render();return;}
    if(b.dataset.deleteRule){if(confirm("删除这条分类规则？未固定的匹配账单将按剩余规则重算，未命中则待分类；固定分类会保留。")){const result=await api(`/api/rules/${b.dataset.deleteRule}`,{method:"DELETE"});toast(ruleFeedback(result,"规则已删除，匹配账单已重算"));await refresh();}return;}
    if(b.dataset.editRule){
      const r=state.data.rules.find(r=>r.id===Number(b.dataset.editRule)),f=$("#rule-form").elements;
      f.rule_id.value=r.id;f.pattern.value=r.pattern;f.description_pattern.value=r.description_pattern || "";f.category.value=r.category;f.kind.value=r.kind;
      $("#rule-save").textContent="保存规则修改";$("#rule-cancel").hidden=false;f.pattern.focus();return;
    }
    if(b.dataset.merchantIndex!==undefined){classificationState.merchant=classificationState.merchants[Number(b.dataset.merchantIndex)].merchant;classificationState.page=1;classificationState.selected.clear();await render();return;}
    const action=b.dataset.action;
    if(action==="cancel-rule-edit"){$("#rule-form").reset();$("#rule-form").elements.rule_id.value="";$("#rule-save").textContent="保存并更新账单";$("#rule-cancel").hidden=true;}
    if(action==="all-merchants"){classificationState.merchant=null;classificationState.page=1;classificationState.selected.clear();await render();}
    if(action==="classification-page"){classificationState.page=Number(b.dataset.page);await render();}
    if(action==="clear-classification"){classificationState.selected.clear();document.querySelectorAll("[data-classification-id]").forEach(e=>e.checked=false);classificationSelection();}
    if(action==="apply-rules"){b.disabled=true;try{const result=await api("/api/rules/apply",{method:"POST"});toast(ruleFeedback(result,"全部规则已应用"));await refresh();}finally{b.disabled=false;}}
    if(action==="close-dialog")$("#entry-dialog").close();
    if(action==="add")openEntry();
    if(action==="salary")openEntry(null,"salary");
    if(action==="membership")openEntry(null,"membership");
    if(action==="retry")await render();
    if(action==="toggle-bonus"){state.withBonus=!state.withBonus;await render();}
    if(action==="page"){state.page=Number(b.dataset.page);await render();}
    if(action==="preview-page"){state.previewPage=Number(b.dataset.page);await render();}
    if(action==="choose-file")$("#file-input").click();
    if(action==="cancel-import"){state.preview=null;state.selected.clear();await render();}
    if(action==="select-import"){state.preview.preview.rows.forEach((r,i)=>{if(!r.duplicate)state.selected.add(i);});await render();}
    if(action==="clear-import"){state.selected.clear();await render();}
    if(action==="commit-import"){
      b.disabled=true;
      try{const result=await api("/api/import/commit",{method:"POST",body:{token:state.preview.token,indices:[...state.selected]}});state.preview=null;state.selected.clear();toast(`已入账 ${result.inserted} 笔${result.updated?`，更新 ${result.updated} 笔退款状态`:""}，跳过 ${result.skipped} 笔重复记录`);state.tab="entries";await refresh();}catch(e){b.disabled=false;throw e;}
    }
    if(action==="export"){
      const data=await api("/api/export");const url=URL.createObjectURL(new Blob([JSON.stringify(data,null,2)],{type:"application/json"}));
      const a=document.createElement("a");a.href=url;a.download=`yibi-${today}.json`;a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);toast("账本数据已导出");
    }
    if(action==="logout" || b.id==="logout")await logout();
    if(b.id==="new-entry")openEntry();
    if(b.id==="delete-entry" && confirm("删除这笔记录及其分摊计划？这项操作无法在应用内撤销。")){
      await api(`/api/entries/${$("#entry-form").elements.id.value}`,{method:"DELETE"});$("#entry-dialog").close();toast("记录已删除");await refresh();
    }
  }catch(e){toast(e.message,true);}
});
document.addEventListener("change",event=>{
  const element=event.target,c=classificationState;
  if(element.dataset.classificationId!==undefined){const id=Number(element.dataset.classificationId);element.checked?c.selected.add(id):c.selected.delete(id);classificationSelection();}
  if(element.id==="classification-select-page"){for(const e of c.visible)element.checked?c.selected.add(e.id):c.selected.delete(e.id);document.querySelectorAll("[data-classification-id]").forEach(e=>e.checked=c.selected.has(Number(e.dataset.classificationId)));classificationSelection();}
  if(element.form?.id==="classification-batch" && element.name==="locked")c.locked=element.checked;
  if(element.dataset.importIndex!==undefined){const i=Number(element.dataset.importIndex);element.checked?state.selected.add(i):state.selected.delete(i);$("#selected-count").textContent=`已选择 ${state.selected.size} 条`;$("#commit-import").disabled=!state.selected.size;}
});
document.addEventListener("toggle",event=>{
  const group=event.target;
  if(!group.isConnected || !group.matches?.("details[data-bike-month]"))return;
  group.open?state.expandedBikeGroups.add(group.dataset.bikeMonth):state.expandedBikeGroups.delete(group.dataset.bikeMonth);
},true);
document.addEventListener("submit",async event=>{
  const form=event.target;if(!["search-form","settings-form","rule-form","classification-search","classification-batch"].includes(form.id))return;
  event.preventDefault();const button=$("button[type=submit],button:not([type])",form);if(button)button.disabled=true;
  try{
    if(form.id==="search-form"){state.search=form.elements.q.value.trim();state.kind=form.elements.kind.value;state.page=1;await render();}
    if(form.id==="settings-form"){await api("/api/settings",{method:"PUT",body:{ledger_start:`${form.elements.start.value}-01`}});toast("记账起点已更新");await refresh();}
    if(form.id==="classification-search"){const c=classificationState;c.search=form.elements.q.value.trim();c.kind=form.elements.kind.value;c.pending=form.elements.pending.checked;c.merchant=null;c.page=1;c.selected.clear();await render();}
    if(form.id==="classification-batch"){const c=classificationState,result=await api("/api/entries/categories",{method:"POST",body:{ids:[...c.selected],category:form.elements.category.value.trim(),category_locked:form.elements.locked.checked}});c.selected.clear();toast(`已整理 ${result.selected} 笔，更新 ${result.updated} 笔分类设置`);await refresh();}
    if(form.id==="rule-form"){const f=form.elements,result=await api("/api/rules",{method:"POST",body:{id:f.rule_id.value?Number(f.rule_id.value):null,pattern:f.pattern.value.trim(),description_pattern:f.description_pattern.value.trim(),category:f.category.value.trim(),kind:f.kind.value}});toast(ruleFeedback(result,"规则已保存"));await refresh();}
  }catch(e){toast(e.message,true);}finally{if(button)button.disabled=false;}
});
$("#login-form").addEventListener("submit",async event=>{
  event.preventDefault();const form=event.target,button=$("button",form);button.disabled=true;$("#login-error").textContent="";
  try{await api("/api/login",{method:"POST",body:{username:form.elements.username.value.trim(),password:form.elements.password.value}});form.elements.password.value="";await bootstrap();await render();}
  catch(e){$("#login-error").textContent=e.message;}finally{button.disabled=false;}
});
$("#month").addEventListener("change",async event=>{if(event.target.value && event.target.validity.valid){state.month=event.target.value;state.page=1;await render();}});
$("#entry-form").addEventListener("submit",saveEntry);
$("#entry-category").addEventListener("focus",()=>openCategories());
$("#entry-category").addEventListener("input",event=>openCategories(event.target.value));
$("#category-toggle").addEventListener("click",()=>{
  if(!$("#category-options").hidden){closeCategories();return;}
  $("#entry-category").focus();openCategories();
});
$("#category-options").addEventListener("pointerdown",event=>event.preventDefault());
$("#category-options").addEventListener("click",event=>{
  const option=event.target.closest("[data-category-index]");if(option)chooseCategory(Number(option.dataset.categoryIndex));
});
$("#entry-category").addEventListener("keydown",event=>{
  if(event.isComposing)return;
  const open=!$("#category-options").hidden;
  if(event.key==="Escape" && open){event.preventDefault();event.stopPropagation();closeCategories();return;}
  if(event.key==="Tab"){closeCategories();return;}
  if(event.key==="Enter" && open){event.preventDefault();if(activeCategory>=0)chooseCategory(activeCategory);else closeCategories();return;}
  if(event.key!=="ArrowDown" && event.key!=="ArrowUp")return;
  event.preventDefault();if(!open)openCategories();if(!categorySuggestions.length)return;
  activeCategory=activeCategory<0?(event.key==="ArrowDown"?0:categorySuggestions.length-1):(activeCategory+(event.key==="ArrowDown"?1:-1)+categorySuggestions.length)%categorySuggestions.length;
  const options=$("#category-options").querySelectorAll("[role=option]");options.forEach((option,i)=>option.classList.toggle("active",i===activeCategory));
  const option=options[activeCategory];$("#entry-category").setAttribute("aria-activedescendant",option.id);option.scrollIntoView({block:"nearest"});
});
document.addEventListener("pointerdown",event=>{if(!$("#category-picker").contains(event.target))closeCategories();});
document.addEventListener("input",event=>{if(event.target.form?.id==="classification-batch" && event.target.name==="category")classificationState.category=event.target.value;});
$("#entry-form").elements.remember_rule.addEventListener("change",event=>$("#remember-rule-options").hidden=!event.target.checked);
$("#entry-form").elements.kind.addEventListener("change",formKind);
$("#entry-form").addEventListener("input",splitHint);
$("#entry-form").elements.split.addEventListener("change",splitHint);
$("#entry-form").elements.refund_search.addEventListener("input",()=>{
  refundSearchVersion++;clearTimeout(refundSearchTimer);refundSearchTimer=setTimeout(loadRefundTargets,250);
});
$("#entry-form").elements.date.addEventListener("change",()=>{if($("#entry-form").elements.kind.value==="refund")loadRefundTargets();});
$("#entry-form").elements.refund_of.addEventListener("change",()=>{
  refundSearchVersion++;clearTimeout(refundSearchTimer);
  $("#refund-target-hint").textContent=$("#entry-form").elements.refund_of.value?"已选择原支出，保存后将回冲该笔支出。":"仅留存退款流水。若原支出尚未确认退款，可关联原支出或编辑其已退款金额。";
});
$("#entry-dialog").addEventListener("close",()=>{refundSearchVersion++;clearTimeout(refundSearchTimer);closeCategories();});
$("#categories").innerHTML=categories.map(c=>`<option value="${esc(c)}">`).join("");
(async()=>{try{await bootstrap();await render();}catch(e){showLogin();if(e.message!=="请先登录")$("#login-error").textContent=e.message;}})();
