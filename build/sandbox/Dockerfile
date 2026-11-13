ARG CUBE_BASE_IMAGE=ghcr.io/tencentcloud/cubesandbox-base:2026.16
FROM node:22-bookworm-slim@sha256:e9f565d32859f0cbfb52125eafe8be39390f24dac302dc3cdf566e8ba3ab21ee AS mcp-build
WORKDIR /opt/agentdrive-mcp
COPY mcp/package.json mcp/package-lock.json ./
RUN npm ci --ignore-scripts --omit=dev --no-audit --no-fund

FROM ${CUBE_BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
# archive.ubuntu.com is often unusable from China and makes the GNOME
# install look hung for tens of minutes. Override with --build-arg APT_MIRROR=
# to use upstream, or another mirror.
ARG APT_MIRROR=http://mirrors.tencent.com/ubuntu

ENV DISPLAY=:0 \
    SCREEN_WIDTH=1280 \
    SCREEN_HEIGHT=720 \
    SCREEN_DEPTH=24 \
    VNC_PORT=5900 \
    NOVNC_PORT=6080 \
    DESKTOP_USER=user \
    LANG=C.UTF-8 \
    XDG_SESSION_TYPE=x11 \
    XDG_CURRENT_DESKTOP=ubuntu:GNOME \
    GNOME_SHELL_SESSION_MODE=ubuntu \
    LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=llvmpipe

RUN if [ -n "${APT_MIRROR}" ]; then \
        sed -i \
            -e "s|http://archive.ubuntu.com/ubuntu|${APT_MIRROR}|g" \
            -e "s|http://security.ubuntu.com/ubuntu|${APT_MIRROR}|g" \
            /etc/apt/sources.list; \
    fi \
    && apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=20 update \
    && apt-get -o Acquire::Retries=3 -y --no-install-recommends install \
        xserver-xorg-core \
        xserver-xorg-video-dummy \
        xserver-xorg-legacy \
        x11-xserver-utils \
        x11-utils \
        xdg-utils \
        xvfb \
        dbus-x11 \
        dbus-user-session \
        ubuntu-session \
        ubuntu-settings \
        ubuntu-wallpapers \
        gnome-shell \
        gnome-session \
        gnome-terminal \
        nautilus \
        gnome-shell-extension-ubuntu-dock \
        gnome-shell-extension-desktop-icons-ng \
        yaru-theme-gtk \
        yaru-theme-icon \
        yaru-theme-gnome-shell \
        fonts-ubuntu \
        libgl1-mesa-dri \
        mesa-utils \
        librsvg2-common \
        libglib2.0-bin \
        at-spi2-core \
        x11vnc \
        novnc \
        websockify \
        python3 \
        python3-tk \
        python3-xlib \
        gnome-screenshot \
        wmctrl \
        ffmpeg \
        xclip \
        xdotool \
        gedit \
        epiphany-browser \
        fonts-dejavu-core \
        sudo \
        iproute2 \
        procps \
        e2fsprogs \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN id -u user >/dev/null 2>&1 || useradd -m -s /bin/bash user


COPY xorg.conf /etc/X11/xorg.conf
COPY Xwrapper.config /etc/X11/Xwrapper.config
COPY start-desktop.sh /usr/local/bin/start-desktop.sh
COPY novnc-index.html /usr/share/novnc/index.html
COPY 90-cube-ubuntu-desktop.gschema.override /usr/share/glib-2.0/schemas/90-cube-ubuntu-desktop.gschema.override

RUN glib-compile-schemas /usr/share/glib-2.0/schemas \
    && chmod 755 /usr/local/bin/start-desktop.sh



# Keep cubesandbox-base ENTRYPOINT (envd in the background).
CMD ["/usr/local/bin/start-desktop.sh"]

ARG COMPUTER_USE_VERSION=0.7.53
ADD https://github.com/agent-sh/computer-use-linux/releases/download/v${COMPUTER_USE_VERSION}/computer-use-linux-x86_64-unknown-linux-gnu /tmp/computer-use-linux
RUN echo "9105b0caefe67881bd9549a51c59252d2f1dfe4306d9cc71be0091cd3085d122  /tmp/computer-use-linux" | sha256sum -c - \
    && install -m 0755 /tmp/computer-use-linux /usr/local/bin/computer-use-linux \
    && rm /tmp/computer-use-linux

COPY guest-init.sh guest-helper.py desktop_environment.py desktop_mcp.py /usr/local/bin/
RUN chmod 755 /usr/local/bin/guest-init.sh /usr/local/bin/guest-helper.py

RUN apt-get -o Acquire::Retries=3 update \
    && apt-get -y --no-install-recommends install \
        fonts-noto-cjk fonts-noto-color-emoji \
        libnss3 libnspr4 libasound2 libgbm1 libcups2 libatk-bridge2.0-0 libxkbcommon0 \
    && rm -rf /var/lib/apt/lists/*

ARG CHROMIUM_ARCHIVE=https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/1743918/chrome-linux.zip
ADD ${CHROMIUM_ARCHIVE} /tmp/chromium.zip
COPY install-chromium.py chromium.json /tmp/chromium-install/
RUN python3 /tmp/chromium-install/install-chromium.py /tmp/chromium.zip /tmp/chromium-install/chromium.json /opt/chromium \
    && rm -rf /tmp/chromium.zip /tmp/chromium-install

COPY chromium /usr/local/bin/chromium
COPY chromium.desktop /usr/share/applications/chromium.desktop
COPY 64-agentdrive-fonts.conf /etc/fonts/conf.d/64-agentdrive-fonts.conf
RUN chmod 755 /usr/local/bin/chromium \
    && update-desktop-database /usr/share/applications \
    && fc-cache -f

COPY --from=mcp-build /usr/local/bin/node /usr/local/bin/node
COPY --from=mcp-build /opt/agentdrive-mcp /opt/agentdrive-mcp
COPY mcp/gateway.mjs mcp/providers.json mcp/playwright.json /opt/agentdrive-mcp/
COPY browser-session.py /usr/local/bin/browser-session.py
COPY template-manifest.py /usr/local/bin/template-manifest.py
RUN python3 /usr/local/bin/template-manifest.py "${COMPUTER_USE_VERSION}" > /opt/agentdrive-template.json
