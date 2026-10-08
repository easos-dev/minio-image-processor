#!/usr/bin/env python3
"""仅生成目标服务器部署文件；不联网、不运行 Docker、不修改 EasOS。

生成目录必须为空；凭据用独占创建和 0600 权限落盘，不回显、不覆盖。
"""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
from urllib.parse import urlsplit
from xml.sax.saxutils import escape


def domain(value):
    if len(value) > 253 or value != value.lower() or len(value.split('.')) < 2:
        raise ValueError('域名必须是小写完整域名')
    if not all(re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', x) for x in value.split('.')):
        raise ValueError('域名不能含协议、端口或配置字符')
    try:
        ipaddress.ip_address(value)
    except ValueError:
        return value
    raise ValueError('HTTPS 自动证书配置需要域名')


def origin(value):
    parsed = urlsplit(value)
    if parsed.scheme not in ('http', 'https') or not parsed.hostname or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment:
        raise ValueError('GUI 来源须为无路径和凭据的 HTTP(S) origin')
    if any(c.isspace() or c in '\\"\'<>$`' for c in value):
        raise ValueError('GUI 来源含不允许的字符')
    _ = parsed.port
    return value


def write(path, data, mode=0o644):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(fd, 'w') as output:
        output.write(data)
    path.chmod(mode)


def prepare(base, storage, processor, bucket, origins, minio_secrets, network):
    storage, processor = domain(storage), domain(processor)
    if storage == processor:
        raise ValueError('存储和处理域名必须不同')
    if not re.fullmatch(r'[a-z][a-z0-9-]{1,48}[a-z0-9]', bucket):
        raise ValueError('桶名须为 3–50 位小写字母、数字和短横线')
    origins = list(dict.fromkeys(origin(x) for x in origins))
    if not 1 <= len(origins) <= 32:
        raise ValueError('须指定 1–32 个 GUI 来源')
    if not re.fullmatch(r'/[a-zA-Z0-9_./-]+', minio_secrets) or '..' in Path(minio_secrets).parts:
        raise ValueError('MinIO 秘密目录须为绝对路径，不能含 shell 字符')
    if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}', network):
        raise ValueError('网络名称无效')
    base = Path(base)
    for name in ('generated', 'secrets', '.env'):
        if (base / name).exists():
            raise ValueError('部署文件已存在；拒绝覆盖现有配置或凭据')
    base.mkdir(parents=True, exist_ok=True)
    generated, secret_dir = base / 'generated', base / 'secrets'
    generated.mkdir(mode=0o755)
    secret_dir.mkdir(mode=0o700)
    write(secret_dir / 'access_key', 'EASOS' + secrets.token_hex(8).upper() + '\n', 0o600)
    write(secret_dir / 'secret_key', secrets.token_hex(32) + '\n', 0o600)
    # 配置没有秘密；允许容器内既有 nobody 身份读取。
    write(generated / 'config.toml', f'''listen = "0.0.0.0:8080"
allowed_origins = {json.dumps(origins)}
max_input_bytes = 104857600
max_input_pixels = 24000000
max_concurrent = 2
fetch_timeout_seconds = 20
[[sources]]
origin = "https://{storage}"
upstream = "http://minio:9000"
path_prefix = "/{bucket}/"
''')
    # 此固定 MinIO 版本的桶 CORS 是空实现。公开入口由网关精确控制 CORS，
    # 不依赖全局默认通配符，也不改动现有 MinIO 的运行配置。
    cors_match = '\n        '.join(f'header Origin "{x}"' for x in origins)
    # Caddy 默认错误日志也可能带请求 URI，屏蔽 HTTP 请求日志类别；保留 TLS/启动日志。
    write(generated / 'Caddyfile', f'''{{
    admin off
    log {{
        exclude http.log.access http.log.error
    }}
}}
{storage} {{
    @origin {{
        {cors_match}
    }}
    @preflight {{
        method OPTIONS
        {cors_match}
        header Access-Control-Request-Method GET
        header Access-Control-Request-Method HEAD
        header Access-Control-Request-Method PUT
    }}
    @bucket path /{bucket} /{bucket}/*
    route {{
      header {{
        +Vary Origin
        >Cache-Control no-store
      }}
      header @origin {{
        >Access-Control-Allow-Origin "{{http.request.header.Origin}}"
        >Access-Control-Allow-Methods "GET, HEAD, PUT"
        >Access-Control-Allow-Headers "content-type, content-md5, if-none-match"
        >Access-Control-Expose-Headers "ETag, Content-Type"
        >Access-Control-Max-Age "300"
      }}
      respond @preflight "" 204
      handle @bucket {{
        reverse_proxy minio:9000 {{
            header_down -Access-Control-Allow-Origin
            header_down -Access-Control-Allow-Credentials
            header_down -Access-Control-Allow-Methods
            header_down -Access-Control-Allow-Headers
            header_down -Access-Control-Expose-Headers
            header_down -Access-Control-Max-Age
        }}
      }}
      handle {{
        respond 404
      }}
    }}
}}
{processor} {{
    reverse_proxy image-processor:8080
    header Referrer-Policy no-referrer
}}
''')
    allowed = ''.join(f'<AllowedOrigin>{escape(x)}</AllowedOrigin>' for x in origins)
    # 仅用于支持桶 CORS 的标准 S3；当前 MinIO 使用上面的网关配置。
    write(generated / 'cors.xml', f'''<CORSConfiguration><CORSRule>{allowed}
<AllowedMethod>GET</AllowedMethod><AllowedMethod>HEAD</AllowedMethod><AllowedMethod>PUT</AllowedMethod>
<AllowedHeader>content-type</AllowedHeader><AllowedHeader>content-md5</AllowedHeader><AllowedHeader>if-none-match</AllowedHeader>
<ExposeHeader>ETag</ExposeHeader><ExposeHeader>Content-Type</ExposeHeader><MaxAgeSeconds>300</MaxAgeSeconds>
</CORSRule></CORSConfiguration>
''')
    policy = {'Version': '2012-10-17', 'Statement': [
        {'Effect': 'Allow', 'Action': ['s3:GetBucketLocation', 's3:ListBucket'], 'Resource': [f'arn:aws:s3:::{bucket}']},
        {'Effect': 'Allow', 'Action': ['s3:GetObject', 's3:PutObject', 's3:DeleteObject'], 'Resource': [f'arn:aws:s3:::{bucket}/*']},
    ]}
    write(generated / 'policy.json', json.dumps(policy, indent=2) + '\n')
    write(base / '.env', f'BUCKET={bucket}\nMINIO_SECRETS_DIR={minio_secrets}\nMINIO_NETWORK={network}\n', 0o600)
    write(generated / 'easos-bucket.example.toml', f'''# 手工加入 File 配置或通过配置页面录入。不要覆盖整个现有 config.toml。
# 此示例无凭据。只用于独立开发实例新桶；既有记录仍引用原桶。
[[buckets]]
key = "minio_preview"
name = "MinIO Preview"
provider = "minio"
bucket = "{bucket}"
region = "us-east-1"
path_style = true
access_key_id = "REPLACE_FROM_secrets/access_key"
access_key_secret = "REPLACE_FROM_secrets/secret_key"
acceleration_excluded_regions = []
server = {{ https = true, host = "{storage}" }}
upload = {{ https = true, host = "{storage}" }}
accelerated_upload = {{ https = true, host = "" }}
download = {{ https = true, host = "{storage}" }}
image_processor = {{ https = true, host = "{processor}" }}
''')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--storage-domain', required=True)
    parser.add_argument('--image-domain', required=True)
    parser.add_argument('--bucket', default='easos-preview')
    parser.add_argument('--gui-origin', action='append', required=True)
    parser.add_argument('--minio-secrets-dir', required=True)
    parser.add_argument('--minio-network', default='image-processor-dev')
    parser.add_argument('--output', type=Path, default=Path(__file__).resolve().parents[1] / 'deploy/https')
    args = parser.parse_args()
    try:
        prepare(args.output, args.storage_domain, args.image_domain, args.bucket, args.gui_origin, args.minio_secrets_dir, args.minio_network)
    except (ValueError, OSError):
        parser.exit(1, '生成失败：请检查参数、目录权限和现有配置；没有启动任何服务。\n')
    print('配置已生成，凭据未输出。部署步骤见 deploy/https/README.md。')

if __name__ == '__main__':
    main()
