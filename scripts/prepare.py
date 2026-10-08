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
    storage, processor = public_host, public_host + ":8443"
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
    # 两个 TLS 端口分别承载 S3 与图片处理，保留 S3 Host、路径和签名查询。
    # 企业证书由运维单独安装，只读挂载；配置生成器不读取或生成私钥。
    cors_map = '\n        '.join(f'"{x}" "{x}";' for x in origins)
    proxy_common = """proxy_set_header Host $http_host;
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
            proxy_hide_header Cache-Control;"""
    def locations(upstream, matches):
        return '\n'.join(f'''        location {match} {{
            if ($request_method = OPTIONS) {{ return 204; }}
            {proxy_common}
            proxy_pass http://{upstream};
        }}''' for match in matches)
    storage_locations = locations('minio:9000', [f'= /{bucket}', f'^~ /{bucket}/'])
    image_locations = locations('image-processor:8080', ['= /', '= /healthz'])
    write(generated / 'nginx.conf', f'''# 不记录访问 URI；查询参数携带短期存储签名。
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
    client_max_body_size 100m;
    large_client_header_buffers 4 32k;
    add_header Access-Control-Allow-Origin $allowed_origin always;
    add_header Access-Control-Allow-Methods "GET, HEAD, PUT" always;
    add_header Access-Control-Allow-Headers "content-type, content-md5, if-none-match" always;
    add_header Access-Control-Expose-Headers "ETag, Content-Type, X-Request-Id" always;
    add_header Access-Control-Max-Age "300" always;
    add_header Vary Origin always;
    add_header Cache-Control no-store always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    server {{
        listen 443 ssl;
        server_name {public_host};
{storage_locations}
        location / {{ return 404; }}
    }}
    server {{
        listen 8443 ssl;
        server_name {public_host};
{image_locations}
        location / {{ return 404; }}
    }}
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
    parser.add_argument('--host', required=True, help='证书 SAN 中的 IPv4 或域名，443 为存储，8443 为图片处理')
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
