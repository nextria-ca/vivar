#!/bin/sh
# Installs vivar (and uv, which it drives today) on Linux and macOS.
#
#   curl -LsSf https://raw.githubusercontent.com/nextria-ca/vivar/main/install.sh | sh
#
# While the repo is private, fetch the script with gh instead:
#   gh api repos/nextria-ca/vivar/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
#
# Offline: extract a vivar-offline-<target>.tar.gz release bundle and run the copy of
# this script inside it. It installs the bundled vivar, uv and Python runtime without
# touching the network.
#
# Environment:
#   VIVAR_VERSION      release tag to install (default: latest)
#   VIVAR_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   VIVAR_SKIP_COMPLETIONS=1  don't install shell completions for $SHELL
#   VIVAR_ARCHIVE      local file or URL of a release archive to install instead
#   GITHUB_TOKEN       used for private-repo downloads when gh isn't logged in
#   VIVAR_SKIP_UV=1    don't install uv
#   VIVAR_POLICY_URL   org policy file to install as ~/.config/vivar/policy.toml
#                      (an offline bundle may carry one as policy.toml next to this script)
set -eu

REPO=nextria-ca/vivar
VERSION=${VIVAR_VERSION:-latest}
DIR=${VIVAR_INSTALL_DIR:-$HOME/.local/bin}
HERE=$(cd "$(dirname "$0")" 2>/dev/null && pwd)

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)            TARGET=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  Darwin-x86_64)           TARGET=x86_64-apple-darwin ;;
  Darwin-arm64)            TARGET=aarch64-apple-darwin ;;
  *) echo "vivar: unsupported platform $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
ASSET=vivar-$TARGET.tar.gz

mkdir -p "$DIR"
if [ -x "$HERE/vivar" ] && [ -x "$HERE/uv" ]; then
  echo "installing from offline bundle $HERE"
  install -m 755 "$HERE/vivar" "$DIR/vivar"
else
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  if [ -n "${VIVAR_ARCHIVE:-}" ]; then
    case "$VIVAR_ARCHIVE" in
      http://*|https://*) curl -LsSf "$VIVAR_ARCHIVE" -o "$TMP/$ASSET" ;;
      *) cp "$VIVAR_ARCHIVE" "$TMP/$ASSET" ;;
    esac
  elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    if [ "$VERSION" = latest ]; then
      gh release download --repo "$REPO" --pattern "$ASSET" --dir "$TMP"
    else
      gh release download "$VERSION" --repo "$REPO" --pattern "$ASSET" --dir "$TMP"
    fi
  else
    if [ "$VERSION" = latest ]; then API=https://api.github.com/repos/$REPO/releases/latest
    else API=https://api.github.com/repos/$REPO/releases/tags/$VERSION; fi
    AUTH=${GITHUB_TOKEN:+-H "Authorization: Bearer $GITHUB_TOKEN"}
    # shellcheck disable=SC2086
    URL=$(curl -sSf $AUTH "$API" | tr -d '\n' \
      | sed -n "s|.*\"url\": *\"\([^\"]*/releases/assets/[0-9]*\)\"[^}]*\"name\": *\"$ASSET\".*|\1|p")
    if [ -z "$URL" ]; then
      echo "vivar: could not find $ASSET in release $VERSION of $REPO (private repo? log in with gh or set GITHUB_TOKEN)" >&2
      exit 1
    fi
    # shellcheck disable=SC2086
    curl -LsSf $AUTH -H "Accept: application/octet-stream" "$URL" -o "$TMP/$ASSET"
  fi
  tar -xzf "$TMP/$ASSET" -C "$TMP"
  install -m 755 "$TMP/vivar-$TARGET/vivar" "$DIR/vivar"
fi
echo "installed $("$DIR/vivar" --version) to $DIR/vivar"

if [ -z "${VIVAR_SKIP_UV:-}" ] && ! command -v uv >/dev/null 2>&1 && [ ! -x "$HOME/.local/bin/uv" ]; then
  echo "installing uv (vivar drives it under the hood)"
  if [ -x "$HERE/uv" ]; then
    install -m 755 "$HERE/uv" "$DIR/uv"
  else
    curl -LsSf https://astral.sh/uv/install.sh | sh -s -- --quiet
  fi
fi

# Bundled Python runtimes go where uv keeps its managed ones, so `vivar init` finds them.
if [ -d "$HERE/python" ]; then
  UV=$(command -v uv 2>/dev/null || true)
  [ -x "${UV:-}" ] || UV=$DIR/uv
  [ -x "$UV" ] || UV=$HOME/.local/bin/uv
  PYDIR=$("$UV" python dir)
  mkdir -p "$PYDIR"
  for p in "$HERE"/python/*/; do
    name=$(basename "$p")
    if [ -d "$PYDIR/$name" ]; then
      echo "python $name already installed"
    else
      cp -R "${p%/}" "$PYDIR/$name"
      echo "installed python $name to $PYDIR"
    fi
  done
fi

# Org policy: from the bundle, or from a URL given by whoever rolls vivar out.
CONF=${XDG_CONFIG_HOME:-$HOME/.config}/vivar
if [ -f "$HERE/policy.toml" ] || [ -n "${VIVAR_POLICY_URL:-}" ]; then
  mkdir -p "$CONF"
  if [ -f "$CONF/policy.toml" ]; then
    echo "keeping existing $CONF/policy.toml"
  else
    # Fetched to a temp file first: a failed or partial download must not become the policy.
    if [ -f "$HERE/policy.toml" ]; then
      cp "$HERE/policy.toml" "$CONF/policy.toml.tmp"
    elif ! curl -LsSf "$VIVAR_POLICY_URL" -o "$CONF/policy.toml.tmp"; then
      rm -f "$CONF/policy.toml.tmp"
      echo "vivar: could not download the org policy from $VIVAR_POLICY_URL" >&2
      exit 1
    fi
    mv "$CONF/policy.toml.tmp" "$CONF/policy.toml"
    echo "installed org policy to $CONF/policy.toml"
  fi
fi

# Man pages, when the archive carries them.
if [ -d "$HERE/man" ]; then
  MANDIR=${XDG_DATA_HOME:-$HOME/.local/share}/man/man1
  mkdir -p "$MANDIR" && cp "$HERE"/man/*.1 "$MANDIR"/ && echo "installed man pages to $MANDIR"
fi

# Shell completions for the shell that runs this, into the directory that
# shell loads on its own (zsh needs the fpath line once).
if [ -z "${VIVAR_SKIP_COMPLETIONS:-}" ]; then
  case "$(basename "${SHELL:-sh}")" in
    bash)
      CDIR=${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions
      mkdir -p "$CDIR" && "$DIR/vivar" completions bash > "$CDIR/vivar" \
        && echo "installed bash completions to $CDIR/vivar" ;;
    zsh)
      CDIR=${ZDOTDIR:-$HOME}/.zfunc
      mkdir -p "$CDIR" && "$DIR/vivar" completions zsh > "$CDIR/_vivar" \
        && echo "installed zsh completions to $CDIR/_vivar (needs 'fpath+=$CDIR' before compinit in .zshrc)" ;;
    fish)
      CDIR=${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions
      mkdir -p "$CDIR" && "$DIR/vivar" completions fish > "$CDIR/vivar.fish" \
        && echo "installed fish completions to $CDIR/vivar.fish" ;;
  esac
fi

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "add $DIR to your PATH, e.g.:  export PATH=\"$DIR:\$PATH\"" ;;
esac
