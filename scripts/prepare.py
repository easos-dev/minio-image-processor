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


def host(value):
    try:
        if isinstance(ipaddress.ip_address(value), ipaddress.IPv4Address):
            return value
    except ValueError:
        pass
    if len(value) > 253 or value != value.lower() or len(value.split('.')) < 2 or not all(re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', x) for x in value.split('.')):
        raise ValueError('需要 IPv4 或小写域名，不含协议、端口或配置字符')
    return value


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


def prepare(base, public_host, bucket, origins, minio_secrets, network):
    public_host = host(public_host)
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
    # 网关配置没有秘密；允许容器内既有 nobody 身份读取。网关把请求原样转给同网络 MinIO。
    write(generated / 'config.toml', '''listen = "0.0.0.0:8080"
upstream = "http://minio:9000"
max_concurrent = 0
fetch_timeout_seconds = 20
''')
    # 443 只开放专用桶：TLS 与浏览器 CORS 在 Nginx，之后一律交给图片处理网关；网关原样转给 MinIO，
    # 只在 GET 带 x-oss-process 时加工。保留原始 Host、路径和签名查询，S3 SigV4 才能通过。
    # 企业证书由运维单独安装，只读挂载；配置生成器不读取或生成私钥。
    cors_map = '\n        '.join(f'"{x}" "{x}";' for x in origins)
    proxy = """proxy_set_header Host $http_host;
            proxy_set_header Connection "";
            proxy_http_version 1.1;
            proxy_next_upstream off;
            proxy_request_buffering off;
            proxy_buffering off;
            proxy_connect_timeout 5s;
            proxy_read_timeout 60s;
            proxy_send_timeout 60s;
            proxy_hide_header Access-Control-Allow-Origin;
            proxy_hide_header Access-Control-Allow-Credentials;
            proxy_hide_header Access-Control-Allow-Methods;
            proxy_hide_header Access-Control-Allow-Headers;
            proxy_hide_header Access-Control-Expose-Headers;
            proxy_hide_header Access-Control-Max-Age;
            proxy_hide_header Vary;"""
    locations = '\n'.join(f'''        location {match} {{
            if ($request_method = OPTIONS) {{ return 204; }}
            {proxy}
            proxy_pass http://image-processor:8080;
        }}''' for match in (f'= /{bucket}', f'^~ /{bucket}/'))
    write(generated / 'nginx.conf', f'''# 不记录访问 URI；查询参数可能携带短期存储签名。
worker_processes auto;
pid /tmp/nginx.pid;
error_log /dev/null;
events {{ worker_connections 1024; }}
http {{
    access_log off;
    client_body_temp_path /tmp/client_temp;
    proxy_temp_path /tmp/proxy_temp;
    map $http_origin $allowed_origin {{
        default "";
        {cors_map}
    }}
    ssl_certificate /etc/nginx/tls/server.crt;
    ssl_certificate_key /etc/nginx/tls/server.key;
    ssl_protocols TLSv1.2 TLSv1.3;
    server_tokens off;
    # 上传大小由 EasOS 与 MinIO 自己限制（EasOS 非图片上限可配到 10 GiB），网关入口不再另设上限。
    client_max_body_size 0;
    large_client_header_buffers 4 32k;
    # 与 EasOS File 文档列出的上游 CORS 要求一致；MinIO 社区版桶 CORS 是空实现，由这里统一给出。
    add_header Access-Control-Allow-Origin $allowed_origin always;
    add_header Access-Control-Allow-Methods "GET, HEAD, PUT" always;
    add_header Access-Control-Allow-Headers "range, content-type, content-md5, cache-control, content-disposition, if-none-match, x-amz-checksum-crc64nvme" always;
    add_header Access-Control-Expose-Headers "Content-Range, Accept-Ranges, ETag, Content-Length, Content-Type, X-Request-Id, X-Image-Cache" always;
    add_header Access-Control-Max-Age "300" always;
    add_header Vary Origin always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    server {{
        listen 443 ssl;
        server_name {public_host};
{locations}
        location / {{ return 404; }}
    }}
}}
''')
    allowed = ''.join(f'<AllowedOrigin>{escape(x)}</AllowedOrigin>' for x in origins)
    # 仅用于支持桶 CORS 的标准 S3；当前 MinIO 使用上面的 Nginx 配置。
    write(generated / 'cors.xml', f'''<CORSConfiguration><CORSRule>{allowed}
<AllowedMethod>GET</AllowedMethod><AllowedMethod>HEAD</AllowedMethod><AllowedMethod>PUT</AllowedMethod>
<AllowedHeader>range</AllowedHeader><AllowedHeader>content-type</AllowedHeader><AllowedHeader>content-md5</AllowedHeader><AllowedHeader>cache-control</AllowedHeader><AllowedHeader>content-disposition</AllowedHeader><AllowedHeader>if-none-match</AllowedHeader><AllowedHeader>x-amz-checksum-crc64nvme</AllowedHeader>
<ExposeHeader>Content-Range</ExposeHeader><ExposeHeader>Accept-Ranges</ExposeHeader><ExposeHeader>ETag</ExposeHeader><ExposeHeader>Content-Length</ExposeHeader><ExposeHeader>X-Image-Cache</ExposeHeader><MaxAgeSeconds>300</MaxAgeSeconds>
</CORSRule></CORSConfiguration>
''')
    policy = {'Version': '2012-10-17', 'Statement': [
        {'Effect': 'Allow', 'Action': ['s3:GetBucketLocation', 's3:ListBucket'], 'Resource': [f'arn:aws:s3:::{bucket}']},
        {'Effect': 'Allow', 'Action': ['s3:GetObject', 's3:PutObject', 's3:DeleteObject'], 'Resource': [f'arn:aws:s3:::{bucket}/*']},
    ]}
    write(generated / 'policy.json', json.dumps(policy, indent=2) + '\n')
    write(base / '.env', f'BUCKET={bucket}\nMINIO_SECRETS_DIR={minio_secrets}\nMINIO_NETWORK={network}\n', 0o600)
    write(generated / 'easos-bucket.example.toml', f'''# 手工加入 File 配置或通过配置页面录入。不要覆盖整个现有 config.toml。
# 此示例无凭据。下载域名就是图片处理网关入口（MinIO 前置网关），没有单独的图片处理地址。
[[buckets]]
key = "minio_preview"
name = "MinIO Preview"
provider = "minio"
bucket = "{bucket}"
region = "us-east-1"
path_style = true
object_prefix = "easos"
access_key_id = "REPLACE_FROM_secrets/access_key"
access_key_secret = "REPLACE_FROM_secrets/secret_key"
server = {{ https = true, host = "{public_host}" }}
upload = {{ https = true, host = "{public_host}" }}
download = {{ https = true, host = "{public_host}" }}
''')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True, help='证书 SAN 中的 IPv4 或域名；443 是对象存储公开入口（经图片处理网关到 MinIO）')
    parser.add_argument('--bucket', default='easos-preview')
    parser.add_argument('--gui-origin', action='append', required=True)
    parser.add_argument('--minio-secrets-dir', required=True)
    parser.add_argument('--minio-network', default='image-processor-dev')
    parser.add_argument('--output', type=Path, default=Path(__file__).resolve().parents[1] / 'deploy/https')
    args = parser.parse_args()
    try:
        prepare(args.output, args.host, args.bucket, args.gui_origin, args.minio_secrets_dir, args.minio_network)
    except (ValueError, OSError):
        parser.exit(1, '生成失败：请检查参数、目录权限和现有配置；没有启动任何服务。\n')
    print('配置已生成，凭据未输出。部署步骤见 deploy/https/README.md。')

if __name__ == '__main__':
    main()
