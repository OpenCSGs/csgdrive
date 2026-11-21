"""Transfer an AgentDrive object with repository-scoped, renewable XNet credentials."""
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def run(request):
    import hf_xet

    opener = urllib.request.build_opener(NoRedirect())
    endpoint = None

    def refresh():
        nonlocal endpoint
        token_request = urllib.request.Request(
            request["token_url"],
            headers={"Authorization": "Bearer " + request["token"]},
        )
        with opener.open(token_request, timeout=30) as response:
            info = json.loads(response.read(64 * 1024))
        url = urllib.parse.urlsplit(info["casUrl"])
        allowed = url.scheme == "https" or (url.scheme == "http" and request["allow_http"])
        if not allowed or not url.hostname or url.username or url.password:
            raise ValueError("invalid CAS endpoint")
        if endpoint is not None and endpoint != info["casUrl"]:
            raise ValueError("CAS endpoint changed during transfer")
        endpoint = info["casUrl"]
        expiry = int(info["exp"])
        if not info["accessToken"] or expiry <= time.time():
            raise ValueError("expired token")
        return (info["accessToken"], expiry)

    credentials = refresh()
    if request["action"] == "check":
        return
    result = hf_xet.upload_files(
        [request["path"]], endpoint, credentials, refresh, None, "code"
    )
    if len(result) != 1 or result[0].file_size != request["size"]:
        raise ValueError("incomplete upload")


if __name__ == "__main__":
    try:
        run(json.load(sys.stdin))
    except (ImportError, AttributeError):
        sys.exit(10)
    except urllib.error.HTTPError as error:
        sys.exit(11 if error.code in (401, 403) else 12 if error.code == 404 else 13)
    except (ValueError, KeyError, TypeError):
        sys.exit(14)
    except Exception:
        sys.exit(13)
