"""校验导入源码的完整性、许可证和 worker 的依赖来源。"""
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def main():
    vendor = ROOT / 'vendor/cube-hypervisor'
    source = json.loads((vendor / 'UPSTREAM.json').read_text())
    failures = []
    for name, expected in source['files_sha256'].items():
        path = vendor / name
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            failures.append(f'导入文件发生变化，需要更新来源记录：{name}')
    for name in ['LICENSE-APACHE', 'LICENSE-BSD-3-Clause', 'LICENSE-CUBESANDBOX',
                 'virtiofsd/LICENSE-APACHE', 'virtiofsd/LICENSE-BSD-3-Clause']:
        if name not in source['files_sha256']:
            failures.append(f'来源记录缺少许可证：{name}')
    worker = ROOT / 'runtime/sandbox-worker'
    metadata = json.loads(subprocess.check_output([
        'cargo', 'metadata', '--locked', '--offline', '--no-deps', '--format-version=1',
        '--manifest-path', str(worker / 'Cargo.toml'),
    ], text=True))
    package = next(p for p in metadata['packages'] if p['name'] == 'agentdrive-sandbox-worker')
    dependency = next(d for d in package['dependencies'] if d['name'] == 'cube-hypervisor')
    if dependency.get('path') != str(vendor) or dependency.get('source'):
        failures.append('worker 必须使用导入的 hypervisor 源码')
    for line in (worker / 'Cargo.lock').read_text().splitlines():
        if line.startswith('source = ') and 'cubesandbox' in line.lower():
            failures.append('依赖锁文件仍然引用远端 CubeSandbox')
    if failures:
        raise SystemExit('\n'.join(failures))
    print(f'已验证 {len(source["files_sha256"])} 个导入文件、许可证和 worker 依赖来源')


if __name__ == '__main__':
    main()
