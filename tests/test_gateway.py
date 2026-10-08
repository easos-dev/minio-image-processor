"""使用现有 Caddy 二进制和临时 HTTP 桩实测网关；无 MinIO、图片服务或持久部署。"""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('prepare', ROOT/'scripts/prepare.py')
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


@unittest.skipUnless(shutil.which('caddy'), '本机无 Caddy，仅跳过网关 HTTP 桩验收；服务器部署前仍须验收')
class GatewayTests(unittest.TestCase):
    def test_exact_cors_bucket_boundary_and_signed_host(self):
        seen = []
        class Upstream(BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append((self.path, self.headers['Host']))
                self.send_response(200)
                self.send_header('Access-Control-Allow-Origin','*')
                self.send_header('Access-Control-Allow-Credentials','true')
                self.end_headers(); self.wfile.write(b'fixture')
            def log_message(self, *_): pass
        upstream = ThreadingHTTPServer(('127.0.0.1',0), Upstream)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True); thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                base = Path(tmp)
                prepare.prepare(base, 'objects.example.test', 'images.example.test', 'easos-preview', ['http://127.0.0.1:18080','https://gui.example.test'], '/unused/secrets', 'fixture-network')
                result = subprocess.run(['caddy','adapt','--config',str(base/'generated/Caddyfile'),'--adapter','caddyfile'], capture_output=True, check=True)
                config = json.loads(result.stdout)
                # 只改测试监听、TLS 和桩地址；路由顺序、CORS、签名路径及 Host 保持生成结果。
                with socket.socket() as s:
                    s.bind(('127.0.0.1',0)); port = s.getsockname()[1]
                server = config['apps']['http']['servers']['srv0']
                server['listen'] = [f'127.0.0.1:{port}']
                server['automatic_https'] = {'disable':True}
                config['apps'].pop('tls',None)
                config['storage'] = {'module':'file_system','root':str(base/'caddy-data')}
                text = json.dumps(config).replace('minio:9000',f'127.0.0.1:{upstream.server_port}')
                (base/'test.json').write_text(text)
                ready = threading.Event()
                env = {**os.environ,'XDG_DATA_HOME':str(base/'data'),'XDG_CONFIG_HOME':str(base/'config')}
                proc = subprocess.Popen(['caddy','run','--config',str(base/'test.json')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,env=env)
                def logs():
                    for line in proc.stderr:
                        if b'serving initial configuration' in line: ready.set()
                log_thread = threading.Thread(target=logs,daemon=True); log_thread.start()
                try:
                    self.assertTrue(ready.wait(5),'Caddy 未就绪')
                    def request(path, origin, method='GET', extra=None):
                        conn = http.client.HTTPConnection('127.0.0.1',port,timeout=5)
                        try:
                            conn.request(method,path,headers={'Host':'objects.example.test','Origin':origin,**(extra or {})})
                            response = conn.getresponse(); body = response.read()
                            return response.status, dict((k.lower(),v) for k,v in response.getheaders()), body
                        finally: conn.close()
                    for origin in ['http://127.0.0.1:18080','https://gui.example.test']:
                        status, headers, body = request('/easos-preview/a%20b?X-Amz-Signature=fixture',origin)
                        self.assertEqual((status,body),(200,b'fixture'))
                        self.assertEqual(headers.get('access-control-allow-origin'),origin)
                        self.assertEqual(headers.get('cache-control'),'no-store')
                        self.assertNotIn('access-control-allow-credentials',headers)
                    status, headers, _ = request('/easos-preview/object','https://unlisted.example.test')
                    self.assertEqual(status,200)
                    self.assertNotIn('access-control-allow-origin',headers)
                    self.assertIn('Origin',headers.get('vary',''))
                    status, headers, _ = request('/easos-preview/object','http://127.0.0.1:18080','OPTIONS',{'Access-Control-Request-Method':'PUT','Access-Control-Request-Headers':'content-md5,content-type,if-none-match'})
                    self.assertEqual(status,204)
                    self.assertEqual(headers.get('access-control-allow-origin'),'http://127.0.0.1:18080')
                    self.assertIn('if-none-match',headers.get('access-control-allow-headers',''))
                    self.assertEqual(request('/minio/admin/v3/info','http://127.0.0.1:18080')[0],404)
                    self.assertEqual(request('/different-bucket/object','http://127.0.0.1:18080')[0],404)
                    self.assertEqual(len(seen),3)
                    self.assertEqual(seen[0],('/easos-preview/a%20b?X-Amz-Signature=fixture','objects.example.test'))
                finally:
                    proc.terminate()
                    try: proc.wait(timeout=5)
                    except subprocess.TimeoutExpired: proc.kill(); proc.wait()
                    log_thread.join(timeout=1); proc.stderr.close()
        finally:
            upstream.shutdown(); upstream.server_close(); thread.join(timeout=1)

if __name__ == '__main__': unittest.main()
