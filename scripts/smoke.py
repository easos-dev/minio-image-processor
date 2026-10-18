#!/usr/bin/env python3
"""部署后的一次性真实联调：经图片处理网关（即对象存储的公开入口）读写真实 MinIO，不操作已有对象。

覆盖直传、签名读取逐字节一致、Range/HEAD/304、签名校验透传、OSS 处理串加工与缓存、OSS 错误码；
给出 --public-prefix 时再核对匿名读取（运维须事先给该前缀配好匿名只读策略，本脚本不改桶策略）。
密钥只从 0600 文件读取；URL、凭据和上游错误正文不进入输出。所有网络请求单次执行，
finally 清理本次随机对象。此检查不替代浏览器与 EasOS 实际路径。
"""
import argparse
import base64
import hashlib
import io
import json
from pathlib import Path
import stat
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

# EasOS File 实际使用的两条处理串（a = 等比放进，b = 短边缩到边长后居中裁剪）。
LFIT_256 = 'image/auto-orient,1/resize,m_lfit,w_256,h_256,limit_1/quality,q_90/format,webp'
FILL_64 = 'image/auto-orient,1/resize,m_fill,w_64,h_64,limit_1/quality,q_90/format,webp'


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def storage_origin(value):
    """HTTPS origin；本机联调允许 http://127.0.0.1 或 http://localhost。"""
    parsed = urllib.parse.urlsplit(value)
    loopback = parsed.scheme == 'http' and parsed.hostname in ('127.0.0.1', 'localhost')
    if (parsed.scheme != 'https' and not loopback) or not parsed.hostname or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment:
        raise argparse.ArgumentTypeError('需要无路径和凭据的 HTTPS origin（本机联调可用 http://127.0.0.1）')
    return value


def secret(path):
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077:
        raise ValueError('凭据文件必须仅当前账户可读')
    value = path.read_text().strip()
    if not value:
        raise ValueError('凭据为空')
    return value


TLS_CONTEXT = None


def http(url, method='GET', data=None, headers=None):
    # 禁用环境代理、自动重定向；只使用调用者明确给出的入口。
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(), urllib.request.HTTPSHandler(context=TLS_CONTEXT))
    request = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        response = opener.open(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        payload = response.read(64 * 1024 * 1024 + 1)
        if len(payload) > 64 * 1024 * 1024:
            raise ValueError('响应超过测试预算')
        return response.status, response.headers, payload


def check(condition):
    if not condition:
        raise AssertionError('检查结果不符')


def run(args):
    import boto3
    from botocore.auth import S3SigV4QueryAuth
    from botocore.awsrequest import AWSRequest
    from botocore.config import Config
    from botocore.credentials import Credentials
    from PIL import Image

    global TLS_CONTEXT
    # Python 3.13 create_default_context 额外启用 RFC 严格扩展检查，旧企业根
    # 没有 KeyUsage 时会被它拒绝。使用标准 TLS 客户端上下文，与 curl 一致；
    # CERT_REQUIRED、证书链、有效期和 check_hostname 全部保留，不忽略 TLS 错误。
    TLS_CONTEXT = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    if args.ca_file:
        TLS_CONTEXT.load_verify_locations(cafile=str(args.ca_file))
    else:
        TLS_CONTEXT.load_default_certs()
    started, stage = time.monotonic(), 'client_setup'
    access, secret_key = secret(args.access_key_file), secret(args.secret_key_file)
    client = boto3.client('s3', endpoint_url=args.storage, region_name='us-east-1',
        verify=str(args.ca_file) if args.ca_file else True,
        aws_access_key_id=access, aws_secret_access_key=secret_key,
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'},
            retries={'total_max_attempts': 1}, connect_timeout=5, read_timeout=20, proxies={}))
    credentials = Credentials(access, secret_key)

    def signed(key, query='', method='GET'):
        """把 query（如 x-oss-process）一并签进预签名地址，与 EasOS File 的私有读取地址相同。
        SigV4 签名绑定方法，HEAD 要单独签。"""
        url = f'{args.storage}/{args.bucket}/{urllib.parse.quote(key)}' + (f'?{query}' if query else '')
        request = AWSRequest(method=method, url=url)
        S3SigV4QueryAuth(credentials, 's3', 'us-east-1', expires=60).add_auth(request)
        return request.url

    def process(value):
        return urllib.parse.urlencode({'x-oss-process': value})

    image = Image.new('RGB', (800, 600))
    image.putdata([(x % 256, y % 256, (x + y) % 256) for y in range(600) for x in range(800)])
    output = io.BytesIO(); image.save(output, format='PNG'); payload = output.getvalue()
    prefix = 'image-processor-check-' + uuid.uuid4().hex + '/'
    key, bad_key = prefix + '测试 image.png', prefix + 'bad-md5.png'
    public_key = (args.public_prefix + prefix + 'public.png') if args.public_prefix else None
    own_keys = [k for k in (key, bad_key, public_key) if k]
    cleanup = False
    passed = []
    origin_headers = {'Origin': args.gui_origin} if args.gui_origin else {}
    try:
        stage = 'presigned_conditional_upload'
        md5 = base64.b64encode(hashlib.md5(payload).digest()).decode()
        params = dict(Bucket=args.bucket, Key=key, ContentType='image/png', ContentMD5=md5, IfNoneMatch='*')
        put_url = client.generate_presigned_url('put_object', Params=params, ExpiresIn=300)
        headers = {'content-type': 'image/png', 'content-md5': md5, 'if-none-match': '*', **origin_headers}
        # 发出写请求前即标记需清理，超时也可能已写入；只删除随机自有键。
        cleanup = True
        status, response_headers, _ = http(put_url, 'PUT', payload, headers)
        check(status == 200)
        if args.gui_origin:
            check(response_headers.get('access-control-allow-origin') == args.gui_origin)
        check(http(put_url, 'PUT', payload, headers)[0] == 412)
        passed.append(stage)

        stage = 'md5_validation'
        bad_md5 = base64.b64encode(bytes(16)).decode()
        params.update(Key=bad_key, ContentMD5=bad_md5)
        bad_url = client.generate_presigned_url('put_object', Params=params, ExpiresIn=300)
        check(http(bad_url, 'PUT', payload, {**headers, 'content-md5': bad_md5})[0] == 400)
        passed.append(stage)

        stage = 'passthrough_integrity'
        meta = client.head_object(Bucket=args.bucket, Key=key)
        check(meta['ContentLength'] == len(payload) and meta['ContentType'] == 'image/png')
        url = signed(key)
        status, got, data = http(url, headers=origin_headers)
        check(status == 200 and data == payload and 'x-image-cache' not in got)
        if args.direct:
            # 网关与直连上游逐字节一致（同一签名地址换成上游 origin，Host 头保持网关的公开 Host）。
            direct = args.direct + url[len(args.storage):]
            host = urllib.parse.urlsplit(args.storage).netloc
            check(http(direct, headers={'Host': host})[2] == data)
        status, partial, data = http(url, headers={'Range': 'bytes=10-19'})
        check(status == 206 and data == payload[10:20] and partial.get('content-range') == f'bytes 10-19/{len(payload)}')
        status, head, data = http(signed(key, method='HEAD'), 'HEAD')
        check(status == 200 and data == b'' and head.get('content-length') == str(len(payload)))
        etag = got.get('etag')
        check(http(url, headers={'If-None-Match': etag})[0] == 304)
        check(http(url.split('?')[0])[0] == 403)
        pairs = urllib.parse.parse_qsl(urllib.parse.urlsplit(url).query)
        tampered = url.split('?')[0] + '?' + urllib.parse.urlencode([(k, '0' * 64 if k == 'X-Amz-Signature' else v) for k, v in pairs])
        check(http(tampered)[0] == 403)
        passed.append(stage)

        if args.gui_origin:
            stage = 'cors_preflight'
            status, cors, _ = http(put_url, 'OPTIONS', headers={'Origin': args.gui_origin, 'Access-Control-Request-Method': 'PUT', 'Access-Control-Request-Headers': 'content-md5,content-type,if-none-match'})
            check(status in (200, 204) and cors.get('access-control-allow-origin') == args.gui_origin)
            check('PUT' in cors.get('access-control-allow-methods', ''))
            allowed_headers = {x.strip().lower() for x in cors.get('access-control-allow-headers', '').split(',')}
            check({'content-md5', 'content-type', 'if-none-match'} <= allowed_headers)
            status, denied, _ = http(put_url, 'OPTIONS', headers={'Origin': 'https://unlisted.example.test', 'Access-Control-Request-Method': 'PUT'})
            check(denied.get('access-control-allow-origin') is None)
            check(http(args.storage + '/minio/admin/v3/info')[0] == 404)
            check(http(args.storage + '/different-bucket/object')[0] == 404)
            passed.append(stage)

        stage = 'image_processing'
        results = {}
        for value, size in [(LFIT_256, (256, 192)), (FILL_64, (64, 64))]:
            url = signed(key, process(value))
            status, got, encoded = http(url, headers=origin_headers)
            check(status == 200 and got.get('content-type') == 'image/webp')
            check(got.get('x-image-cache') == 'MISS' and got.get('etag') == etag)
            with Image.open(io.BytesIO(encoded)) as decoded:
                decoded.load(); check(decoded.size == size)
            # 换一个签名再取：网关向上游做条件 GET，304 后复用编码结果。
            time.sleep(1.1)
            status, cached, again = http(signed(key, process(value)), headers=origin_headers)
            check(status == 200 and cached.get('x-image-cache') == 'HIT' and again == encoded)
            if args.gui_origin:
                check('x-image-cache' in cached.get('access-control-expose-headers', '').lower())
            results[value] = len(encoded)
        # 自查口径：同一对象两条处理串长度不同即网关生效。
        check(results[LFIT_256] != results[FILL_64])
        passed.append(stage)

        stage = 'processing_authorization'
        # 签名后再追加处理串：与 OSS 一样验签失败；网关不缓存放行。
        appended = signed(key) + '&' + process(LFIT_256)
        check(http(appended)[0] == 403)
        check(http(url.split('?')[0] + '?' + process(LFIT_256))[0] == 403)
        passed.append(stage)

        stage = 'oss_errors'
        status, got, body = http(signed(key, process('image/resize,w_abc')))
        check(status == 400 and got.get('x-oss-ec') == '0040-00000312' and b'<Code>InvalidArgument</Code>' in body)
        passed.append(stage)

        if public_key:
            stage = 'anonymous_read'
            client.put_object(Bucket=args.bucket, Key=public_key, Body=payload, ContentType='image/png')
            public = f'{args.storage}/{args.bucket}/{urllib.parse.quote(public_key)}'
            status, _, data = http(public)
            check(status == 200 and data == payload)
            status, got, encoded = http(public + '?' + process(LFIT_256))
            check(status == 200 and got.get('content-type') == 'image/webp' and got.get('x-image-cache') in ('MISS', 'HIT'))
            with Image.open(io.BytesIO(encoded)) as decoded:
                decoded.load(); check(decoded.size == (256, 192))
            passed.append(stage)
    except Exception as error:
        # 异常文本可能包含签名 URL，只输出固定阶段和异常类型。
        print(json.dumps({'status': 'failed', 'stage': stage, 'error_type': type(error).__name__}))
        raise SystemExit(1) from None
    finally:
        if cleanup:
            try:
                for own_key in own_keys:
                    client.delete_object(Bucket=args.bucket, Key=own_key)
                for own_prefix in {prefix, (args.public_prefix or '') + prefix}:
                    check(client.list_objects_v2(Bucket=args.bucket, Prefix=own_prefix).get('KeyCount', 0) == 0)
            except Exception:
                print(json.dumps({'status': 'cleanup_failed', 'test_prefix': prefix}))
                raise SystemExit(1) from None
    print(json.dumps({'status': 'passed', 'checks': passed, 'cleanup': 'passed', 'duration_seconds': round(time.monotonic() - started, 2)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--storage', type=storage_origin, required=True, help='网关（对象存储公开入口）的 origin')
    parser.add_argument('--bucket', required=True)
    parser.add_argument('--ca-file', type=Path, help='企业 CA 公共证书；仍严格验证证书链和 IP/主机名')
    parser.add_argument('--gui-origin', help='给出时核对前置 Nginx 的 CORS')
    parser.add_argument('--direct', help='可选：上游 MinIO 的 origin，核对网关透传与直连逐字节一致')
    parser.add_argument('--public-prefix', help='可选：已配匿名只读策略的对象前缀（如 public/），核对匿名读取与加工')
    parser.add_argument('--access-key-file', type=Path, required=True)
    parser.add_argument('--secret-key-file', type=Path, required=True)
    args = parser.parse_args()
    try:
        run(args)
    except KeyboardInterrupt:
        print('{"status":"cancelled"}'); raise SystemExit(130) from None
    except Exception as error:
        print(json.dumps({'status': 'failed', 'stage': 'setup', 'error_type': type(error).__name__})); raise SystemExit(1) from None


if __name__ == '__main__':
    main()
