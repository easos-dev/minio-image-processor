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
            self.assertEqual(conf['sources'][0]['path_prefix'], '/easos-preview/')
            self.assertEqual(conf['allowed_origins'], ['http://127.0.0.1:18080'])
            example = tomllib.loads((Path(__file__).resolve().parents[1]/'config.example.toml').read_text())
            self.assertEqual(conf['max_input_pixels'], example['max_input_pixels'])
            bucket = tomllib.loads((base/'generated/easos-bucket.example.toml').read_text())['buckets'][0]
            self.assertEqual(bucket['image_processor']['host'], '33.79.0.35:8443')
            self.assertEqual(bucket['download']['host'], '33.79.0.35')
            self.assertEqual(bucket['server']['host'], '33.79.0.35')
            self.assertIn('listen 443 ssl;', (base/'generated/nginx.conf').read_text())
            self.assertIn('X-Image-Cache', (base/'generated/nginx.conf').read_text())
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
