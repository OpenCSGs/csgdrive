FROM agentdrive-ubuntu:local
USER root
COPY .cache/csgclaw-release.tar.gz /tmp/csgclaw.tar.gz
RUN echo '80083df3224a5d9727c43ce86e580a998a5009f37f1d8446aee487eaf6ae9b97  /tmp/csgclaw.tar.gz' | sha256sum -c - \
    && tar -xzf /tmp/csgclaw.tar.gz -C /opt && rm /tmp/csgclaw.tar.gz \
    && ln -s /opt/csgclaw/bin/csgclaw /usr/local/bin/csgclaw
COPY guest-init.sh guest-helper.py browser-session.py desktop_environment.py desktop_mcp.py /usr/local/bin/
COPY assistant/assistant-mcp.py assistant/assistant-service.py /usr/local/bin/
COPY assistant/runtime.json /etc/agentdrive-runtime.json
COPY mcp/gateway.mjs mcp/providers.json mcp/playwright.json /opt/agentdrive-mcp/
RUN python3 -c 'import json,datetime; p="/opt/agentdrive-template.json"; d=json.load(open(p)); d["name"]="Ubuntu Desktop + CSGClaw"; d["built_at"]=datetime.datetime.now(datetime.timezone.utc).isoformat(); d["tools"].append("CSGClaw v0.10.0 (Web UI + Agent)"); json.dump(d,open(p,"w"),ensure_ascii=False,indent=2)'
