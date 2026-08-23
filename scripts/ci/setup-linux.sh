#!/usr/bin/env bash
set -euo pipefail

readonly usage="usage: $0 [--list] {build|build-no-gstreamer|oracle|ecosystem|display|release}"

list_only=false
if [[ ${1:-} == "--list" ]]; then
    list_only=true
    shift
fi

readonly profile=${1:-}
if [[ -z $profile || $# -ne 1 ]]; then
    echo "$usage" >&2
    exit 2
fi

readonly -a build_packages=(
    build-essential
    git
    pkg-config
    cmake
    m4
    libssl-dev
    fontconfig
    fonts-noto-core
    libfontconfig1-dev
    libfreetype-dev
    libncurses-dev
    libglib2.0-dev
    libunwind-dev
    libxkbcommon-dev
    libxkbcommon-x11-dev
    libwayland-dev
    wayland-protocols
    libxcb1-dev
    libxrandr-dev
    libxinerama-dev
    libxi-dev
    libxcursor-dev
    mesa-vulkan-drivers
    libvulkan-dev
    libdbus-1-dev
    libsystemd-dev
    libsqlite3-dev
    libxml2-dev
    liblcms2-dev
    gnutls-bin
    libgnutls28-dev
    zlib1g-dev
)

readonly -a video_backend_packages=(
    libgstreamer1.0-dev
    libgstreamer-plugins-base1.0-dev
)

readonly -a gnu_runtime_packages=(
    libgtk-3-0t64
    libjansson4
    libharfbuzz0b
    libtree-sitter0
    libsqlite3-0
    libgnutls30t64
    libgdk-pixbuf-2.0-0
    libgif7
    libjpeg8
    libpng16-16t64
    libtiff6
    libxpm4
    libxft2
    libotf1
)

declare -a profile_packages=()
declare -a required_commands=()
requires_emacs=false
requires_libfaketime=false
requires_nerd_font=false
requires_gstreamer=true
case "$profile" in
    build)
        ;;
    build-no-gstreamer)
        requires_gstreamer=false
        ;;
    # GNU Emacs is NOT an apt package here: every GNU-vs-Neomacs comparison
    # runs against the pinned build downloaded by
    # .github/actions/download-test-assets (not an apt emacs-nox version skew).
    oracle)
        profile_packages=("${gnu_runtime_packages[@]}" libfaketime)
        requires_emacs=false
        requires_libfaketime=true
        ;;
    ecosystem)
        profile_packages=(
            "${gnu_runtime_packages[@]}"
            ca-certificates
            curl
            gnupg
            libfaketime
            xvfb
            xauth
            x11-utils
            xdotool
            imagemagick
            weston
        )
        required_commands=(gpg Xvfb xauth xdpyinfo xdotool import weston)
        requires_emacs=true
        requires_libfaketime=true
        requires_nerd_font=true
        ;;
    display)
        profile_packages=(ca-certificates curl)
        requires_nerd_font=true
        ;;
    release)
        profile_packages=(rpm binutils cpio file dpkg-dev)
        required_commands=(rpm objdump cpio file dpkg-shlibdeps)
        ;;
    *)
        echo "unknown profile: $profile" >&2
        echo "$usage" >&2
        exit 2
        ;;
esac

declare -a packages=("${build_packages[@]}" "${profile_packages[@]}")
if $requires_gstreamer; then
    packages+=("${video_backend_packages[@]}")
fi
if $list_only; then
    printf '%s\n' "${packages[@]}"
    exit 0
fi

if [[ $(uname -s) != "Linux" ]] || ! command -v apt-get >/dev/null 2>&1; then
    echo "setup-linux.sh requires an apt-based Linux runner" >&2
    exit 1
fi

sudo apt-get update
sudo apt-get install -y --no-install-recommends "${packages[@]}"

if $requires_nerd_font; then
    readonly nerd_font_url="https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.4.0/patched-fonts/JetBrainsMono/Ligatures/Regular/JetBrainsMonoNerdFont-Regular.ttf"
    readonly nerd_font_sha256="0ec29a68b539ece7078fc714cebff0c0accb2f4948f8f7963d9f5e86633b12d9"
    readonly nerd_font_tmp="$(mktemp)"
    readonly nerd_font_dir="/usr/local/share/fonts/neomacs"
    trap 'rm -f "$nerd_font_tmp"' EXIT

    curl \
        --fail \
        --location \
        --retry 5 \
        --retry-all-errors \
        --retry-delay 2 \
        --connect-timeout 20 \
        --output "$nerd_font_tmp" \
        "$nerd_font_url"
    printf '%s  %s\n' "$nerd_font_sha256" "$nerd_font_tmp" | sha256sum --check --status -
    sudo install -D -m 0644 "$nerd_font_tmp" \
        "$nerd_font_dir/JetBrainsMonoNerdFont-Regular.ttf"
    fc-cache -f "$nerd_font_dir"

    if ! fc-match --format='%{family}\n' 'JetBrainsMono Nerd Font:charset=f48a' |
        grep -Fq 'JetBrainsMono Nerd Font'; then
        echo "JetBrainsMono Nerd Font U+F48A coverage is unavailable" >&2
        exit 1
    fi
fi

# Fail at the environment seam instead of silently compiling out optional
# primitives and discovering the mismatch much later in an oracle test.
pkg-config --modversion lcms2
if $requires_gstreamer; then
    pkg-config --modversion gstreamer-1.0
fi

if $requires_emacs; then
    if [[ -z ${NEOMACS_MELPA_ORACLE_EMACS:-} ]]; then
        echo "setup-linux.sh $profile requires NEOMACS_MELPA_ORACLE_EMACS" >&2
        exit 1
    fi
    readonly gnu_emacs="$NEOMACS_MELPA_ORACLE_EMACS"
    readonly gnu_root="$(cd "$(dirname "$gnu_emacs")/.." && pwd)"
    env \
        "EMACSDATA=$gnu_root/etc" \
        "EMACSDOC=$gnu_root/etc" \
        "EMACSPATH=$gnu_root/lib-src" \
        "EMACSLOADPATH=$gnu_root/lisp" \
        "$gnu_emacs" --batch --quick --eval '(kill-emacs 0)'
fi
if $requires_libfaketime; then
    dpkg -L libfaketime | grep -q '/libfaketime\.so\.1$'
fi
for program in "${required_commands[@]}"; do
    command -v "$program" >/dev/null
done
