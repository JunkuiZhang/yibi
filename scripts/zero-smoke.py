#!/usr/bin/env python3
"""Check zero-value imports and monthly ride groups through a disposable server."""
import csv
import http.cookiejar
import io
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("TALLY_TEST_BINARY", ROOT / "target/debug/tally"))


def alipay_fixture():
    """Synthetic GBK bill; contains no data copied from a personal statement."""
    output = io.StringIO(newline="")
    writer = csv.writer(output)
    writer.writerow(["支付宝交易记录明细"])
    writer.writerow(["交易时间", "交易分类", "交易对方", "商品说明", "收/支", "金额",
                     "收/付款方式", "交易状态", "交易订单号"])

    def row(identifier, date, merchant, description, amount="0.00", direction="支出"):
        writer.writerow([date + " 12:00:00", "交通出行", merchant, description,
                         direction, amount, "余额", "交易成功", identifier])

    for index in range(61):
        label = "检索骑行" if index < 10 else "日常骑行"
        row(f"zero-ride-september-{index}", "2026-09-29", "测试共享单车", label)
    for index in range(2):
        row(f"zero-ride-july-{index}", "2026-07-20", "测试共享单车", "日常骑行")
    row("paid-ride", "2026-09-29", "测试共享单车", "收费骑行", "2.00")
    row("zero-card", "2026-09-29", "测试共享单车", "骑行月卡")
    row("zero-voucher", "2026-09-29", "零元普通商品", "优惠兑换")
    row("zero-transfer", "2026-09-29", "测试账户", "账户转账", direction="不计收支")
    row("zero-income", "2026-09-29", "测试入账", "零元收入", direction="收入")
    return output.getvalue().encode("gbk")


def main():
    assert BIN.exists(), "Build the executable with cargo build first"
    with tempfile.TemporaryDirectory(prefix="tally-zero-smoke-") as directory:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = {**os.environ, "TALLY_DB": str(Path(directory) / "test.sqlite3"),
               "TALLY_BIND": f"127.0.0.1:{port}"}
        password = secrets.token_urlsafe(24)
        subprocess.run([str(BIN), "init", "owner", "--password-stdin"], input=password,
                       text=True, env=env, check=True, capture_output=True)
        process = subprocess.Popen([str(BIN), "serve", "--dev"], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        base = f"http://127.0.0.1:{port}"
        checks = 0

        def request(path, data=None, method=None, expected=200, headers=None):
            nonlocal checks
            method = method or ("POST" if data is not None else "GET")
            headers = dict(headers or {})
            if method != "GET":
                headers["X-Tally-Request"] = "1"
            if data is not None and not isinstance(data, bytes):
                data = json.dumps(data).encode()
                headers["Content-Type"] = "application/json"
            req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
            try:
                response = opener.open(req, timeout=15)
            except urllib.error.HTTPError as error:
                response = error
            body = response.read()
            assert response.code == expected, (path, response.code, body[:500])
            checks += 1
            return json.loads(body) if body else None

        def preview(content):
            boundary = "tally-zero-" + secrets.token_hex(8)
            data = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; '
                    f'filename="synthetic-alipay.csv"\r\nContent-Type: application/octet-stream'
                    f'\r\n\r\n'.encode() + content + f'\r\n--{boundary}--\r\n'.encode())
            return request("/api/import/preview", data,
                           headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})

        def list_rows(**params):
            return request("/api/entries?" + urllib.parse.urlencode(params))

        def all_grouped(**params):
            first = list_rows(group_bikes="true", **params)
            rows = first["entries"].copy()
            for page in range(2, (first["display_total"] + 49) // 50 + 1):
                rows.extend(list_rows(group_bikes="true", page=page, **params)["entries"])
            assert len(rows) == first["display_total"]
            return first, rows

        def groups(rows):
            return [row for row in rows if row.get("row_type") == "bike_group"]

        template = dict(date="2026-09-01", kind="expense", amount_cents=100,
                        merchant="测试普通商户", description="合成测试", category="日常补给",
                        account="支付宝", note="", split_start=None, split_months=None,
                        refunded_cents=0, refund_of=None)

        def add(**fields):
            return request("/api/entries", {**template, **fields})["id"]

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
            request("/api/login", {"username": "owner", "password": password})

            bill = alipay_fixture()
            parsed = preview(bill)
            rows = parsed["preview"]["rows"]
            assert parsed["preview"]["source"] == "alipay"
            assert len(rows) == 68 and not parsed["preview"]["errors"]
            assert sum(row["entry"]["amount_cents"] == 0 for row in rows) == 67
            result = request("/api/import/commit", {"token": parsed["token"],
                                                    "indices": list(range(len(rows)))})
            assert result == {"inserted": 68, "skipped": 0, "updated": 0}

            grouped, displayed = all_grouped(month="2026-09")
            rides = groups(displayed)
            assert len(rides) == 1 and rides[0]["count"] == 61
            assert rides[0]["month"] == "2026-09" and len(rides[0]["entries"]) == 61
            assert all(row["amount_cents"] == 0 and "net_amount_cents" in row
                       for row in rides[0]["entries"])
            imported_ids = {row["id"] for row in request("/api/export")["entries"]}
            parsed = preview(bill)
            assert all(row["duplicate"] for row in parsed["preview"]["rows"])
            assert request("/api/import/commit", {"token": parsed["token"],
                        "indices": list(range(68))}) == {"inserted": 0, "skipped": 68, "updated": 0}
            assert {row["id"] for row in request("/api/export")["entries"]} == imported_ids
            assert groups(all_grouped(month="2026-09")[1])[0]["count"] == 61

            # Enough regular entries to force display pagination after grouping.
            for index in range(55):
                add(description=f"普通明细 {index}")
            fully_refunded = add(date="2026-09-29", merchant="测试共享单车",
                                 description="已退骑行", amount_cents=500, refunded_cents=500)
            add(date="2026-07-05", kind="income", amount_cents=2500000,
                category="工资", merchant="测试公司")
            august_salary = add(date="2026-08-05", kind="income", amount_cents=2200000,
                                category="工资", merchant="测试公司")
            add(date="2026-09-30", kind="income", amount_cents=0,
                category="工资", merchant="测试公司")
            zero_expense = add(amount_cents=0, merchant="零元待分类支出", category="待分类")
            add(kind="income", amount_cents=0, category="待分类")
            add(kind="transfer", amount_cents=0, merchant="零元单车转账", description="日常骑行")
            request("/api/entries", {**template, "kind": "refund", "amount_cents": 0}, expected=400)
            request("/api/entries", {**template, "amount_cents": -1}, expected=400)
            request("/api/entries", {**template, "amount_cents": 0,
                    "split_start": "2026-09", "split_months": 12}, expected=400)

            raw = request("/api/export")["entries"]
            september_raw = [row for row in raw if row["date"].startswith("2026-09")]
            legacy = list_rows(month="2026-09")
            assert legacy["total"] == len(september_raw) and len(legacy["entries"]) == 50
            assert not groups(legacy["entries"])
            grouped, displayed = all_grouped(month="2026-09")
            assert grouped["total"] == len(september_raw)
            assert grouped["display_total"] == len(september_raw) - 60
            assert len(displayed) > 50 and len(groups(displayed)) == 1
            assert groups(displayed)[0]["count"] == 61
            normal = [row for row in displayed if row.get("row_type") != "bike_group"]
            normal_ids = {row["id"] for row in normal}
            excluded = [row for row in september_raw if row["external_id"] in
                        ("paid-ride:expense", "zero-card:expense", "zero-voucher:expense")]
            assert len(excluded) == 3 and all(row["id"] in normal_ids for row in excluded)
            assert fully_refunded in normal_ids
            expected_ids = normal_ids | {row["id"] for row in groups(displayed)[0]["entries"]}
            assert expected_ids == {row["id"] for row in september_raw}
            assert len(expected_ids) == grouped["total"]

            _, every_month = all_grouped()
            assert {row["month"]: row["count"] for row in groups(every_month)} == {
                "2026-07": 2, "2026-09": 61}
            searched, matches = all_grouped(month="2026-09", q="检索骑行")
            assert searched["total"] == 10 and searched["display_total"] == 1
            assert len(matches) == 1 and matches[0]["count"] == 10
            assert all("检索骑行" in row["description"] for row in matches[0]["entries"])
            _, expenses = all_grouped(month="2026-09", kind="expense")
            assert groups(expenses)[0]["count"] == 61
            _, income = all_grouped(month="2026-09", kind="income")
            assert not groups(income) and all(row["kind"] == "income" for row in income)
            _, transfers = all_grouped(month="2026-09", kind="transfer")
            assert not groups(transfers) and all(row["kind"] == "transfer" for row in transfers)
            _, cards = all_grouped(month="2026-09", q="月卡")
            assert len(cards) == 1 and not groups(cards)

            for mode in ("cash", "allocated"):
                summary = request(f"/api/summary?month=2026-09&mode={mode}")
                assert (summary["income"], summary["expense"], summary["unclassified"]) == (0, 5700, 0)
                assert summary["income_months"] == 2 and summary["income_average"] == 2350000
            assert request("/api/state")["last_salary"]["id"] == august_salary
            candidates = request("/api/refund-targets?date=2026-10-01")["entries"]
            assert candidates and all(row["amount_cents"] > 0 for row in candidates)
            candidates = request(f"/api/refund-targets?selected={zero_expense}")["entries"]
            assert all(row["id"] != zero_expense for row in candidates)

            # Group children remain the original independently editable records.
            first, second = groups(displayed)[0]["entries"][:2]
            request(f'/api/entries/{first["id"]}', {**first, "amount_cents": 100}, method="PUT")
            _, changed = all_grouped(month="2026-09")
            assert groups(changed)[0]["count"] == 60
            assert any(row.get("id") == first["id"] for row in changed)
            assert request(f'/api/entries/{second["id"]}', method="DELETE")["deleted"] == 1
            _, changed = all_grouped(month="2026-09")
            assert groups(changed)[0]["count"] == 59
            request(f'/api/entries/{first["id"]}', {**first, "date": "2026-07-29"}, method="PUT")
            _, changed = all_grouped()
            assert {row["month"]: row["count"] for row in groups(changed)} == {
                "2026-07": 3, "2026-09": 59}
            exported = request("/api/export")["entries"]
            assert len(exported) == len(raw) - 1
            assert not groups(exported) and all("id" in row for row in exported)
            assert len({row["id"] for row in exported}) == len(exported)
            print(json.dumps({"status": "passed", "http_checks": checks,
                              "gbk_zero_import": "passed", "monthly_ride_groups": "passed",
                              "pagination_filtering_editing": "passed",
                              "income_statistics": "passed", "original_export": "passed"}))
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
