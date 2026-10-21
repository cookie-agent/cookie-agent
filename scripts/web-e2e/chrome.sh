# Finds the Chrome-family browser and the chromedriver the browser suites
# drive (sourced by scripts/test-web-page.sh, scripts/test-wasm-client.sh,
# and scripts/web-screenshots.sh). `find_chrome` exports:
#
#   CHROME        the browser: $CHROME if set, else the first of
#                 google-chrome, google-chrome-stable, chromium, and
#                 chromium-browser on PATH;
#   CHROMEDRIVER  its driver: $CHROMEDRIVER if set, else chromedriver in
#                 $CHROMEWEBDRIVER (as GitHub's Ubuntu runners have it) or on
#                 PATH when its major version is the browser's, else the
#                 Chrome for Testing chromedriver of the browser's version,
#                 downloaded once into ~/.cache/cookie-agent/chromedriver.
#
# A snap's browser sees a private /tmp, where the suites keep their files,
# so it gets a warning.

find_chrome() {
    local name
    if [[ -z "${CHROME:-}" ]]; then
        for name in google-chrome google-chrome-stable chromium chromium-browser; do
            CHROME="$(command -v "$name" || true)"
            [[ -n "$CHROME" ]] && break
        done
    fi
    if [[ -z "${CHROME:-}" ]]; then
        echo "no Chrome or Chromium found; install one or set CHROME" >&2
        return 1
    fi
    if [[ "$(readlink -f "$CHROME")" == /snap/* || "$CHROME" == /snap/* ]]; then
        echo "warning: $CHROME is a snap, which sees a private /tmp; the suites may fail to hand it files there" >&2
    fi
    local version
    version="$("$CHROME" --version | grep -oE '[0-9]+(\.[0-9]+){3}' | head -n 1)"
    if [[ -z "$version" ]]; then
        echo "could not tell the version of $CHROME" >&2
        return 1
    fi
    export CHROME
    if [[ -z "${CHROMEDRIVER:-}" ]]; then
        local candidate
        for candidate in "${CHROMEWEBDRIVER:+$CHROMEWEBDRIVER/chromedriver}" "$(command -v chromedriver || true)"; do
            if [[ -n "$candidate" && -x "$candidate" ]] &&
                [[ "$("$candidate" --version | grep -oE '[0-9]+' | head -n 1)" == "${version%%.*}" ]]; then
                CHROMEDRIVER="$candidate"
                break
            fi
        done
    fi
    if [[ -z "${CHROMEDRIVER:-}" ]]; then
        CHROMEDRIVER="$(download_chromedriver "$version")" || return 1
    fi
    export CHROMEDRIVER
    echo "browser: $("$CHROME" --version) ($CHROME); driver: $CHROMEDRIVER" >&2
}

# Prints the path of the Chrome for Testing chromedriver for browser version
# $1 (that version, else the newest of its build), downloading it once.
download_chromedriver() {
    local version="$1" cache="${XDG_CACHE_HOME:-$HOME/.cache}/cookie-agent/chromedriver"
    local platform
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) platform=linux64 ;;
        Linux-aarch64) platform=linux-arm64 ;;
        Darwin-arm64) platform=mac-arm64 ;;
        Darwin-x86_64) platform=mac-x64 ;;
        *)
            echo "no Chrome for Testing chromedriver for $(uname -sm); set CHROMEDRIVER" >&2
            return 1
            ;;
    esac
    if [[ -x "$cache/$version/chromedriver" ]]; then
        echo "$cache/$version/chromedriver"
        return
    fi
    echo "downloading the chromedriver for Chrome $version into $cache/$version" >&2
    mkdir -p "$cache"
    local staging
    staging="$(mktemp -d "$cache/.download.XXXXXX")"
    python3 -I - "$version" "$platform" "$staging" <<'PY' || { rm -rf "$staging"; return 1; }
import io, json, os, sys, urllib.request, zipfile

version, platform, staging = sys.argv[1:]
index = "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json"
with urllib.request.urlopen(index, timeout=60) as response:
    versions = json.load(response)["versions"]
build = version.rsplit(".", 1)[0]
key = lambda v: [int(part) for part in v["version"].split(".")]
matches = [v for v in versions if v["version"] == version] or sorted(
    (v for v in versions if v["version"].rsplit(".", 1)[0] == build), key=key)[-1:]
downloads = [d for v in matches for d in v["downloads"].get("chromedriver", []) if d["platform"] == platform]
if not downloads:
    sys.exit("Chrome for Testing has no chromedriver for Chrome %s on %s; set CHROMEDRIVER" % (version, platform))
with urllib.request.urlopen(downloads[0]["url"], timeout=300) as response:
    archive = zipfile.ZipFile(io.BytesIO(response.read()))
member = next(name for name in archive.namelist() if name.endswith("/chromedriver"))
with open(os.path.join(staging, "chromedriver"), "wb") as file:
    file.write(archive.read(member))
os.chmod(os.path.join(staging, "chromedriver"), 0o755)
PY
    rm -rf "${cache:?}/$version"
    mv "$staging" "$cache/$version"
    echo "$cache/$version/chromedriver"
}

# The browser's command-line switches: headless, and without the sandbox
# only on CI, whose runners cannot give Chrome its user namespaces.
chrome_args_json() {
    if [[ "${CI:-}" == "true" ]]; then
        echo '["--headless=new", "--no-sandbox", "--disable-dev-shm-usage"]'
    else
        echo '["--headless=new"]'
    fi
}
