#!/usr/bin/env python3
"""Exercise the real binary against a disposable database, using only Python's stdlib."""
import http.cookiejar
import io
import json
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from xml.sax.saxutils import escape
import zipfile

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("TALLY_TEST_BINARY", ROOT / "target/debug/tally"))


def workbook(rows):
    """Minimal real XLSX with inline strings, matching downloadable bill layout."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
        z.writestr("_rels/.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        z.writestr("xl/workbook.xml", '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="账单" sheetId="1" r:id="rId1"/></sheets></workbook>')
        z.writestr("xl/_rels/workbook.xml.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
        xml_rows = []
        for i, row in enumerate(rows, 1):
            cells = ''.join(f'<c r="{chr(65+j)}{i}" t="inlineStr"><is><t>{escape(str(v))}</t></is></c>' for j, v in enumerate(row))
            xml_rows.append(f'<row r="{i}">{cells}</row>')
        z.writestr("xl/worksheets/sheet1.xml", '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>' + ''.join(xml_rows) + '</sheetData></worksheet>')
    return out.getvalue()


def refund_checks(request, preview, template, header):
    """Check refund attribution through the real HTTP API using synthetic entries."""
    created = []
    base = {**template, "split_start":None, "split_months":None,
            "refunded_cents":0, "refund_of":None}

    def add(**fields):
        payload = {**base, **fields}
        identifier = request("/api/entries", payload)["id"]
        created.append(identifier)
        return identifier, payload

    def rows(month, mode="cash"):
        return request(f"/api/entries?month={month}&mode={mode}")["entries"]

    def saved(identifier, month, mode="cash"):
        return next(e for e in rows(month, mode) if e["id"] == identifier)

    def expense(month, mode="cash"):
        return request(f"/api/summary?month={month}&mode={mode}")["expense"]

    def remove(identifier):
        assert request(f"/api/entries/{identifier}", method="DELETE")["deleted"] == 1
        created.remove(identifier)

    def imported(row):
        csv = ("微信支付账单明细\n" + ",".join(header) + "\n" + ",".join(row) + "\n").encode()
        result = preview("refund-fixture.csv", csv)
        return result, request("/api/import/commit", {"token":result["token"], "indices":[0]})

    try:
        original, _ = add(date="2029-03-10", amount_cents=10000, merchant="refund-test-payment")
        first, first_data = add(date="2029-05-01", kind="refund", amount_cents=2500,
                                merchant="refund-test-payment", refund_of=original)
        assert expense("2029-03") == 7500
        may = request("/api/summary?month=2029-05")
        assert (may["expense"], may["income"]) == (0, 0)
        payment = saved(original, "2029-03")
        assert (payment["amount_cents"], payment["refunded_total_cents"], payment["net_amount_cents"]) == (10000, 2500, 7500)
        refund = saved(first, "2029-05")
        assert refund["net_amount_cents"] == 0 and refund["refund_target"]["id"] == original

        orphan, _ = add(date="2029-05-02", kind="refund", amount_cents=800,
                        merchant="refund-test-unlinked")
        may = request("/api/summary?month=2029-05")
        assert (may["expense"], may["income"]) == (0, 0)
        orphan_row = saved(orphan, "2029-05")
        assert orphan_row["refund_of"] is None and orphan_row["net_amount_cents"] == 0

        second, second_data = add(date="2029-06-01", kind="refund", amount_cents=7500,
                                  merchant="refund-test-payment", refund_of=original)
        assert expense("2029-03") == 0 and expense("2029-06") == 0
        payment = saved(original, "2029-03")
        assert (payment["amount_cents"], payment["refunded_total_cents"], payment["linked_refunded_cents"], payment["net_amount_cents"]) == (10000, 10000, 10000, 0)
        request("/api/entries", {**first_data, "refund_of":99999999}, expected=400)
        request("/api/entries", {**first_data, "refund_of":orphan}, expected=400)
        request("/api/entries", {**first_data, "date":"2029-03-09"}, expected=400)
        request("/api/entries", {**first_data, "amount_cents":1}, expected=400)
        request(f"/api/entries/{second}", {**second_data, "amount_cents":7501}, method="PUT", expected=400)
        request(f"/api/entries/{original}", method="DELETE", expected=400)
        request(f"/api/entries/{first}", {**first_data, "refund_of":None}, method="PUT")
        request(f"/api/entries/{second}", {**second_data, "refund_of":None}, method="PUT")
        remove(original)
        assert {first, orphan}.issubset({e["id"] for e in rows("2029-05")})

        membership, _ = add(date="2029-07-10", amount_cents=14800,
                            refunded_cents=4001, merchant="refund-test-membership",
                            split_start="2030-02", split_months=12)
        add(date="2029-08-01", kind="refund", amount_cents=3000,
            merchant="refund-test-membership", refund_of=membership)
        member = saved(membership, "2029-07")
        assert (member["refunded_cents"], member["linked_refunded_cents"], member["refunded_total_cents"], member["net_amount_cents"]) == (4001, 3000, 4001, 10799)
        assert expense("2029-07") == 10799 and expense("2029-08") == 0
        allocated = []
        for i in range(12):
            index = 2030 * 12 + 1 + i
            allocated.append(expense(f"{index//12:04}-{index%12+1:02}", "allocated"))
        assert sum(allocated) == 10799 and max(allocated) - min(allocated) == 1
        subscriptions = request("/api/subscriptions")["entries"]
        assert next(e for e in subscriptions if e["id"] == membership)["net_amount_cents"] == 10799
        add(date="2029-09-01", kind="refund", amount_cents=11800,
            merchant="refund-test-membership", refund_of=membership)
        assert expense("2030-04", "allocated") == 0
        assert saved(membership, "2030-04", "allocated")["net_amount_cents"] == 0

        historical, _ = add(date="2026-01-05", amount_cents=6000,
                            merchant="refund-test-historical")
        targets = request("/api/refund-targets?q=refund-test-historical&date=2026-07-15")
        assert any(e["id"] == historical for e in targets["entries"])
        assert all(e["id"] != historical for e in request("/api/entries?q=refund-test-historical")["entries"])
        add(date="2026-07-15", kind="refund", amount_cents=6000,
            merchant="refund-test-historical", refund_of=historical)
        assert saved(historical, "2026-01")["net_amount_cents"] == 0

        row = ["2029/10/01 12:00:00", "商户消费", "refund-update-shop", "测试会员",
               "支出", "148.00", "零钱", "支付成功", "refund-update-wx-1"]
        _, result = imported(row)
        assert result == {"inserted":1, "skipped":0, "updated":0}
        imported_entry = next(e for e in rows("2029-10") if e["merchant"] == "refund-update-shop")
        imported_id = imported_entry["id"]
        created.append(imported_id)
        custom = {**imported_entry, "category":"自定义退款验收", "note":"保留此备注与原分摊期限",
                  "split_start":"2040-02", "split_months":12}
        request(f"/api/entries/{imported_id}", custom, method="PUT")
        original_count = len(request("/api/export")["entries"])
        row[7] = "已退款(¥48.01)"
        p, result = imported(row)
        assert not p["preview"]["rows"][0]["duplicate"]
        assert result == {"inserted":0, "skipped":0, "updated":1}
        changed = saved(imported_id, "2029-10")
        assert (changed["refunded_cents"], changed["net_amount_cents"], changed["import_status"]) == (4801, 9999, row[7])
        for field in ("category", "note", "split_start", "split_months"):
            assert changed[field] == custom[field], field
        assert len(request("/api/export")["entries"]) == original_count
        _, result = imported(row)
        assert result == {"inserted":0, "skipped":1, "updated":0}
        row[7] = "已全额退款"
        _, result = imported(row)
        assert result == {"inserted":0, "skipped":0, "updated":1}
        assert saved(imported_id, "2040-04", "allocated")["net_amount_cents"] == 0
    finally:
        # Refunds are created after their parents, so reversing creation order keeps
        # referential protections intact and restores the nine baseline fixtures.
        for identifier in reversed(created.copy()):
            remove(identifier)


def main():
    assert BIN.exists(), "Build the executable with cargo build first"
    with tempfile.TemporaryDirectory(prefix="tally-smoke-") as directory:
        db = Path(directory) / "test.sqlite3"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = {**os.environ, "TALLY_DB": str(db), "TALLY_BIND": f"127.0.0.1:{port}"}
        password = secrets.token_urlsafe(24)
        subprocess.run([str(BIN), "init", "owner", "--password-stdin"], input=password, text=True, env=env, check=True, capture_output=True)
        process = subprocess.Popen([str(BIN), "serve", "--dev"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        base = f"http://127.0.0.1:{port}"
        checks = 0

        def request(path, data=None, method=None, expected=200, csrf=True, headers=None):
            nonlocal checks
            method = method or ("POST" if data is not None else "GET")
            headers = dict(headers or {})
            if csrf and method != "GET":
                headers["X-Tally-Request"] = "1"
            if data is not None and not isinstance(data, bytes):
                data = json.dumps(data).encode()
                headers["Content-Type"] = "application/json"
            req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
            try:
                response = opener.open(req, timeout=15)
            except urllib.error.HTTPError as e:
                response = e
            body = response.read()
            assert response.code == expected, (path, response.code, body[:500])
            checks += 1
            return json.loads(body) if body else None

        def preview(filename, content, expected=200):
            boundary = "tally-" + secrets.token_hex(8)
            data = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode() + content + f'\r\n--{boundary}--\r\n'.encode())
            return request("/api/import/preview", data, expected=expected, headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})

        try:
            for _ in range(80):
                try:
                    opener.open(base + "/health", timeout=1).close()
                    break
                except OSError:
                    if process.poll() is not None:
                        raise RuntimeError(process.stderr.read().decode())
                    time.sleep(.1)
            else:
                raise RuntimeError("Server did not start")
            request("/api/entries", expected=401)
            request("/api/login", {"username":"owner", "password":password}, csrf=False, expected=403)
            request("/api/login", {"username":"owner", "password":password})
            template = dict(date="2026-07-05", kind="expense", amount_cents=14800, merchant="视频会员", description="年度会员", category="会员订阅", account="支付宝", note="", split_start="2027-02", split_months=12)
            membership = request("/api/entries", template)["id"]
            request("/api/entries", {**template, "date":"2026-02-01", "split_start":"2026-02", "amount_cents":12000})
            salary = {**template, "kind":"income", "merchant":"公司", "description":"工资", "category":"工资", "account":"工资卡", "split_start":None, "split_months":None}
            request("/api/entries", {**salary, "amount_cents":2500000})
            request("/api/entries", {**salary, "date":"2026-08-05", "amount_cents":2200000})
            request("/api/entries", {**salary, "category":"年终奖", "amount_cents":1000000})
            request("/api/entries", {**salary, "kind":"transfer", "category":"账户转账"})
            request("/api/entries", {**template, "split_months":0}, expected=400)
            request("/api/entries", {**template, "amount_cents":-1}, expected=400)
            cash = request("/api/summary?month=2026-07&mode=cash")
            assert (cash["income"], cash["expense"], cash["bonus"]) == (3500000, 14800, 1000000)
            allocated = request("/api/summary?month=2026-07&mode=allocated")
            assert allocated["expense"] == 1000 and allocated["pending"] == 20800
            total = 0
            for i in range(12):
                index = 2027 * 12 + 1 + i
                month = f"{index//12:04}-{index%12+1:02}"
                total += request(f"/api/summary?month={month}&mode=allocated")["expense"]
            assert total == 14800, total
            august = request("/api/summary?month=2026-08")
            assert august["income"] == 2200000 and august["income_months"] == 2
            header = ["交易时间","交易类型","交易对方","商品","收/支","金额(元)","支付方式","当前状态","交易单号"]
            row = ["2026-07-01 12:00:00","商户消费","咖啡店","拿铁","支出","18.50","零钱","支付成功","test-wx-1"]
            csv = ("微信支付账单明细\n" + ",".join(header) + "\n" + ",".join(row) + "\n" + ",".join(row)).encode()
            p = preview("wechat.csv", csv)
            assert p["preview"]["rows"][1]["duplicate"]
            result = request("/api/import/commit", {"token":p["token"], "indices":[0,1]})
            assert result == {"inserted":1,"skipped":1,"updated":0}
            p = preview("wechat.csv", csv)
            assert all(r["duplicate"] for r in p["preview"]["rows"])
            row[-1] = "test-xlsx-2"
            row[0] = "2026/07/01 12:00:00"
            p = preview("wechat.xlsx", workbook([header, row]))
            assert p["preview"]["rows"][0]["entry"]["amount_cents"] == 1850
            request("/api/import/commit", {"token":p["token"], "indices":[0]})
            ali = "支付宝交易记录明细\n交易时间,交易分类,交易对方,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号\n2026-07-02 10:00:00,日用,某超市,用品,支出,32.00,花呗,交易成功,test-ali-1\n".encode("gbk")
            p = preview("alipay.csv", ali)
            assert p["preview"]["source"] == "alipay"
            request("/api/import/commit", {"token":p["token"], "indices":[0]})
            request("/api/rules", {"pattern":"超市","category":"日常补给","kind":"expense"})
            assert request("/api/state")["rules"][0]["category"] == "日常补给"
            p = preview("alipay.csv", ali)
            assert p["preview"]["rows"][0]["entry"]["category"] == "日常补给"
            request(f"/api/entries/{membership}", {**template, "note":"修改备注不会重复扣款"}, method="PUT")
            refund_checks(request, preview, template, header)
            exported = request("/api/export")
            assert len(exported["entries"]) == 9 and "password_hash" not in json.dumps(exported)
            backup = Path(directory) / "backup.sqlite3"
            subprocess.run([str(BIN), "backup", str(backup)], env=env, check=True, capture_output=True)
            with sqlite3.connect(backup) as connection:
                assert connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                assert connection.execute("SELECT count(*) FROM entries").fetchone()[0] == 9
            request("/api/logout", method="POST")
            request("/api/export", expected=401)
            status = Path(f"/proc/{process.pid}/status").read_text()
            memory = [line for line in status.splitlines() if line.startswith(("VmRSS:", "VmHWM:"))]
            print(json.dumps({"status":"passed", "http_checks":checks, "xlsx":"passed", "refunds":"passed", "backup":"passed", "memory":memory}, ensure_ascii=False))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
