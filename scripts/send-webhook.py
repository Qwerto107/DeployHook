#!/usr/bin/env python3
"""CI 构建并推送镜像成功后调用，密钥从环境变量读取。"""

import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    url = os.environ["DEPLOYHOOK_URL"]
    target = urllib.parse.urlsplit(url)
    if target.scheme != "https" or not target.hostname or target.username or target.password:
        raise ValueError("DEPLOYHOOK_URL 必须为不含用户名和密码的 HTTPS 地址")
    secret = os.environ["DEPLOYHOOK_SECRET"].encode("ascii")
    if not 32 <= len(secret) <= 256 or not all(33 <= byte <= 126 for byte in secret):
        raise ValueError("密钥必须为 32-256 字节的 ASCII 文本，且不含空白")
    timestamp = str(int(time.time()))
    # 签名和发送使用完全相同的 UTF-8 字节，保留原始空格与换行。
    body = os.environ.get("DEPLOYHOOK_BODY", "{}").encode("utf-8")
    if len(body) > 16 * 1024:
        raise ValueError("DEPLOYHOOK_BODY 不能超过 16 KiB")
    json.loads(body)
    signature = hmac.new(secret, timestamp.encode() + b"." + body, hashlib.sha256).hexdigest()
    request = urllib.request.Request(
        url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "X-Deploy-Timestamp": timestamp,
            "X-Deploy-Signature": "sha256=" + signature,
        },
        method="POST",
    )

    # 禁止重定向，避免认证头被转发给其他站点；不自动重试可能已受理的请求。
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None

    opener = urllib.request.build_opener(NoRedirect)
    with opener.open(request, timeout=25) as response:
        if response.status != 202:
            raise RuntimeError(f"非预期响应：HTTP {response.status}")
        print("更新触发已受理；实际执行结果请查看服务器日志。")


if __name__ == "__main__":
    try:
        main()
    except urllib.error.HTTPError as error:
        print(f"Webhook 请求失败：HTTP {error.code}", file=sys.stderr)
        sys.exit(1)
    except (KeyError, ValueError, RuntimeError, OSError) as error:
        print(f"Webhook 请求失败：{error}", file=sys.stderr)
        sys.exit(1)
