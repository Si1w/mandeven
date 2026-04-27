#!/usr/bin/env sh
#
# Installer for the `mandeven` binary.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Si1w/mandeven/main/scripts/install.sh | sh
#
# Environment overrides:
#   MANDEVEN_VERSION       Pin a specific tag (default: latest GitHub release).
#   MANDEVEN_INSTALL_DIR   Where to drop the binary (default: $HOME/.local/bin).
#   MANDEVEN_REPO          Override the GitHub slug (default: Si1w/mandeven).

set -eu

MANDEVEN_REPO="${MANDEVEN_REPO:-Si1w/mandeven}"
MANDEVEN_INSTALL_DIR="${MANDEVEN_INSTALL_DIR:-$HOME/.local/bin}"
MANDEVEN_VERSION="${MANDEVEN_VERSION:-}"

err() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

info() {
    printf 'install.sh: %s\n' "$*"
}

require() {
    command -v "$1" >/dev/null 2>&1 || err "missing required tool: $1"
}

require uname
require tar
require mkdir
require mktemp
require chmod

# Pick a downloader.
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    fetch_stdout() { wget -q -O - "$1"; }
else
    err "neither curl nor wget is installed"
fi

# Resolve target triple from `uname` output. Mirrors the matrix in
# .github/workflows/release.yml. Anything outside this list means we
# don't ship a prebuilt binary for the host — fall back to a clear
# error suggesting `cargo install` or a build from source.
detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    if [ "$os" != "Darwin" ]; then
        err "unsupported OS: $os (only macOS Apple Silicon is prebuilt; build from source via \`cargo build --release\`)"
    fi
    case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        *) err "unsupported macOS architecture: $arch (only Apple Silicon is prebuilt; build from source via \`cargo build --release\`)" ;;
    esac
}

# `latest` resolves to the most recent published release; explicit
# tags are passed through verbatim.
resolve_version() {
    if [ -n "$MANDEVEN_VERSION" ]; then
        echo "$MANDEVEN_VERSION"
        return
    fi
    api_url="https://api.github.com/repos/$MANDEVEN_REPO/releases/latest"
    tag="$(fetch_stdout "$api_url" | grep -E '"tag_name"' | head -n 1 | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [ -n "$tag" ] || err "could not resolve latest release from $api_url"
    echo "$tag"
}

verify_checksum() {
    archive="$1"
    checksum_file="$2"
    if command -v sha256sum >/dev/null 2>&1; then
        ( cd "$(dirname "$archive")" && sha256sum -c "$(basename "$checksum_file")" >/dev/null )
    elif command -v shasum >/dev/null 2>&1; then
        ( cd "$(dirname "$archive")" && shasum -a 256 -c "$(basename "$checksum_file")" >/dev/null )
    else
        info "neither sha256sum nor shasum found — skipping checksum verification"
        return 0
    fi
}

main() {
    target="$(detect_target)"
    tag="$(resolve_version)"
    version="${tag#v}"
    archive="mandeven-${version}-${target}.tar.gz"
    base="https://github.com/${MANDEVEN_REPO}/releases/download/${tag}"

    info "installing mandeven ${tag} for ${target}"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT INT HUP TERM

    fetch "${base}/${archive}" "${tmpdir}/${archive}"
    if fetch "${base}/${archive}.sha256" "${tmpdir}/${archive}.sha256" 2>/dev/null; then
        verify_checksum "${tmpdir}/${archive}" "${tmpdir}/${archive}.sha256"
    else
        info "no checksum published for ${archive} — proceeding without verification"
    fi

    tar -xzf "${tmpdir}/${archive}" -C "${tmpdir}"
    extracted_dir="${tmpdir}/mandeven-${version}-${target}"
    [ -x "${extracted_dir}/mandeven" ] || err "archive did not contain mandeven binary"

    mkdir -p "$MANDEVEN_INSTALL_DIR"
    install_path="${MANDEVEN_INSTALL_DIR}/mandeven"
    cp "${extracted_dir}/mandeven" "$install_path"
    chmod +x "$install_path"

    info "installed to ${install_path}"

    configure_path "$MANDEVEN_INSTALL_DIR"

    info "done. Run \`mandeven\` to start."
}

# Make sure the install dir is reachable from `$PATH`. Skips silently
# when it is already in the current shell's PATH; otherwise picks a
# profile file based on $SHELL, appends an export line if it isn't
# already there, and prints a one-line reminder to reload the shell.
configure_path() {
    install_dir="$1"

    case ":$PATH:" in
        *":${install_dir}:"*)
            return 0
            ;;
    esac

    shell_name="$(basename "${SHELL:-/bin/zsh}")"
    case "$shell_name" in
        zsh)
            profile="$HOME/.zshrc"
            export_line="export PATH=\"${install_dir}:\$PATH\""
            ;;
        bash)
            if [ -f "$HOME/.bash_profile" ]; then
                profile="$HOME/.bash_profile"
            else
                profile="$HOME/.bashrc"
            fi
            export_line="export PATH=\"${install_dir}:\$PATH\""
            ;;
        fish)
            profile="$HOME/.config/fish/config.fish"
            export_line="set -gx PATH \"${install_dir}\" \$PATH"
            ;;
        *)
            info "unrecognized shell '${shell_name}'; add ${install_dir} to PATH manually."
            return 0
            ;;
    esac

    mkdir -p "$(dirname "$profile")"
    if [ -f "$profile" ] && grep -Fq "$install_dir" "$profile" 2>/dev/null; then
        info "${install_dir} already referenced in ${profile}; \`source ${profile}\` or open a new terminal."
        return 0
    fi

    {
        printf '\n# Added by mandeven install.sh\n'
        printf '%s\n' "$export_line"
    } >> "$profile"

    info "added ${install_dir} to PATH in ${profile}; run \`source ${profile}\` or open a new terminal."
}

main "$@"
