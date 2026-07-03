#!/usr/bin/env bash
# Phase 20 — dev toolbox: the CT-104 dev-template app set, ALL via apt (no flatpak/snap):
# dev/CLI tools, Docker, cloud CLIs, build libs, fonts, themes, browsers, Cursor, VS Code,
# Celluloid/ffmpeg, Extension Manager, ONLYOFFICE, plus HMCL / Mission Center / Monaspace /
# adw-gtk3 from their upstream releases, and the system-wide GNOME dconf defaults.
#
# BEST-EFFORT by design: these are genuinely optional apps, so a transient network/apt
# failure WARNs (via `apti` / the `mc_install`+`mona_install` functions) rather than sinking
# the build. The load-bearing base desktop is already in place from phase 10.
set -euo pipefail
. /setup/lib.sh

log "dev toolbox: third-party apt repos (docker/chrome/gh/cursor/mozilla/azure/gcloud/stripe)"
. /etc/os-release; CODENAME="${VERSION_CODENAME:-resolute}"
apt-get install -y -qq ca-certificates curl gnupg >/dev/null 2>&1 || warn "repo prereqs"
install -d -m0755 /etc/apt/keyrings

curl -fsSL https://download.docker.com/linux/ubuntu/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/docker.gpg || warn "docker key"
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $CODENAME stable" > /etc/apt/sources.list.d/docker.list

curl -fsSL https://dl.google.com/linux/linux_signing_key.pub | gpg --dearmor -o /etc/apt/keyrings/google-chrome.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/google-chrome.gpg || warn "chrome key"
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/google-chrome.gpg] http://dl.google.com/linux/chrome/deb/ stable main" > /etc/apt/sources.list.d/google-chrome.list

curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/githubcli-archive-keyring.gpg || warn "gh key"
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list

# Cursor: its repo key is gated behind the download portal, so pin Anysphere's public
# repo-signing key inline (fpr 380F F4BC DC34 A4BD 92A3 5653 42A1 772E 62E4 92D6). If
# Anysphere ever rotates it, `apt update` will flag the cursor repo — refresh here.
base64 -d > /usr/share/keyrings/anysphere.gpg <<'ANYSPHERE_KEY'
mQINBGhv/tgBEAC24VCTfKi5NSVaUAuSaIERf2EC5PCyOQz7WOh/UwyuG/1RB2r8/SYtipV+fD2b+xdu7WGPqrSHrKNNO1A9j6TtqbLVDDweJU2keHOqfIaamxrcyfCw3LMF9elIsmdkbZBukezWM32YBrG5MOwfCmG782sN79jYIPYckGZehh8Q6uIlZAzMTR7Qr6mlRR9cRZOF1gY1hRVCXQc1P3SH+ncX1abo/w3idRjxW3l0tqzjLcovWXD1xQdgt5odrpHlUkXRxxr7ukkPu2yJ2tL0KJydLtRDFf7k6ipYoCQv6hrFziHBHqfAEAMymr4YH96GhlbeP/zTSeUn8Y9Blz18q8sJJ2AKoAwpxWTYDIk7D3GDxHQYkcWIuh3MNJd3nulrptCOXgLBPqAF9/N1PW6UyX2XZmcFf0MQYC++IO0FwgcRw968L4LvgIGJdSCA5umcadDPoCQNcdobTur0WtzrsZ8letGoZ18FAhfeWfMWfljHDPbG0LSImKthaiAwXggi76sQyo374azY/ZfjepxRG3U7iEcesopqeo9p8l/8R7aZEk3zUbVt45yhp7XN8YtDrFvAPZfcIuoQTkeDZEub9Cch+fbeqdNk+LAyUbVzX/cFWBvRWC4ajsI/rD4IgeZInV39uG5ngpiwdb755xmZFiZSD1riGUYFYMfFfI1d80EOtQARAQABtCVBbnlzcGhlcmUgSW5jIDxzZWN1cml0eUBhbnlzcGhlcmUuY28+iQJRBBMBCAA7FiEEOA/0vNw0pL2So1ZTQqF3LmLkktYFAmhv/tgCGwMFCwkIBwICIgIGFQoJCAsCBBYCAwECHgcCF4AACgkQQqF3LmLkktZXUw//fAEm1Vo8uQ1E/4lNToEPM24olQp6If49+HSwFLCB5HhsGFmed6Zx1L+iNDJ8eW8niuepIqSRTX8G/+0z487hP29moLTE85g/YNsgWfkptbps3vgxlStotfgXZIKI71/m7FItBiA/tMS2ZkL1UwCSUQWE1YJgYJ8Gm4IbvoqYNwHv+8i0wJi3/G6lphHMxQp6XuO4HVlIk0dteQPaeszFK7jf74udRVTpxu+ffM0x/NFw08qYPsmBQJ9Of4/dhRfAYI9ZQOAFnIhujykOs7QBnq49JlzF3pYG/ZnvXwpUzRQgga+ro+5bXoQ1DZrNH+zl4EXtiXKpowUoYOZpDSELRVPGUW4vsyi+n34M5jMnxglYQJGB/ZTW95al7c84WZANripx2szeIxKukDcln7y0Qd7jpfGIC7xAjTzwVK8JzsDPisP9KPfua/zifr972QMK/4xlwjRRS6yRyM7Z2QZVdtzpUdPsVgbnXJkb6IBQSJDXKN7LQeB5Wi+4Cg9hddAG6sPu5wIcig67qFN/GEaeu6P4SuQqgBhmtf0x26Y0MDBbJtQ4adHSr90F8Fn8si6/Hb5xjSSOTg9QsPbAbBpmXjblLLRmdhUt0JCbIrBn2+jPKL+bT7aLkXiyI/k6kNC3AbI+YYwYgIDqSpqNHbdu+t9IOHK033IS4qoybKtiKbY=
ANYSPHERE_KEY
chmod a+r /usr/share/keyrings/anysphere.gpg
cat > /etc/apt/sources.list.d/cursor.sources <<'SRC'
Types: deb
URIs: https://downloads.cursor.com/aptrepo
Suites: stable
Components: main
Architectures: amd64,arm64
Signed-By: /usr/share/keyrings/anysphere.gpg
SRC

# Firefox from Mozilla's apt repo (pinned over the snap-transitional). Skip if the base
# image already ships a packages.mozilla.org source — else apt warns "configured twice".
if ! grep -rqs packages.mozilla.org /etc/apt/sources.list.d/ 2>/dev/null; then
  curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg -o /etc/apt/keyrings/packages.mozilla.org.asc 2>/dev/null && chmod a+r /etc/apt/keyrings/packages.mozilla.org.asc || warn "mozilla key"
  echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" > /etc/apt/sources.list.d/mozilla.list
  printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla
fi

curl -fsSL https://packages.microsoft.com/keys/microsoft.asc | gpg --dearmor -o /etc/apt/keyrings/microsoft.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/microsoft.gpg || warn "msft key"
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/microsoft.gpg] https://packages.microsoft.com/repos/azure-cli/ noble main" > /etc/apt/sources.list.d/azure-cli.list
# VS Code — same microsoft.gpg keyring imported just above; the `code` repo.
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/microsoft.gpg] https://packages.microsoft.com/repos/code stable main" > /etc/apt/sources.list.d/vscode.list

curl -fsSL https://packages.cloud.google.com/apt/doc/apt-key.gpg | gpg --dearmor -o /etc/apt/keyrings/cloud.google.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/cloud.google.gpg || warn "gcloud key"
echo "deb [signed-by=/etc/apt/keyrings/cloud.google.gpg] https://packages.cloud.google.com/apt cloud-sdk main" > /etc/apt/sources.list.d/google-cloud-sdk.list

curl -fsSL https://packages.stripe.dev/api/security/keypair/stripe-cli-gpg/public | gpg --dearmor -o /etc/apt/keyrings/stripe.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/stripe.gpg || warn "stripe key"
echo "deb [signed-by=/etc/apt/keyrings/stripe.gpg] https://packages.stripe.dev/stripe-cli-debian-local stable main" > /etc/apt/sources.list.d/stripe.list

# ngrok — its own keyring (dedicated, matching the per-repo keyring pattern above). No auth
# token is baked here: NGROK_AUTHTOKEN is a per-clone preset env var (the agent reads it
# natively), set through the Presets UI, consistent with the no-env-settings invariant.
curl -fsSL https://ngrok-agent.s3.amazonaws.com/ngrok.asc | gpg --dearmor -o /etc/apt/keyrings/ngrok.gpg 2>/dev/null && chmod a+r /etc/apt/keyrings/ngrok.gpg || warn "ngrok key"
echo "deb [signed-by=/etc/apt/keyrings/ngrok.gpg] https://ngrok-agent.s3.amazonaws.com buster main" > /etc/apt/sources.list.d/ngrok.list

# ONLYOFFICE Desktop Editors — official repo (replaces the Flathub build). GPG-KEY file is
# ASCII-armored; handle the binary case too just in case. "squeeze" is ONLYOFFICE's fixed
# repo suite, not a Debian release.
if curl -fsSL https://download.onlyoffice.com/GPG-KEY-ONLYOFFICE -o /tmp/oo.key 2>/dev/null; then
  if grep -q "BEGIN PGP" /tmp/oo.key; then gpg --dearmor < /tmp/oo.key > /etc/apt/keyrings/onlyoffice.gpg || warn "onlyoffice key"; else cp /tmp/oo.key /etc/apt/keyrings/onlyoffice.gpg; fi
  chmod a+r /etc/apt/keyrings/onlyoffice.gpg; rm -f /tmp/oo.key
  echo "deb [signed-by=/etc/apt/keyrings/onlyoffice.gpg] https://download.onlyoffice.com/repo/debian squeeze main" > /etc/apt/sources.list.d/onlyoffice.list
else
  warn "onlyoffice key fetch failed"
fi

apt-get update -qq || warn "apt update after adding toolbox repos"

log "dev toolbox: install (grouped so one bad package doesn't sink the rest)"
apti fish ripgrep micro tmux just gh xdotool build-essential clang libclang-dev default-jdk
apti docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin fuse-overlayfs
apti azure-cli google-cloud-cli stripe ngrok
apti libsodium23 libpq-dev libcairo2 libcairo2-dev
apti fonts-noto-cjk fonts-noto-color-emoji papirus-icon-theme
apti celluloid ffmpeg firefox google-chrome-stable cursor code
# Former Flathub apps now available via apt: Extension Manager (Ubuntu universe) + ONLYOFFICE.
apti gnome-shell-extension-manager onlyoffice-desktopeditors

log "dev toolbox: HMCL (latest .deb from its GitHub release)"
# `|| true` inside the substitution (mirrors the NVM_TAG line in 30-user.sh): under
# `set -euo pipefail`, an unreachable/rate-limited GitHub API makes `curl` fail and `grep`
# find nothing (exit 1), which would otherwise abort this best-effort optional-app step.
HMCL_URL="$(curl -fsSL https://api.github.com/repos/HMCL-dev/HMCL/releases/latest 2>/dev/null | grep -oE 'https://[^"]+/HMCL-[0-9.]+\.deb' | head -1 || true)"
if [ -n "${HMCL_URL:-}" ] && curl -fsSL "$HMCL_URL" -o /tmp/hmcl.deb 2>/dev/null; then
  apti /tmp/hmcl.deb && log "HMCL installed ($(basename "$HMCL_URL"))"; rm -f /tmp/hmcl.deb
else
  warn "could not fetch HMCL release"
fi

# Mission Center (system monitor) — no apt/deb upstream, only Flatpak + AppImage. Pull the
# latest x86_64 AppImage, --appimage-extract it (no FUSE needed), install the raw tree under
# /opt, and wire up a PATH wrapper + desktop entry (Exec rewritten to the wrapper). Defined
# as a function so `set -e` is suppressed across the block (run as `mc_install && … || …`),
# keeping this optional app from ever aborting the build.
mc_install(){
  local url d
  url="$(curl -fsSL 'https://gitlab.com/api/v4/projects/mission-center-devs%2Fmission-center/releases' 2>/dev/null | grep -oE 'https://[^"]+x86_64\.AppImage' | head -1)"
  [ -n "$url" ] || return 1
  curl -fsSL "$url" -o /tmp/mc.AppImage || return 1
  chmod +x /tmp/mc.AppImage
  ( cd /tmp && rm -rf squashfs-root && /tmp/mc.AppImage --appimage-extract >/dev/null 2>&1 ) || true
  [ -d /tmp/squashfs-root ] || return 1
  rm -rf /opt/mission-center; mv /tmp/squashfs-root /opt/mission-center; chown -R root:root /opt/mission-center
  printf '#!/bin/sh\nexec /opt/mission-center/AppRun "$@"\n' > /usr/local/bin/mission-center; chmod 755 /usr/local/bin/mission-center
  [ -d /opt/mission-center/usr/share/icons ] && cp -rn /opt/mission-center/usr/share/icons/* /usr/share/icons/ 2>/dev/null || true
  d="$(ls /opt/mission-center/usr/share/applications/*.desktop 2>/dev/null | head -1)"; [ -n "$d" ] || d="$(ls /opt/mission-center/*.desktop 2>/dev/null | head -1)"
  [ -n "$d" ] && sed -E 's#^Exec=.*#Exec=/usr/local/bin/mission-center#; s#^TryExec=.*#TryExec=/usr/local/bin/mission-center#' "$d" > /usr/share/applications/io.missioncenter.MissionCenter.desktop
  update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
  gtk-update-icon-cache -f /usr/share/icons/hicolor >/dev/null 2>&1 || true
  rm -f /tmp/mc.AppImage
}
log "dev toolbox: Mission Center (AppImage → extract → /opt + desktop entry)"
mc_install && log "Mission Center installed" || warn "Mission Center install skipped"

# Monaspace fonts (githubnext/monaspace) — full set: static (family "Monaspace Neon"),
# frozen ("Monaspace Neon Frozen", texture-healing baked in — used as the default mono), and
# variable ("Monaspace Neon Var"). frozen+variable are TTF, static is OTF — copy both.
command -v unzip >/dev/null 2>&1 || apt-get install -y -qq unzip >/dev/null 2>&1 || warn "unzip unavailable; Monaspace may be skipped"
mona_install(){
  local json url v
  json="$(curl -fsSL https://api.github.com/repos/githubnext/monaspace/releases/latest 2>/dev/null)"
  [ -n "$json" ] || return 1
  rm -rf /tmp/mona; mkdir -p /tmp/mona /usr/share/fonts/monaspace
  for v in static frozen variable; do
    url="$(echo "$json" | grep -oE "https://[^\"]+monaspace-$v-[^\"]+\.zip" | head -1)"
    [ -n "$url" ] || continue
    curl -fsSL "$url" -o "/tmp/mona/$v.zip" && unzip -oq "/tmp/mona/$v.zip" -d "/tmp/mona/$v" || true
  done
  find /tmp/mona -type f \( -iname '*.otf' -o -iname '*.ttf' \) -exec cp -f {} /usr/share/fonts/monaspace/ \;
  fc-cache -f >/dev/null 2>&1 || true
  rm -rf /tmp/mona
  [ -n "$(find /usr/share/fonts/monaspace -type f -name '*.ttf' 2>/dev/null | head -1)" ]
}
log "dev toolbox: Monaspace fonts (full: static + frozen + variable)"
mona_install && log "Monaspace installed ($(find /usr/share/fonts/monaspace -type f 2>/dev/null | wc -l) files)" || warn "Monaspace install skipped"

# adw-gtk3 (https://github.com/lassekongo83/adw-gtk3) — a GTK3 theme matching libadwaita's
# look, so legacy GTK3 apps stop clashing with the GTK4/libadwaita apps that already look
# native. Pinned release tarball (NOT `main`); the sha256 below was independently downloaded
# + hashed (matches the digest GitHub's own release-assets API reports for this file).
# UNLIKE the best-effort apps above (HMCL / Mission Center / Monaspace), this is
# LOAD-BEARING: it becomes the system default theme below, so a theme mismatch would be
# visible product surface. No `apti`/`|| warn` here — a failed download, checksum mismatch,
# or extract trips `set -e` and FAILS THE BUILD.
ADW_GTK3_VERSION=v6.5
ADW_GTK3_URL="https://github.com/lassekongo83/adw-gtk3/releases/download/${ADW_GTK3_VERSION}/adw-gtk3${ADW_GTK3_VERSION}.tar.xz"
ADW_GTK3_SHA256=a81780fadfc432be0fc3d89c4ebb41aa28e4f032d42c36f9789c57dd10cfa41c
log "adw-gtk3 ${ADW_GTK3_VERSION} (pinned + sha256-verified, load-bearing)"
curl -fsSL "$ADW_GTK3_URL" -o /tmp/adw-gtk3.tar.xz
echo "${ADW_GTK3_SHA256}  /tmp/adw-gtk3.tar.xz" | sha256sum -c -
install -d -m0755 /usr/share/themes
# Filter to the two expected top-level dirs: also doubles as a structure assertion — if the
# release ever stops shipping either, `tar` exits non-zero and (via set -e) fails the build.
tar -xJf /tmp/adw-gtk3.tar.xz -C /usr/share/themes/ adw-gtk3 adw-gtk3-dark
rm -f /tmp/adw-gtk3.tar.xz
log "adw-gtk3 installed: $(ls -d /usr/share/themes/adw-gtk3 /usr/share/themes/adw-gtk3-dark | wc -l)/2 dirs present"

# GNOME desktop defaults, system-wide via dconf (session-independent → every clone gets them
# on first boot): adw-gtk3 as the GTK theme, Papirus icons, Monaspace Neon Frozen 11 as the
# default monospace font, and all three window buttons (minimize/maximize/close). Users can
# still override per-session.
log "GNOME desktop defaults: adw-gtk3 + Papirus icons + Monaspace Neon Frozen mono + 3 window buttons"
install -d /etc/dconf/profile /etc/dconf/db/local.d
printf 'user-db:user\nsystem-db:local\n' > /etc/dconf/profile/user
cat > /etc/dconf/db/local.d/00-rmng-desktop <<'DCONF'
[org/gnome/desktop/interface]
gtk-theme='adw-gtk3'
icon-theme='Papirus'
monospace-font-name='Monaspace Neon Frozen 11'

[org/gnome/desktop/wm/preferences]
button-layout='appmenu:minimize,maximize,close'
DCONF
dconf update 2>/dev/null || warn "dconf update failed"

# Apt lists deliberately stay (dropped once, in the Dockerfile's tail cleanup) — same
# reasoning as phase 10.
