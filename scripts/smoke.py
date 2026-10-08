#!/usr/bin/env python3
"""目标服务器部署后的一次性 HTTPS/S3/图片处理联调，不操作已有对象。

密钥只从 0600 文件读取；URL、凭据和上游错误正文不进入输出。
所有网络请求单次执行，finally 清理本次随机对象。此检查不替代浏览器与 EasOS 实际路径。
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


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def https_origin(value):
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment:
        raise argparse.ArgumentTypeError('需要无路径和凭据的 HTTPS origin')
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
    # 禁用环境代理、自动重定向；只使用调用者明确给出的 TLS 入口。
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(), urllib.request.HTTPSHandler(context=TLS_CONTEXT))
    request = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        response = opener.open(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        payload = response.read(20 * 1024 * 1024 + 1)
        if len(payload) > 20 * 1024 * 1024:
            raise ValueError('响应超过测试预算')
        return response.status, response.headers, payload


def check(condition):
    if not condition:
        raise AssertionError('检查结果不符')


def run(args):
    import boto3
    from botocore.config import Config
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
    client = boto3.client('s3', endpoint_url=args.storage, region_name='us-east-1',
        verify=str(args.ca_file) if args.ca_file else True,
        aws_access_key_id=secret(args.access_key_file), aws_secret_access_key=secret(args.secret_key_file),
        config=Config(signature_version='s3v4', s3={'addressing_style': 'path'},
            retries={'total_max_attempts': 1}, connect_timeout=5, read_timeout=20, proxies={}))
    image = Image.new('RGBA', (120,60))
    image.putdata([(x*2, y*4, (x+y)%256, 255) for y in range(60) for x in range(120)])
    output = io.BytesIO(); image.save(output, format='PNG'); payload = output.getvalue()
    prefix = 'image-processor-check-' + uuid.uuid4().hex + '/'
    key, bad_key = prefix + '测试 image.png', prefix + 'bad-md5.png'
    cleanup = False
    passed = []
    try:
        stage = 'presigned_conditional_upload'
        md5 = base64.b64encode(hashlib.md5(payload).digest()).decode()
        params = dict(Bucket=args.bucket, Key=key, ContentType='image/png', ContentMD5=md5, IfNoneMatch='*')
        put_url = client.generate_presigned_url('put_object', Params=params, ExpiresIn=300)
        headers = {'content-type':'image/png', 'content-md5':md5, 'if-none-match':'*', 'origin':args.gui_origin}
        # 发出写请求前即标记需清理，超时也可能已写入；只删除随机自有键。
        cleanup = True
        status, response_headers, _ = http(put_url, 'PUT', payload, headers)
        check(status == 200)
        check(response_headers.get('access-control-allow-origin') == args.gui_origin)
        check(http(put_url, 'PUT', payload, headers)[0] == 412)
        passed.append(stage)
        stage = 'md5_validation'
        bad_md5 = base64.b64encode(bytes(16)).decode()
        params.update(Key=bad_key, ContentMD5=bad_md5)
        bad_url = client.generate_presigned_url('put_object', Params=params, ExpiresIn=300)
        check(http(bad_url, 'PUT', payload, {**headers,'content-md5':bad_md5})[0] == 400)
        passed.append(stage)
        stage = 'head_get_integrity'
        meta = client.head_object(Bucket=args.bucket, Key=key)
        check(meta['ContentLength'] == len(payload) and meta['ContentType'] == 'image/png')
        signed = client.generate_presigned_url('get_object', Params={'Bucket':args.bucket,'Key':key}, ExpiresIn=60)
        status, got_headers, data = http(signed, headers={'Origin':args.gui_origin})
        check(status == 200 and data == payload)
        check(got_headers.get('access-control-allow-origin') == args.gui_origin)
        check(http(signed.split('?')[0])[0] == 403)
        pairs = urllib.parse.parse_qsl(urllib.parse.urlsplit(signed).query)
        tampered = signed.split('?')[0]+'?'+urllib.parse.urlencode([(k, '0'*64 if k=='X-Amz-Signature' else v) for k,v in pairs])
        check(http(tampered)[0] == 403)
        passed.append(stage)
        stage = 'cors_preflight'
        status, cors, _ = http(put_url, 'OPTIONS', headers={'Origin':args.gui_origin,'Access-Control-Request-Method':'PUT','Access-Control-Request-Headers':'content-md5,content-type,if-none-match'})
        check(status in (200,204) and cors.get('access-control-allow-origin') == args.gui_origin)
        check('PUT' in cors.get('access-control-allow-methods',''))
        allowed_headers = {x.strip().lower() for x in cors.get('access-control-allow-headers','').split(',')}
        check({'content-md5','content-type','if-none-match'} <= allowed_headers)
        status, denied, _ = http(put_url, 'OPTIONS', headers={'Origin':'https://unlisted.example.test','Access-Control-Request-Method':'PUT'})
        check(denied.get('access-control-allow-origin') is None)
        check(http(args.storage+'/minio/admin/v3/info')[0] == 404)
        check(http(args.storage+'/different-bucket/object')[0] == 404)
        passed.append(stage)
        stage = 'image_conversion'
        for fit, size in [('contain',(40,20)), ('cover',(40,40)), ('fill',(40,40))]:
            for fmt, mime in [('jpg','image/jpeg'),('png','image/png'),('webp','image/webp')]:
                signed = client.generate_presigned_url('get_object', Params={'Bucket':args.bucket,'Key':key}, ExpiresIn=60)
                query = urllib.parse.urlencode(dict(url=signed,width=40,height=40,quality=85,fit=fit,format=fmt))
                status, got_headers, encoded = http(args.processor+'/?'+query, headers={'Origin':args.gui_origin})
                check(status == 200 and got_headers.get('content-type') == mime)
                check(got_headers.get('access-control-allow-origin') == args.gui_origin)
                check(got_headers.get('cache-control') == 'no-store')
                with Image.open(io.BytesIO(encoded)) as decoded:
                    decoded.load(); check(decoded.size == size)
        passed.append(stage)
        stage = 'processor_source_rejection'
        query = urllib.parse.urlencode(dict(url='http://169.254.169.254/latest/meta-data/'))
        check(http(args.processor+'/?'+query)[0] == 403)
        passed.append(stage)
    except Exception as error:
        # 异常文本可能包含签名 URL，只输出固定阶段和异常类型。
        print(json.dumps({'status':'failed','stage':stage,'error_type':type(error).__name__}))
        raise SystemExit(1) from None
    finally:
        if cleanup:
            try:
                for own_key in (key,bad_key):
                    client.delete_object(Bucket=args.bucket, Key=own_key)
                leftovers = client.list_objects_v2(Bucket=args.bucket, Prefix=prefix).get('KeyCount',0)
                check(leftovers == 0)
            except Exception:
                print(json.dumps({'status':'cleanup_failed','test_prefix':prefix}))
                raise SystemExit(1) from None
    print(json.dumps({'status':'passed','checks':passed,'cleanup':'passed','duration_seconds':round(time.monotonic()-started,2)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--storage', type=https_origin, required=True)
    parser.add_argument('--processor', type=https_origin, required=True)
    parser.add_argument('--bucket', required=True)
    parser.add_argument('--ca-file', type=Path, help='企业 CA 公共证书；仍严格验证证书链和 IP/主机名')
    parser.add_argument('--gui-origin', required=True)
    parser.add_argument('--access-key-file', type=Path, required=True)
    parser.add_argument('--secret-key-file', type=Path, required=True)
    args = parser.parse_args()
    try:
        run(args)
    except KeyboardInterrupt:
        print('{"status":"cancelled"}'); raise SystemExit(130) from None
    except Exception as error:
        print(json.dumps({'status':'failed','stage':'setup','error_type':type(error).__name__})); raise SystemExit(1) from None

if __name__ == '__main__':
    main()
