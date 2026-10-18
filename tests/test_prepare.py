"""验证部署输入边界、最小权限和秘密文件保护，不连接服务器。"""
import importlib.util
import json
from pathlib import Path
import tempfile
import tomllib
import unittest
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location('prepare', Path(__file__).resolve().parents[1] / 'scripts/prepare.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class PrepareTests(unittest.TestCase):
    def test_host_and_origin_reject_injection(self):
        for value in ['example.test {', 'https://example.test', 'EXAMPLE.test', 'example.test\nother', 'example.test:9000']:
            with self.assertRaises(ValueError): module.host(value)
        for value in ['*', 'https://user:secret@example.test', 'https://example.test/path', 'https://example.test?x=y']:
            with self.assertRaises(ValueError): module.origin(value)

    def test_generated_configuration_secrets_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            args = (base, '33.79.0.35', 'easos-preview', ['http://127.0.0.1:18080'], '/opt/image-processor-dev/secrets', 'image-processor-dev')
            module.prepare(*args)
            conf = tomllib.loads((base/'generated/config.toml').read_text())
            # 网关配置与示例同结构：只有监听、可信上游与资源预算。
            example = tomllib.loads((Path(__file__).resolve().parents[1]/'config.example.toml').read_text())
            self.assertEqual(set(conf), set(example))
            self.assertEqual(conf['upstream'], 'http://minio:9000')
            self.assertEqual(conf['max_concurrent'], 0)
            nginx = (base/'generated/nginx.conf').read_text()
            # 只有 443 一个入口：专用桶交给图片处理网关，不再单开 8443，也不直连 MinIO。
            self.assertIn('listen 443 ssl;', nginx)
            self.assertNotIn('8443', nginx)
            self.assertNotIn('proxy_pass http://minio', nginx)
            self.assertEqual(nginx.count('proxy_pass http://image-processor:8080;'), 2)
            self.assertIn('location ^~ /easos-preview/', nginx)
            self.assertIn('X-Image-Cache', nginx)
            # 已发布对象的 Cache-Control 必须原样到达浏览器，不能被入口改写成 no-store。
            self.assertNotIn('no-store', nginx)
            self.assertNotIn('proxy_hide_header Cache-Control', nginx)
            for header in ['range', 'content-md5', 'cache-control', 'content-disposition', 'if-none-match', 'x-amz-checksum-crc64nvme']:
                self.assertIn(header, nginx.split('Access-Control-Allow-Headers')[1].split('\n')[0])
            bucket = tomllib.loads((base/'generated/easos-bucket.example.toml').read_text())['buckets'][0]
            self.assertEqual(bucket['download']['host'], '33.79.0.35')
            self.assertEqual(bucket['server']['host'], '33.79.0.35')
            # EasOS 已删掉外部图片处理地址与加速字段，示例里不能再出现。
            for removed in ['image_processor', 'accelerated_upload', 'acceleration_excluded_regions']:
                self.assertNotIn(removed, bucket)
            cors = ET.parse(base/'generated/cors.xml')
            self.assertIn('if-none-match', [x.text for x in cors.findall('.//AllowedHeader')])
            policy = json.loads((base/'generated/policy.json').read_text())
            self.assertNotIn('*', [r for s in policy['Statement'] for r in s['Resource']])
            self.assertEqual((base/'secrets').stat().st_mode & 0o777, 0o700)
            for name in ['access_key', 'secret_key']:
                secret = base/'secrets'/name
                self.assertEqual(secret.stat().st_mode & 0o777, 0o600)
                for file in (base/'generated').iterdir():
                    self.assertNotIn(secret.read_text().strip(), file.read_text())
            old = (base/'secrets/secret_key').read_bytes()
            with self.assertRaises(ValueError): module.prepare(*args)
            self.assertEqual((base/'secrets/secret_key').read_bytes(), old)

if __name__ == '__main__': unittest.main()
