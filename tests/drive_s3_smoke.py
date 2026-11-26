"""Exercise the path-style S3 endpoint against an isolated local AgentDrive directory."""
import argparse
import concurrent.futures
import json
import os
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def denied(op, *args, **kwargs):
    try:
        op(*args, **kwargs)
    except ClientError as error:
        assert error.response["Error"]["Code"] == "AccessDenied", error
    else:
        raise AssertionError("standby or retired session published a write")


def conditional_requests(s3, bucket):
    def rejected(status, action, **args):
        try:
            action(**args)
        except ClientError as exc:
            assert exc.response["ResponseMetadata"]["HTTPStatusCode"] == status, exc
        else:
            raise AssertionError(f"{action.__name__} accepted a false or unsupported condition")

    def contents(key):
        return s3.get_object(Bucket=bucket, Key=key)["Body"].read()

    key = "conditional-source"
    before = s3.put_object(Bucket=bucket, Key=key, Body=b"before")["ETag"]
    current = s3.put_object(Bucket=bucket, Key=key, Body=b"after")["ETag"]
    rejected(501, s3.delete_object, Bucket=bucket, Key=key, VersionId="old-version")
    rejected(412, s3.delete_object, Bucket=bucket, Key=key, IfMatch=before)
    for fields, code in [({"ETag": before}, "PreconditionFailed"), ({"Size": 999}, "PreconditionFailed"),
                         ({"VersionId": "old-version"}, "NotImplemented")]:
        result = s3.delete_objects(Bucket=bucket, Delete={"Objects":[{"Key":key, **fields}]})
        assert result["Errors"][0]["Code"] == code, result
        assert contents(key) == b"after"
    target = "conditional-target"
    s3.put_object(Bucket=bucket, Key=target, Body=b"preserved")
    copy = dict(Bucket=bucket, Key=target, CopySource={"Bucket":bucket, "Key":key})
    rejected(412, s3.copy_object, **copy, CopySourceIfNoneMatch=current)
    mtime = s3.head_object(Bucket=bucket, Key=key)["LastModified"]
    rejected(412, s3.copy_object, **copy, CopySourceIfModifiedSince=mtime)
    assert contents(target) == b"preserved"
    upload = s3.create_multipart_upload(Bucket=bucket, Key=target)["UploadId"]
    rejected(412, s3.upload_part_copy, **copy, UploadId=upload, PartNumber=1, CopySourceIfNoneMatch=current)
    part = s3.upload_part_copy(**copy, UploadId=upload, PartNumber=1,
                               CopySourceIfMatch=current, CopySourceRange="bytes=1-3")
    s3.complete_multipart_upload(Bucket=bucket, Key=target, UploadId=upload,
        MultipartUpload={"Parts":[{"PartNumber":1, "ETag":part["CopyPartResult"]["ETag"]}]})
    assert contents(target) == b"fte"
    s3.copy_object(**copy, CopySourceIfNoneMatch=before)
    assert contents(target) == b"after"
    result = s3.delete_objects(Bucket=bucket, Delete={"Objects":[{"Key":key, "ETag":current},{"Key":target}]})
    assert len(result["Deleted"]) == 2 and not result.get("Errors")


def ranged_read(s3, bucket, server_pid):
    key = "ranged-file"
    size = 8 * 1024 * 1024
    payload = bytes(range(256)) * (size // 256)
    s3.put_object(Bucket=bucket, Key=key, Body=payload)
    # Warm the local content cache before measuring gateway reads.
    assert s3.get_object(Bucket=bucket, Key=key)["Body"].read() == payload
    counters = Path(f"/proc/{server_pid}/io")

    def read_bytes():
        return int(dict(line.split(": ") for line in counters.read_text().splitlines())["rchar"])

    before = read_bytes() if counters.is_file() else None
    offset = size - 8193
    reply = s3.get_object(Bucket=bucket, Key=key, Range=f"bytes={offset}-{offset + 4095}")
    assert reply["ResponseMetadata"]["HTTPStatusCode"] == 206
    assert reply["ContentLength"] == 4096 and reply["Body"].read() == payload[offset:offset+4096]
    if before is not None:
        consumed = read_bytes() - before
        assert consumed < size // 2, f"4 KiB GET read {consumed} bytes from the OS"
    s3.delete_object(Bucket=bucket, Key=key)


def run(binary, daemon):
    s3_port, admin_port = port(), port()
    while admin_port == s3_port:
        admin_port = port()
    ui_port = port()
    while ui_port in (s3_port, admin_port):
        ui_port = port()
    ui = f"http://127.0.0.1:{ui_port}"
    endpoint = f"http://127.0.0.1:{s3_port}"
    admin = f"http://127.0.0.1:{admin_port}/mounts"
    s3 = boto3.client("s3", endpoint_url=endpoint, region_name="us-east-1",
                      aws_access_key_id="drive-demo", aws_secret_access_key="drive-demo-secret",
                      config=Config(s3={"addressing_style": "path"}, retries={"max_attempts": 0}))

    def manage(action, **params):
        req = urllib.request.Request(admin, json.dumps(dict(action=action, **params)).encode(),
                                     {"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.load(response)

    def start(directory):
        if daemon:
            config = Path(directory) / "agentdrive.json"
            if not config.exists():
                subprocess.run([binary, "init", "--directory", directory, "--listen", f"127.0.0.1:{ui_port}"], check=True)
            process = subprocess.Popen([binary, "serve", "--config", config, "--drive-s3-listen", f"127.0.0.1:{s3_port}",
                                        "--drive-s3-admin-port", str(admin_port)],
                                       env={**os.environ, "DRIVE_S3_ACCESS_KEY": "drive-demo", "DRIVE_S3_SECRET_KEY": "drive-demo-secret"})
        else:
            process = subprocess.Popen([binary, "--data-dir", directory, "--listen", f"127.0.0.1:{s3_port}",
                                        "--admin-port", str(admin_port)])
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"server exited with {process.returncode}")
                try:
                    with socket.create_connection(("127.0.0.1", admin_port), timeout=.3):
                        return process
                except OSError:
                    time.sleep(.05)
            raise TimeoutError("drive-s3 did not start")
        except BaseException:
            process.terminate()
            process.wait()
            raise

    def stop(process):
        process.terminate()
        process.wait(timeout=10)

    a, b = "sandbox-alpha", "sandbox-beta"
    with tempfile.TemporaryDirectory() as directory:
        process = start(directory)
        try:
            for bucket in (a, b):
                manage("bind", bucket=bucket, workspace="shared", branch="work")
            assert manage("status", bucket=a)["branch_id"] == manage("status", bucket=b)["branch_id"]
            manage("acquire", bucket=a)
            key, data = "目录/file 1+%.txt", b"shared bytes"
            s3.put_object(Bucket=a, Key=key, Body=data)
            assert s3.get_object(Bucket=b, Key=key, Range="bytes=1-3")["Body"].read() == data[1:4]
            assert s3.head_object(Bucket=b, Key=key)["ContentLength"] == len(data)
            try:
                s3.get_object(Bucket=b, Key=key, IfMatch='"stale-version"')
            except ClientError as error:
                assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 412
            else:
                raise AssertionError("If-Match did not reject a stale range reader")
            s3.copy_object(Bucket=a, Key="copy", CopySource={"Bucket": a, "Key": key})
            assert s3.get_object(Bucket=b, Key="copy")["Body"].read() == data
            page = s3.get_paginator("list_objects_v2").paginate(Bucket=b, Delimiter="/", PaginationConfig={"PageSize": 1})
            items = {(k, item[k]) for result in page for group, k in (("Contents", "Key"), ("CommonPrefixes", "Prefix")) for item in result.get(group, [])}
            assert items == {("Key", "copy"), ("Prefix", "目录/")}, items
            conditional_requests(s3, a)
            ranged_read(s3, a, process.pid)
            denied(s3.put_object, Bucket=b, Key="standby", Body=b"forbidden")
            upload = s3.create_multipart_upload(Bucket=a, Key="late")["UploadId"]
            part = s3.upload_part(Bucket=a, Key="late", UploadId=upload, PartNumber=1, Body=data)
            successful = s3.create_multipart_upload(Bucket=a, Key="completed")["UploadId"]
            chunk = b"x" * (5 * 1024 * 1024)
            pieces = []
            for n, value in enumerate((chunk, data), start=1):
                response = s3.upload_part(Bucket=a, Key="completed", UploadId=successful, PartNumber=n, Body=value)
                pieces.append({"PartNumber": n, "ETag": response["ETag"]})
            s3.complete_multipart_upload(Bucket=a, Key="completed", UploadId=successful, MultipartUpload={"Parts": pieces})
            assert s3.get_object(Bucket=b, Key="completed", Range=f"bytes={len(chunk)}-")["Body"].read() == data
            manage("handoff", **{"from": a, "to": b})
            denied(s3.complete_multipart_upload, Bucket=a, Key="late", UploadId=upload,
                   MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]})
            denied(s3.delete_object, Bucket=a, Key="copy")
            s3.put_object(Bucket=b, Key="copy", Body=b"new writer")
            assert s3.get_object(Bucket=a, Key="copy")["Body"].read() == b"new writer"
            pair = ("editor-east", "editor-west")
            session = manage("bind", bucket=pair[0], workspace="shared", branch="editable", write_mode="serialized")
            session = manage("bind_ids", bucket=pair[1], workspace_id=session["workspace_id"],
                             branch_id=session["branch_id"], write_mode="serialized")
            assert session["writable"] and session["write_mode"] == "serialized"
            s3.put_object(Bucket=pair[0], Key="note.txt", Body=b"east")
            s3.copy_object(Bucket=pair[1], Key="copy", CopySource={"Bucket":pair[1], "Key":"note.txt"})
            assert s3.get_object(Bucket=pair[0], Key="copy")["Body"].read() == b"east"
            s3.delete_object(Bucket=pair[0], Key="copy")
            upload = s3.create_multipart_upload(Bucket=pair[1], Key="note.txt")["UploadId"]
            part = s3.upload_part(Bucket=pair[1], Key="note.txt", UploadId=upload, PartNumber=1, Body=b"west")
            s3.complete_multipart_upload(Bucket=pair[1], Key="note.txt", UploadId=upload,
                                         MultipartUpload={"Parts":[{"PartNumber":1, "ETag":part["ETag"]}]})
            assert s3.get_object(Bucket=pair[0], Key="note.txt")["Body"].read() == b"west"
            if daemon:
                with urllib.request.urlopen(ui + "/api/workspaces") as response:
                    assert session["workspace_id"] in [w["id"] for w in json.load(response)]
                branch_api = f"{ui}/api/workspaces/{session['workspace_id']}/branches/{session['branch_id']}"
                with urllib.request.urlopen(branch_api + "/s3-mounts") as response:
                    assert [m["bucket"] for m in json.load(response)] == list(pair)
                with urllib.request.urlopen(branch_api + "/files?path=/") as response:
                    listing = json.load(response)
                    assert [entry["path"] for entry in listing["entries"]] == ["/note.txt"]
                query = urllib.parse.urlencode({"path":"/note.txt", "version":listing["version"], "offset":0, "size":100})
                with urllib.request.urlopen(branch_api + "/file?" + query) as response:
                    assert response.read() == b"west"
            other = "sandbox-other"
            manage("bind", bucket=other, workspace="shared", branch="main")
            assert not s3.list_objects_v2(Bucket=other).get("Contents")
            manage("bind", bucket="sandbox-competitor", workspace="shared", branch="main")

            def acquire(bucket):
                try:
                    manage("acquire", bucket=bucket)
                    return "writer"
                except urllib.error.HTTPError as e:
                    assert e.code in (403, 409), e
                    return "reader"

            with concurrent.futures.ThreadPoolExecutor(2) as executor:
                assert sorted(executor.map(acquire, (other, "sandbox-competitor"))) == ["reader", "writer"]
        finally:
            stop(process)
        process = start(directory)
        try:
            assert s3.get_object(Bucket=pair[1], Key="note.txt")["Body"].read() == b"west"
            status = manage("status", bucket=a)
            assert status["retired"] and status["holder"] == b
            denied(s3.put_object, Bucket=a, Key="stale", Body=b"stale")
            assert s3.get_object(Bucket=b, Key="copy")["Body"].read() == b"new writer"
            s3.delete_objects(Bucket=b, Delete={"Objects": [{"Key": "copy"}]})
            assert "copy" not in [o["Key"] for o in s3.list_objects_v2(Bucket=a).get("Contents", [])]
        finally:
            stop(process)
    print("PASS  shared branch, both write modes, listing, range, copy, stale multipart, competing writers, restart")
    if daemon:
        print("PASS  dashboard branch listing, mount policies and cross-client file content")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--daemon", action="store_true", help="test agentdrived with its dashboard and S3 listeners")
    args = parser.parse_args()
    run(args.binary.resolve(), args.daemon)
