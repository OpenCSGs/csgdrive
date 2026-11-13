"""校验并安装模板使用的 Chromium 官方构建。"""
import hashlib
import json
from pathlib import Path
import shutil
import sys
import tempfile
import zipfile


def install(archive_path, manifest_path, destination):
    manifest = json.loads(Path(manifest_path).read_text())
    digest = hashlib.sha256()
    with open(archive_path, 'rb') as archive:
        for chunk in iter(lambda: archive.read(1024 * 1024), b''):
            digest.update(chunk)
    if digest.hexdigest() != manifest['sha256']:
        raise ValueError('Chromium SHA-256 校验失败')
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise FileExistsError(destination)
    with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
        with zipfile.ZipFile(archive_path) as archive:
            for item in archive.infolist():
                target = Path(temporary, item.filename).resolve()
                if not target.is_relative_to(Path(temporary).resolve()):
                    raise ValueError('Chromium 压缩包路径无效')
            archive.extractall(temporary)
        source = Path(temporary, 'chrome-linux')
        for name in ('chrome', 'chrome_crashpad_handler'):
            (source / name).chmod(0o755)
        (source / 'chrome_sandbox').chmod(0o4755)
        shutil.move(str(source), destination)
    (destination / 'agentdrive-build.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    install(*sys.argv[1:])
